use rmcp::schemars::JsonSchema;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;
use std::path::Path;

use crate::applier::{self, PathSource, ResolvedPath, resolve_path};
use crate::{parser, reader};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchExecParams {
    #[schemars(
        description = "Compact syntax patch text containing Read, Add, Update, Delete operations. Wrap in === begin / === end."
    )]
    pub patch: String,

    #[schemars(
        description = "Optional fuzzy matching threshold (0.0-1.0). Higher values (e.g., 0.97) require stricter matching. Default: 0.97."
    )]
    pub threshold: Option<f32>,
}

/// Metrics tracked during map operations.
#[derive(Debug, Clone, Default)]
pub struct MapMetrics {
    pub file_count: usize,
    pub total_lines: usize,
    pub max_depth: usize,
}

#[derive(Clone)]
pub struct WeavePatchServer {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl WeavePatchServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "patch__exec",
        description = "Unified workspace tool: read, map, add, update, and delete files atomically. Supports batch operations — read multiple files in a single call (prefer over repeated cat/head/tail).\nVersion: 0.0.11\n\nFORMAT: wrap everything in === begin / === end.\n\nEXAMPLES:\n\n1. Read a file:\n  === begin\n  read src/main.rs\n  === end\n\n2. Read with symbols:\n  === begin\n  read src/lib.rs symbols=Server,handle_request language=rust\n  === end\n\n3. Read with line range:\n  === begin\n  read config.py offset=10 limit=50\n  === end\n\n4. Map a directory:\n  === begin\n  map src/ depth=2\n  === end\n\n5. Add a new file (raw text, no +/- prefixes):\n  === begin\n  create src/hello.rs\n  pub fn hello() { println!(\"Hello!\"); }\n  === end\n\n6. Update a file (+/- for changes, preserve indentation):\n  === begin\n  update src/lib.rs\n  @@ fn main\n   fn main() {\n  -    old_code();\n  +    new_code();\n   }\n  === end\n\n7. Update with multiple hunks:\n  === begin\n  update src/lib.rs\n  @@ fn setup\n   fn setup() {\n  -    old_init();\n  +    new_init();\n   }\n  @@ fn teardown\n   fn teardown() {\n  -    old_cleanup();\n  +    new_cleanup();\n   }\n  === end\n\n8. Rename a file:\n  === begin\n  update src/old.rs\n  move_to src/new.rs\n  @@ fn foo\n   fn foo() { ... }\n  === end\n\n9. Delete a file:\n  === begin\n  delete src/old.rs\n  === end\n\n10. Combined operations:\n  === begin\n  read src/main.rs\n  update src/lib.rs\n  @@ fn main\n   fn main() {\n  -    old();\n  +    new();\n   }\n  create src/greet.rs\n  pub fn greet() { println!(\"hi\"); }\n  delete src/deprecated.rs\n  === end\n\n11. Read multiple files:\n  === begin\n  read src/main.rs\n  read src/lib.rs\n  read src/config.rs\n  === end\n\n12. Update multiple files:\n  === begin\n  update src/api.rs\n  @@ fn handle\n   fn handle() {\n  -    old();\n  +    new();\n   }\n  update src/db.rs\n  @@ fn connect\n   fn connect() {\n  -    let url = \"old\";\n  +    let url = \"new\";\n   }\n  === end\n\n13. Delete multiple files:\n  === begin\n  delete src/deprecated1.rs\n  delete src/deprecated2.rs\n  delete src/deprecated3.rs\n  === end\n\nOPTIONS:\n  read <path> [symbols=a,b] [language=rust] [offset=N] [limit=N]\n  map <path> [depth=N] [limit=N]  (default: depth=3, limit=6000)\n  update <path>   @@ hint (optional)   context/ -/ + lines\n  create <path>   content lines (raw text, no prefixes)\n  delete <path>   no body needed\n  move_to <path>  (inside Update, renames file)\n\nPARAMETERS:\n  patch: String (required) — Compact syntax patch text wrapped in === begin / === end.\n  threshold: Option<f32> (optional, default 0.97) — Fuzzy matching threshold for updates. Higher values (e.g., 0.99) require stricter matching.\n\nUPDATE RULES:\n  - Provide 2-3 unique context lines above and below each change\n  - Preserve exact indentation (whitespace matching is strict)\n  - Use - prefix for lines to remove, + prefix for lines to add\n\nLIMITS:\n  - Files over 1000 lines truncated unless symbols/limit used\n  - Binary files unsupported/ignored\n\nSECURITY: No path traversal (../), no symlinks. Absolute paths and ~ home expansion are allowed.\n\nVALIDATORS: rustfmt, gofmt, py_compile, json.tool, bash -n, node --check, terraform fmt (advisory)."
    )]
    async fn exec(
        &self,
        Parameters(params): Parameters<BatchExecParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if params.patch.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "patch is required. Wrap operations in === begin / === end.",
            )]));
        }

        let base_dir = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                let msg = format!("Cannot determine working directory: {e}");
                tracing::error!("{}", msg);
                return Ok(CallToolResult::error(vec![Content::text(msg)]));
            }
        };

        let mut output_parts: Vec<String> = Vec::new();

        let parse_result = match parser::parse_patch(&params.patch) {
            Ok(result) => result,
            Err(e) => {
                let msg = format!("Parse error: {e}");
                tracing::error!("{}", msg);
                output_parts.push(format!("PATCH ERROR: {}", msg));
                let combined = output_parts.join("\n---\n");
                return Ok(CallToolResult::error(vec![Content::text(combined)]));
            }
        };

        // Separate Read operations from write operations
        let (read_ops, write_ops): (Vec<_>, Vec<_>) = parse_result
            .ops
            .into_iter()
            .partition(|op| matches!(op, parser::FileOp::Read { .. } | parser::FileOp::Map { .. }));

        // Execute Read operations first (read-only, safe)
        if !read_ops.is_empty() {
            let canonical_base = match base_dir.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    let msg = format!("Cannot canonicalize working directory: {e}");
                    tracing::error!("{}", msg);
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            };

            const MAX_TOTAL: usize = 2 * 1024 * 1024;
            const MAX_FILE: usize = 512 * 1024;

            let mut read_results: Vec<String> = Vec::new();
            let mut total_bytes: usize = 0;

            for op in &read_ops {
                match op {
                    parser::FileOp::Map {
                        path,
                        depth,
                        output_limit,
                    } => {
                        let map_result = match self
                            .execute_map(&canonical_base, path, *depth, *output_limit)
                            .await
                        {
                            Ok(result) => result,
                            Err(e) => format!("Map error for '{}': {}", path, e),
                        };

                        let mut map_out = format!("--- Map: {} ---\n{}", path, map_result);

                        if let Some(limit) = output_limit
                            && map_out.len() > *limit
                        {
                            map_out.truncate(*limit);
                            map_out.push_str("\n... [truncated: output limit reached]");
                        }

                        if total_bytes + map_out.len() > MAX_TOTAL {
                            read_results.push(
                                "... [truncated: total output exceeds 2MB limit]".to_string(),
                            );
                            break;
                        }

                        total_bytes += map_out.len();
                        read_results.push(map_out);
                        continue;
                    }
                    parser::FileOp::Read {
                        path,
                        symbols,
                        language,
                        offset,
                        limit,
                    } => {
                        let (content, path_source) =
                            match self.read_single_file(&canonical_base, path).await {
                                Ok((c, ps)) => (c, ps),
                                Err(e) => {
                                    read_results
                                        .push(format!("--- {} [base] ---\nError: {}", path, e));
                                    continue;
                                }
                            };

                        // Apply default 1000-line truncation if no explicit limit/symbols
                        const DEFAULT_LINE_LIMIT: usize = 1000;
                        let effective_limit = if symbols.is_some() || limit.is_some() {
                            *limit // Use explicit limit or None
                        } else {
                            Some(DEFAULT_LINE_LIMIT) // Apply default truncation
                        };

                        let total_lines = content.lines().count();
                        let (formatted, header_suffix, truncated_notice) = if let Some(syms) =
                            symbols
                        {
                            if !syms.is_empty() {
                                let lang = language.clone().unwrap_or_else(|| infer_language(path));
                                let extracted = reader::extract_symbols(&content, &lang, syms);
                                (extracted, "[symbols]".to_string(), String::new())
                            } else {
                                let (sliced, start, end) =
                                    reader::apply_line_range(&content, *offset, effective_limit);
                                (
                                    sliced,
                                    format!("[lines {}-{} of {}]", start, end, total_lines),
                                    String::new(),
                                )
                            }
                        } else {
                            let (sliced, start, end) =
                                reader::apply_line_range(&content, *offset, effective_limit);
                            let notice = if limit.is_none()
                                && content.lines().count() > DEFAULT_LINE_LIMIT
                            {
                                format!(
                                    "\n... [truncated at {} lines - use limit or symbols for full file]",
                                    DEFAULT_LINE_LIMIT
                                )
                            } else {
                                String::new()
                            };
                            (
                                sliced,
                                format!("[lines {}-{} of {}]", start, end, total_lines),
                                notice,
                            )
                        };

                        let mut file_out = format!(
                            "--- {} [{}] {} ---\n{}{}",
                            path, path_source, header_suffix, formatted, truncated_notice
                        );

                        if file_out.len() > MAX_FILE {
                            file_out.truncate(MAX_FILE);
                            file_out.push_str("\n... [truncated: file exceeds 512KB limit]");
                        }

                        if total_bytes + file_out.len() > MAX_TOTAL {
                            read_results.push(
                                "... [truncated: total output exceeds 2MB limit]".to_string(),
                            );
                            break;
                        }

                        total_bytes += file_out.len();
                        read_results.push(file_out);
                    }
                    _ => continue, // Add/Delete/Update won't be in read_ops
                }
            }

            if !read_results.is_empty() {
                output_parts.push(read_results.join("\n\n"));
            }
        }

        // Execute Write operations (add/update/delete)
        let result = applier::weave_patch_with_threshold(
            write_ops,
            &base_dir,
            parse_result.threshold.or(params.threshold),
        );

        let has_error = result
            .operations
            .iter()
            .any(|op| op.status == OpStatus::FatalError || op.status == OpStatus::RecoverableError);

        let patch_output = if has_error {
            result
                .operations
                .iter()
                .filter(|op| {
                    op.status == OpStatus::FatalError || op.status == OpStatus::RecoverableError
                })
                .map(|op| {
                    let base = format!(
                        "[ERROR] {} \u{2014} {}: {}",
                        op.op_type, op.path, op.message
                    );
                    if let Some(ref llm_json) = op.llm_error {
                        format!("{}\n```json\n{}\n```", base, llm_json)
                    } else {
                        base
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            // Count operations by type for summary header
            let creates = result
                .operations
                .iter()
                .filter(|op| op.op_type == "add")
                .count();
            let updates = result
                .operations
                .iter()
                .filter(|op| op.op_type == "update")
                .count();
            let deletes = result
                .operations
                .iter()
                .filter(|op| op.op_type == "delete")
                .count();
            let total = result.operations.len();

            let mut lines: Vec<String> = Vec::new();

            // Prepend summary header for write operations
            if creates + updates + deletes > 0 {
                let mut summary = format!("✓ {} operations completed", total);
                let mut parts = Vec::new();
                if creates > 0 {
                    parts.push(format!("{} created", creates));
                }
                if updates > 0 {
                    parts.push(format!("{} updated", updates));
                }
                if deletes > 0 {
                    parts.push(format!("{} deleted", deletes));
                }
                if !parts.is_empty() {
                    summary.push_str(&format!(" ({})", parts.join(", ")));
                }
                lines.push(summary);
            }

            for op in &result.operations {
                let warn_suffix = if op.warnings.is_empty() {
                    String::new()
                } else {
                    format!(" [warnings: {}]", op.warnings.join("; "))
                };

                let batch_suffix = format_batch(&op.batch_index, &op.batch_total);
                let header = match op.op_type.as_str() {
                    "update" => {
                        if let Some((before, after)) = op.line_changes {
                            format!(
                                "✓ {} \u{2014} {} ({} \u{2192} {} lines){}{}",
                                op.op_type, op.path, before, after, warn_suffix, batch_suffix
                            )
                        } else {
                            format!(
                                "✓ {} \u{2014} {}{}{}",
                                op.op_type, op.path, warn_suffix, batch_suffix
                            )
                        }
                    }
                    "add" => {
                        if let Some((_, after)) = op.line_changes {
                            format!(
                                "✓ {} \u{2014} {} ({} lines){}{}",
                                op.op_type, op.path, after, warn_suffix, batch_suffix
                            )
                        } else {
                            format!(
                                "✓ {} \u{2014} {}{}{}",
                                op.op_type, op.path, warn_suffix, batch_suffix
                            )
                        }
                    }
                    "delete" => {
                        format!(
                            "✓ {} \u{2014} {} ({}){}{}",
                            op.op_type, op.path, op.message, warn_suffix, batch_suffix
                        )
                    }
                    _ => format!(
                        "✓ {} \u{2014} {}{}{}",
                        op.op_type, op.path, warn_suffix, batch_suffix
                    ),
                };

                lines.push(header);

                // Append match_info for updates
                if let Some(ref mi) = op.match_info
                    && op.op_type == "update"
                {
                    if mi.contains("; ") {
                        for (i, part) in mi.split("; ").enumerate() {
                            lines.push(format!("  Hunk {}: {}", i + 1, part.trim()));
                        }
                    } else {
                        lines.push(format!("  {mi}"));
                    }
                }

                // Append truncated diff preview (max 5 lines)
                if let Some(ref diff) = op.diff {
                    let diff_lines: Vec<&str> = diff.lines().collect();
                    for diff_line in diff_lines.iter().take(5) {
                        lines.push(format!("   {diff_line}"));
                    }
                    if diff_lines.len() > 5 {
                        let remaining = diff_lines.len() - 5;
                        lines.push(format!("   ... (+{remaining} more lines)"));
                    }
                }
            }
            if lines.is_empty() {
                "No write operations performed.".to_string()
            } else {
                lines.join("\n")
            }
        };

        tracing::info!("weave_patch result: {}", patch_output);

        if has_error {
            output_parts.push(patch_output);
            let combined = output_parts.join("\n---\n");
            return Ok(CallToolResult::error(vec![Content::text(combined)]));
        } else if !patch_output.is_empty() && patch_output != "No write operations performed." {
            output_parts.push(patch_output);
        }

        let combined = output_parts.join("\n---\n");
        Ok(CallToolResult::success(vec![Content::text(combined)]))
    }

    async fn execute_map(
        &self,
        base_dir: &std::path::Path,
        rel_path: &str,
        max_depth: Option<usize>,
        output_limit: Option<usize>,
    ) -> Result<String, String> {
        use tokio::time::{Duration, timeout};

        const DEFAULT_DEPTH: usize = 3;
        const DEFAULT_LIMIT: usize = 6000;

        let max_depth = max_depth.unwrap_or(DEFAULT_DEPTH);
        let output_limit = output_limit.unwrap_or(DEFAULT_LIMIT);
        let resolved: ResolvedPath = resolve_path(base_dir, rel_path);
        let full_path = resolved.full_path;
        let path_source = resolved.source;

        if !full_path.exists() {
            return Err("Directory not found".to_string());
        }

        if !full_path.is_dir() {
            return Err("Path is not a directory".to_string());
        }

        let map_result = timeout(Duration::from_secs(3), async {
            self.map_directory_with_metrics(&full_path, max_depth, output_limit, 0)
                .await
        })
        .await;

        match map_result {
            Ok(Ok((lines, metrics))) => {
                let header = format!(
                    "map: {} [{}] ({} files, {} lines total, depth={})",
                    rel_path,
                    path_source,
                    metrics.file_count,
                    metrics.total_lines,
                    metrics.max_depth
                );
                let mut output = vec![header];
                output.extend(lines);
                let result = output.join("\n");
                if result.len() > output_limit {
                    Ok(result[..output_limit].to_string() + "\n... [truncated]")
                } else {
                    Ok(result)
                }
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Map operation timed out after 3 seconds".to_string()),
        }
    }

    #[allow(clippy::type_complexity)]
    #[allow(clippy::only_used_in_recursion)]
    fn map_directory_with_metrics<'a>(
        &'a self,
        dir_path: &'a std::path::Path,
        max_depth: usize,
        _output_limit: usize,
        current_depth: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(Vec<String>, MapMetrics), String>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            use tokio::fs;

            if current_depth > max_depth {
                return Ok((vec![], MapMetrics::default()));
            }

            const SKIP_DIRS: &[&str] = &[
                "node_modules",
                ".git",
                "__pycache__",
                ".next",
                "dist",
                "build",
                "target",
                ".venv",
            ];
            const SKIP_EXTENSIONS: &[&str] = &[
                ".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".bin",
                ".lock", ".pdf", ".zip", ".tar", ".gz", ".svg", ".mp4", ".mp3", ".wav", ".webm",
            ];

            let dir_name = dir_path.file_name().and_then(|n| n.to_str()).unwrap_or(".");

            let mut lines: Vec<String> = Vec::new();
            let mut metrics = MapMetrics {
                max_depth: current_depth,
                ..Default::default()
            };
            let prefix = "  ".repeat(current_depth);

            let mut entries = match fs::read_dir(dir_path).await {
                Ok(entries) => entries,
                Err(e) => return Err(format!("Cannot read directory: {e}")),
            };

            let mut file_count = 0;
            let mut dir_entries: Vec<(String, bool, std::path::PathBuf)> = Vec::new();

            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);

                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }

                if is_dir {
                    dir_entries.push((name.clone(), true, path));
                } else {
                    let _ext = std::path::Path::new(&name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    if !SKIP_EXTENSIONS.iter().any(|skip| name.ends_with(skip)) {
                        dir_entries.push((name, false, path));
                        file_count += 1;
                    }
                }
            }

            dir_entries.sort_by(|a, b| match (a.1, b.1) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.0.cmp(&b.0),
            });

            if current_depth > 0 && current_depth <= 3 {
                match current_depth {
                    1 | 2 => lines.push(format!("{}{}/", prefix, dir_name)),
                    3 => lines.push(format!("{}{}/  ({} files)", prefix, dir_name, file_count)),
                    _ => {}
                }
            }

            for (_name, is_dir, path) in dir_entries {
                if is_dir {
                    let (sub_lines, sub_metrics) = self
                        .map_directory_with_metrics(
                            &path,
                            max_depth,
                            _output_limit,
                            current_depth + 1,
                        )
                        .await?;
                    lines.extend(sub_lines);
                    metrics.file_count += sub_metrics.file_count;
                    metrics.total_lines += sub_metrics.total_lines;
                    metrics.max_depth = metrics.max_depth.max(sub_metrics.max_depth);
                } else {
                    let (file_info, line_count) =
                        self.describe_file_with_lines(&path, current_depth).await?;
                    lines.push(file_info);
                    metrics.file_count += 1;
                    metrics.total_lines += line_count;
                }
            }

            Ok((lines, metrics))
        })
    }

    async fn describe_file_with_lines(
        &self,
        file_path: &std::path::Path,
        current_depth: usize,
    ) -> Result<(String, usize), String> {
        use tokio::fs;

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let prefix = "  ".repeat(current_depth + 1);

        // Read bytes for binary detection
        let bytes = match fs::read(file_path).await {
            Ok(b) => b,
            Err(_) => return Ok((format!("{}{}  [unreadable]", prefix, file_name), 0)),
        };

        // Check for null bytes in first 8KB (binary indicator)
        const CHECK_SIZE: usize = 8192;
        let check_len = std::cmp::min(bytes.len(), CHECK_SIZE);
        let is_binary = bytes[..check_len].contains(&0);

        if is_binary {
            let size_kb = bytes.len() / 1024;
            let size_display = if size_kb > 0 {
                format!("{} KB", size_kb)
            } else {
                format!("{} B", bytes.len())
            };
            return Ok((
                format!("{}{}  {} [binary]", prefix, file_name, size_display),
                0,
            ));
        }

        // Convert to string for text files
        let content = match String::from_utf8(bytes) {
            Ok(c) => c,
            Err(_) => return Ok((format!("{}{}  [decode error]", prefix, file_name), 0)),
        };

        let line_count = content.lines().count();

        if (1..=2).contains(&current_depth) {
            let func_ranges = extract_function_ranges(&content);
            if func_ranges.is_empty() {
                Ok((
                    format!("{}{}  {} LOC [text]", prefix, file_name, line_count),
                    line_count,
                ))
            } else {
                let func_list = func_ranges
                    .iter()
                    .map(|(name, start, end)| format!("{}[{}:{}]", name, start, end))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok((
                    format!(
                        "{}{}  {} LOC [text]  [{}]",
                        prefix, file_name, line_count, func_list
                    ),
                    line_count,
                ))
            }
        } else {
            Ok((
                format!("{}{}  {} LOC [text]", prefix, file_name, line_count),
                line_count,
            ))
        }
    }

    async fn read_single_file(
        &self,
        base_dir: &Path,
        rel_path: &str,
    ) -> Result<(String, PathSource), String> {
        let resolved: ResolvedPath = resolve_path(base_dir, rel_path);
        let full_path = resolved.full_path;
        let path_source = resolved.source;

        if !full_path.exists() {
            return Err("File not found".to_string());
        }

        let meta = tokio::fs::symlink_metadata(&full_path)
            .await
            .map_err(|e| format!("IO error: {e}"))?;

        if meta.file_type().is_symlink() {
            return Err("Symlink rejected".to_string());
        }

        if !meta.is_file() {
            return Err("Not a regular file".to_string());
        }

        // Binary detection: read bytes first
        let bytes = tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Read error: {e}"))?;

        // Check for null bytes in first 8KB (binary indicator)
        const CHECK_SIZE: usize = 8192;
        let check_len = std::cmp::min(bytes.len(), CHECK_SIZE);
        if bytes[..check_len].contains(&0) {
            return Ok((
                "[binary file - content not displayed]".to_string(),
                path_source,
            ));
        }

        // Convert to string, handling UTF-8 errors gracefully
        String::from_utf8(bytes)
            .map(|s| (s, path_source))
            .map_err(|e| format!("UTF-8 decode error: {e}"))
    }
}

