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

use crate::{applier, parser, reader};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchExecParams {
    #[schemars(
        description = "V4A patch text containing Read, Add, Update, Delete operations. Wrap in *** Begin Patch / *** End Patch."
    )]
    pub patch: String,
}

#[derive(Clone)]
pub struct ApplyPatchServer {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ApplyPatchServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "batch__exec",
        description = "Unified workspace tool: read, map, add, update, and delete files atomically.\nVersion: 1.0.4\n\nFORMAT: wrap everything in *** Begin Patch / *** End Patch.\n\nEXAMPLES:\n\n1. Read a file:\n  *** Begin Patch\n  *** Read File: src/main.rs\n  *** End Patch\n\n2. Read with symbols:\n  *** Begin Patch\n  *** Read File: src/lib.rs symbols=Server,handle_request language=rust\n  *** End Patch\n\n3. Read with line range:\n  *** Begin Patch\n  *** Read File: config.py offset=10 limit=50\n  *** End Patch\n\n4. Map a directory:\n  *** Begin Patch\n  *** Map Directory: src/ depth=2\n  *** End Patch\n\n5. Add a new file:\n  *** Begin Patch\n  *** Add File: src/hello.rs\n  +pub fn hello() { println!(\\\"Hello!\\\"); }\n  *** End Patch\n\n6. Update a file:\n  *** Begin Patch\n  *** Update File: src/lib.rs\n  @@ fn main\n   fn main() {\n  -    old_code();\n  +    new_code();\n   }\n  *** End Patch\n\n7. Update with multiple hunks:\n  *** Begin Patch\n  *** Update File: src/lib.rs\n  @@ fn setup\n   fn setup() {\n  -    old_init();\n  +    new_init();\n   }\n  @@ fn teardown\n   fn teardown() {\n  -    old_cleanup();\n  +    new_cleanup();\n   }\n  *** End Patch\n\n8. Rename a file:\n  *** Begin Patch\n  *** Update File: src/old.rs\n  *** Move to: src/new.rs\n  @@ fn foo\n   fn foo() { ... }\n  *** End Patch\n\n9. Delete a file:\n  *** Begin Patch\n  *** Delete File: src/old.rs\n  *** End Patch\n\n10. Combined operations:\n  *** Begin Patch\n  *** Read File: src/main.rs\n  *** Update File: src/lib.rs\n  @@ fn main\n   fn main() {\n  -    old();\n  +    new();\n   }\n  *** Add File: src/greet.rs\n  +pub fn greet() { println!(\\\"hi\\\"); }\n  *** Delete File: src/deprecated.rs\n  *** End Patch\n\nOPTIONS:\n  *** Read File: <path> [symbols=a,b] [language=rust] [offset=N] [limit=N]\n  *** Map Directory: <path> [depth=N] [limit=N]  (default: depth=3, limit=6000)\n  *** Update File: <path>   @@ hint (optional)   context/ -/ + lines\n  *** Add File: <path>      +content lines\n  *** Delete File: <path>   no body needed\n  *** Move to: <path>       (inside Update, renames file)\n\nSECURITY: No path traversal (../), no symlinks, no absolute paths.\nERRORS: JSON with structured diagnostics. ContextNotFound includes closest_matches.\nVALIDATORS (advisory): rustfmt, gofmt, py_compile, json.tool, bash -n, node --check"
    )]
    async fn exec(
        &self,
        Parameters(params): Parameters<BatchExecParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if params.patch.is_empty() {
            return Ok(CallToolResult::error(vec![Content::text(
                "patch is required. Wrap operations in *** Begin Patch / *** End Patch.",
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

        let ops = match parser::parse_patch(&params.patch) {
            Ok(ops) => ops,
            Err(e) => {
                let msg = format!("Parse error: {e}");
                tracing::error!("{}", msg);
                output_parts.push(format!("PATCH ERROR: {}", msg));
                let combined = output_parts.join("\n---\n");
                return Ok(CallToolResult::error(vec![Content::text(combined)]));
            }
        };

        // Separate Read operations from write operations
        let (read_ops, write_ops): (Vec<_>, Vec<_>) = ops
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
                        let content = match self.read_single_file(&canonical_base, path).await {
                            Ok(c) => c,
                            Err(e) => {
                                read_results.push(format!("--- {} ---\nError: {}", path, e));
                                continue;
                            }
                        };

                        let (formatted, header_suffix) = if let Some(syms) = symbols {
                            if !syms.is_empty() {
                                let lang = language.clone().unwrap_or_else(|| infer_language(path));
                                let extracted = reader::extract_symbols(&content, &lang, syms);
                                (extracted, "[symbols]".to_string())
                            } else {
                                let (sliced, start, end) =
                                    reader::apply_line_range(&content, *offset, *limit);
                                (sliced, format!("[lines {}-{}]", start, end))
                            }
                        } else {
                            let (sliced, start, end) =
                                reader::apply_line_range(&content, *offset, *limit);
                            (sliced, format!("[lines {}-{}]", start, end))
                        };

                        let mut file_out =
                            format!("--- {} {} ---\n{}", path, header_suffix, formatted);

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
        let result = applier::apply_patch(write_ops, &base_dir);

        let has_error = result.operations.iter().any(|op| op.status == "error");

        let patch_output = if has_error {
            result
                .operations
                .iter()
                .filter(|op| op.status == "error")
                .map(|op| {
                    format!(
                        "[ERROR] {} \u{2014} {}: {}",
                        op.op_type,
                        op.path,
                        op.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            let mut lines: Vec<String> = Vec::new();
            for op in &result.operations {
                let warn_suffix = if op.warnings.is_empty() {
                    String::new()
                } else {
                    format!(" [warnings: {}]", op.warnings.join("; "))
                };

                let header = match op.op_type.as_str() {
                    "update" => {
                        if let Some((before, after)) = op.line_changes {
                            format!(
                                "[成功] {} \u{2014} {} ({} \u{2192} {} lines){}",
                                op.op_type, op.path, before, after, warn_suffix
                            )
                        } else {
                            format!("[成功] {} \u{2014} {}{}", op.op_type, op.path, warn_suffix)
                        }
                    }
                    "add" => {
                        if let Some((_, after)) = op.line_changes {
                            format!(
                                "[成功] {} \u{2014} {} ({} lines){}",
                                op.op_type, op.path, after, warn_suffix
                            )
                        } else {
                            format!("[成功] {} \u{2014} {}{}", op.op_type, op.path, warn_suffix)
                        }
                    }
                    "delete" => {
                        format!(
                            "[成功] {} \u{2014} {} ({}){}",
                            op.op_type, op.path, op.message, warn_suffix
                        )
                    }
                    _ => format!("[成功] {} \u{2014} {}{}", op.op_type, op.path, warn_suffix),
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

        tracing::info!("apply_patch result: {}", patch_output);

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

        let full_path = applier::validate_path(base_dir, rel_path)
            .map_err(|e| format!("Validation error: {e}"))?;

        if !full_path.exists() {
            return Err("Directory not found".to_string());
        }

        if !full_path.is_dir() {
            return Err("Path is not a directory".to_string());
        }

        let map_result = timeout(Duration::from_secs(3), async {
            self.map_directory(&full_path, max_depth, output_limit, 0)
                .await
        })
        .await;

        match map_result {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Map operation timed out after 3 seconds".to_string()),
        }
    }

    fn map_directory<'a>(
        &'a self,
        dir_path: &'a std::path::Path,
        max_depth: usize,
        output_limit: usize,
        current_depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            use tokio::fs;

            if current_depth > max_depth {
                return Ok(String::new());
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
                    let subresult = self
                        .map_directory(&path, max_depth, output_limit, current_depth + 1)
                        .await?;
                    if !subresult.is_empty() {
                        lines.push(subresult);
                    }
                } else {
                    let file_info = self.describe_file(&path, current_depth).await?;
                    lines.push(file_info);
                }
            }

            let result = lines.join("\n");
            if result.len() > output_limit {
                Ok(result[..output_limit].to_string() + "\n... [truncated]")
            } else {
                Ok(result)
            }
        })
    }

    async fn describe_file(
        &self,
        file_path: &std::path::Path,
        current_depth: usize,
    ) -> Result<String, String> {
        use tokio::fs;

        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let prefix = "  ".repeat(current_depth + 1);

        let content = match fs::read_to_string(file_path).await {
            Ok(c) => c,
            Err(_) => return Ok(format!("{}{}  [binary]", prefix, file_name)),
        };

        let line_count = content.lines().count();

        if (1..=2).contains(&current_depth) {
            let func_ranges = extract_function_ranges(&content);
            if func_ranges.is_empty() {
                Ok(format!("{}{}  {} LOC", prefix, file_name, line_count))
            } else {
                let func_list = func_ranges
                    .iter()
                    .map(|(name, start, end)| format!("{}[{}:{}]", name, start, end))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!(
                    "{}{}  {} LOC  [{}]",
                    prefix, file_name, line_count, func_list
                ))
            }
        } else {
            Ok(format!("{}{}  {} LOC", prefix, file_name, line_count))
        }
    }

    async fn read_single_file(&self, base_dir: &Path, rel_path: &str) -> Result<String, String> {
        let full_path = applier::validate_path(base_dir, rel_path)
            .map_err(|e| format!("Validation error: {e}"))?;

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

        tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| format!("Read error: {e}"))
    }
}

impl Default for ApplyPatchServer {
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

    // Pattern to find function/class/definition starts
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

    // Sort by start line
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
impl ServerHandler for ApplyPatchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Structured file patching MCP server. One tool: batch__exec.\n\n\
             Unified format: Read, Add, Update, Delete files in *** Begin Patch / *** End Patch.\n\n\
             Read: *** Read File: <path> [symbols=a,b] [language=rust] [offset=0] [limit=100]\n\n\
             Write: *** Add/Update/Delete File: <path>\n\n\
             Security: no path traversal, no symlinks.\n\n\
             Validators: rustfmt, gofmt, py_compile, json.tool, bash -n, node --check, terraform fmt (advisory)."
        )
    }
}

pub async fn run() -> anyhow::Result<()> {
    let service = ApplyPatchServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