impl Default for WeavePatchServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract function names with their line ranges from source code content.
/// Returns Vec of (name, start_line, end_line).
#[allow(clippy::needless_range_loop)]
#[allow(clippy::collapsible_if)]
#[allow(clippy::unnecessary_cast)]
fn extract_function_ranges(content: &str) -> Vec<(String, usize, usize)> {
    use regex::Regex;
    let patterns = [
        (r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)", "rust"),
        (r"^\s*function\s+(\w+)", "js"),
        (r"^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)", "js"),
        (r"^\s*(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s*)?\(", "js"),
        (r"^\s*(?:pub\s+)?class\s+(\w+)", "rust"),
        (r"^\s*def\s+(\w+)", "python"),
    ];

    let mut functions: Vec<(String, usize, usize)> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    for (pattern, _lang) in &patterns {
        let regex = match Regex::new(pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for (line_num, line) in lines.iter().enumerate() {
            if let Some(cap) = regex.captures(line) {
                if let Some(matched) = cap.get(1) {
                    let name = matched.as_str().to_string();
                    // Skip common false positives
                    if name == "if" || name == "while" || name == "for" || name == "match" {
                        continue;
                    }

                    let start_line = line_num + 1; // 1-based
                    let end_line = find_function_end(&lines, line_num);

                    // Check if we already have this function (from different pattern)
                    if !functions
                        .iter()
                        .any(|(n, s, _)| n == &name && *s == start_line)
                    {
                        functions.push((name, start_line, end_line));
                    }
                }
            }
        }
    }

    functions.sort_by(|a, b| a.1.cmp(&b.1));
    functions.truncate(5);
    functions
}

/// Find the end line of a function by tracking braces/indentation.
#[allow(clippy::needless_range_loop)]
#[allow(clippy::unnecessary_cast)]
fn find_function_end(lines: &[&str], start_idx: usize) -> usize {
    if start_idx >= lines.len() {
        return start_idx + 1;
    }

    let start_line = lines[start_idx];

    // Python: find next line with same or less indentation (or end of file)
    if start_line.trim_start().starts_with("def ") {
        let start_indent = start_line.len() - start_line.trim_start().len();
        for i in (start_idx + 1)..lines.len() {
            let line = lines[i];
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if indent <= start_indent
                && !trimmed.starts_with("else")
                && !trimmed.starts_with("elif")
                && !trimmed.starts_with("except")
                && !trimmed.starts_with("finally")
            {
                return i; // Function ends at this line (previous was last line of function)
            }
        }
        return lines.len();
    }

    // For brace-based languages, track brace balance
    let mut brace_count = 0i32;
    let mut in_string = false;
    let mut string_char: char = ' ';
    let mut escaped = false;

    for i in start_idx..lines.len() {
        let line = lines[i];

        for c in line.chars() {
            if escaped {
                escaped = false;
                continue;
            }

            if c == '\\' {
                escaped = true;
                continue;
            }

            if in_string {
                if c == string_char {
                    in_string = false;
                }
                continue;
            }

            if c == '"' || c == ('\'' as char) {
                in_string = true;
                string_char = c;
                continue;
            }

            // Skip single-line comments (simplified)
            if c == '/' && line.trim().starts_with('/') {
                break;
            }

            if c == '{' {
                brace_count += 1;
            } else if c == '}' {
                brace_count -= 1;
                if brace_count == 0 && i > start_idx {
                    return i + 1; // End line is 1-based
                }
            }
        }
    }

    // Fallback: return last line
    lines.len()
}

fn format_batch(batch_index: &Option<usize>, batch_total: &Option<usize>) -> String {
    if let (Some(idx), Some(total)) = (batch_index, batch_total) {
        format!(" [{}/{}]", idx, total)
    } else {
        String::new()
    }
}

fn infer_language(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "rs" => "rust".to_string(),
        "py" => "python".to_string(),
        "ts" | "tsx" => "typescript".to_string(),
        "js" | "jsx" | "mjs" | "cjs" => "javascript".to_string(),
        "go" => "go".to_string(),
        _ => "unknown".to_string(),
    }
}

#[tool_handler]
impl ServerHandler for WeavePatchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Structured file patching MCP server. One tool: patch__exec.\n\n\
             Unified format: Read, Add, Update, Delete files in === begin / === end.\n\n\
             Read: read  <path> [symbols=a,b] [language=rust] [offset=0] [limit=100]\n\n\
             Write: create/update/delete  <path>\n\n\
             Security: no path traversal (../), no symlinks. Absolute paths and ~ home expansion are allowed.\n\n\
             Validators: rustfmt, gofmt, py_compile, json.tool, bash -n, node --check, terraform fmt (advisory)."
        )
    }
}

pub async fn run() -> anyhow::Result<()> {
    let service = WeavePatchServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
use crate::applier::OpStatus;
