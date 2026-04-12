use rmcp::schemars::JsonSchema;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::applier::{
    self, OpResult, OpStatus, PatchSession, VirtualPathKind, make_error_op, make_op,
    with_path_source,
};
use crate::error::PatchError;
use crate::{parser, reader, tool_contract};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchExecParams {
    #[schemars(
        description = "Compact patch text containing view/read, map, create, write, update, move, and delete operations. Use weave syntax wrapped in === begin / === end, or paste native apply_patch-style *** Begin Patch blocks."
    )]
    pub patch: String,

    #[schemars(
        description = "Optional fuzzy matching threshold (0.0-1.0). Higher values (e.g., 0.97) require stricter matching. Default: 0.95."
    )]
    pub threshold: Option<f32>,

    #[schemars(
        description = "When true, preview the batch against staged state without committing filesystem changes."
    )]
    #[serde(default)]
    pub dry_run: bool,

    #[schemars(
        description = "Response format. Use 'text' for the human-readable summary (default) or 'json' for a machine-readable JSON payload in the tool text response."
    )]
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
}

/// Metrics tracked during map operations.
#[derive(Debug, Clone, Default)]
pub struct MapMetrics {
    pub file_count: usize,
    pub total_lines: usize,
    pub max_depth: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Serialize)]
struct ExecutionSummary {
    total_operations: usize,
    ok_operations: usize,
    skipped_operations: usize,
    error_operations: usize,
    add_operations: usize,
    write_operations: usize,
    update_operations: usize,
    delete_operations: usize,
    move_operations: usize,
    read_operations: usize,
    map_operations: usize,
}

#[derive(Debug, Serialize)]
struct ExecutionReport {
    ok: bool,
    dry_run: bool,
    committed: bool,
    summary: ExecutionSummary,
    operations: Vec<OpResult>,
}

struct ReadRequest<'a> {
    path: &'a str,
    symbols: Option<&'a [String]>,
    language: Option<&'a str>,
    offset: Option<usize>,
    limit: Option<usize>,
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

    #[doc = include_str!("patch_exec_description.txt")]
    #[tool(name = "patch__exec")]
    async fn exec(
        &self,
        Parameters(params): Parameters<BatchExecParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let response_format = params.response_format.unwrap_or_default();

        if params.patch.is_empty() {
            let payload = self.render_error_payload(
                "patch is required. Wrap operations in === begin / === end.",
                params.dry_run,
                response_format,
            );
            return Ok(CallToolResult::error(vec![Content::text(payload)]));
        }

        let base_dir = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                let msg = format!("Cannot determine working directory: {e}");
                tracing::error!("{}", msg);
                let payload = self.render_error_payload(&msg, params.dry_run, response_format);
                return Ok(CallToolResult::error(vec![Content::text(payload)]));
            }
        };

        let report = match self.execute_batch_at(&base_dir, &params).await {
            Ok(report) => report,
            Err(msg) => {
                tracing::error!("{}", msg);
                let payload = self.render_error_payload(&msg, params.dry_run, response_format);
                return Ok(CallToolResult::error(vec![Content::text(payload)]));
            }
        };

        let payload = self.render_report_payload(&report, response_format);
        tracing::info!("weave_patch result: {}", payload);

        if report.ok {
            Ok(CallToolResult::success(vec![Content::text(payload)]))
        } else {
            Ok(CallToolResult::error(vec![Content::text(payload)]))
        }
    }

    async fn execute_batch_at(
        &self,
        base_dir: &Path,
        params: &BatchExecParams,
    ) -> Result<ExecutionReport, String> {
        let parse_result =
            parser::parse_patch(&params.patch).map_err(|e| format!("Parse error: {e}"))?;
        let threshold = parse_result.threshold.or(params.threshold);
        let mut session = PatchSession::new(base_dir, threshold);
        let total = parse_result.ops.len();
        let mut operations: Vec<OpResult> = Vec::with_capacity(total);
        let mut rollback_msg: Option<String> = None;
        let mut total_output_bytes = 0usize;
        let mut output_limit_hit = false;

        for (idx, op) in parse_result.ops.into_iter().enumerate() {
            let batch = Some((idx + 1, total));
            if let Some(ref message) = rollback_msg {
                operations.push(self.skipped_result(&op, message, batch));
                continue;
            }

            let mut result = match op {
                parser::FileOp::Read {
                    path,
                    symbols,
                    language,
                    offset,
                    limit,
                } => self.perform_read(
                    &session,
                    ReadRequest {
                        path: &path,
                        symbols: symbols.as_deref(),
                        language: language.as_deref(),
                        offset,
                        limit,
                    },
                    batch,
                ),
                parser::FileOp::Map {
                    path,
                    depth,
                    output_limit,
                } => {
                    self.perform_map(&session, &path, depth, output_limit, batch)
                        .await
                }
                other => session.stage_op(other, batch),
            };

            self.apply_output_limits(&mut result, &mut total_output_bytes, &mut output_limit_hit);

            if is_mutating_result(&result)
                && matches!(
                    result.status,
                    OpStatus::FatalError | OpStatus::RecoverableError
                )
            {
                rollback_msg = Some(format!(
                    "Batch rolled back due to failure at op {}/{}: {}",
                    result.batch_index.unwrap_or(0),
                    result.batch_total.unwrap_or(0),
                    result.message
                ));
            }

            operations.push(result);
        }

        let mut committed = false;
        if let Some(message) = rollback_msg {
            for op in &mut operations {
                if is_mutating_result(op) && op.status == OpStatus::Ok {
                    op.status = OpStatus::Skipped;
                    op.rollback_reason = Some(message.clone());
                }
            }
        } else if params.dry_run {
            let note = "Dry run: preview only; no filesystem changes committed".to_string();
            for op in &mut operations {
                if is_mutating_result(op) && op.status == OpStatus::Ok {
                    op.rollback_reason = Some(note.clone());
                }
            }
        } else if let Err(e) = session.commit() {
            let message = format!("Commit failed: {e}");
            for op in &mut operations {
                if is_mutating_result(op) && op.status == OpStatus::Ok {
                    op.status = OpStatus::FatalError;
                    op.message = message.clone();
                    op.rollback_reason = Some(message.clone());
                }
            }
        } else {
            committed = true;
        }

        let ok = !operations
            .iter()
            .any(|op| matches!(op.status, OpStatus::FatalError | OpStatus::RecoverableError));

        Ok(ExecutionReport {
            ok,
            dry_run: params.dry_run,
            committed,
            summary: build_summary(&operations),
            operations,
        })
    }

    fn perform_read(
        &self,
        session: &PatchSession,
        request: ReadRequest<'_>,
        batch: Option<(usize, usize)>,
    ) -> OpResult {
        let path = request.path;
        let symbols = request.symbols;
        let language = request.language;
        let offset = request.offset;
        let limit = request.limit;
        let resolved = session.resolve_path(path);
        let read_result = session.read_virtual_file(path);

        match read_result {
            Ok(virtual_read) => {
                let bytes = virtual_read.bytes;
                let check_len = std::cmp::min(bytes.len(), 8192);
                let is_binary = bytes[..check_len].contains(&0);

                let (formatted, header_suffix, truncated_notice) = if is_binary {
                    (
                        "[binary file - content not displayed]".to_string(),
                        "[binary]".to_string(),
                        String::new(),
                    )
                } else {
                    let content = match String::from_utf8(bytes) {
                        Ok(content) => content,
                        Err(e) => {
                            return with_path_source(
                                make_error_op(
                                    path,
                                    "read",
                                    &PatchError::Io(std::io::Error::other(format!(
                                        "UTF-8 decode error: {e}"
                                    ))),
                                    batch,
                                ),
                                resolved.source,
                            );
                        }
                    };

                    let effective_limit = if symbols.is_some() || limit.is_some() {
                        limit
                    } else {
                        Some(tool_contract::DEFAULT_READ_LINE_LIMIT)
                    };
                    let total_lines = content.lines().count();

                    if let Some(symbol_list) = symbols {
                        if !symbol_list.is_empty() {
                            let lang = language
                                .map(str::to_string)
                                .unwrap_or_else(|| infer_language(path));
                            (
                                reader::extract_symbols(&content, &lang, symbol_list),
                                "[symbols]".to_string(),
                                String::new(),
                            )
                        } else {
                            let (sliced, start, end) =
                                reader::apply_line_range(&content, offset, effective_limit);
                            (
                                sliced,
                                format!("[lines {}-{} of {}]", start, end, total_lines),
                                String::new(),
                            )
                        }
                    } else {
                        let (sliced, start, end) =
                            reader::apply_line_range(&content, offset, effective_limit);
                        let notice = if limit.is_none()
                            && content.lines().count() > tool_contract::DEFAULT_READ_LINE_LIMIT
                        {
                            format!(
                                "\n... [truncated at {} lines - use limit or symbols for full file]",
                                tool_contract::DEFAULT_READ_LINE_LIMIT
                            )
                        } else {
                            String::new()
                        };
                        (
                            sliced,
                            format!("[lines {}-{} of {}]", start, end, total_lines),
                            notice,
                        )
                    }
                };

                let staged_suffix = if virtual_read.staged { " [staged]" } else { "" };
                let output = format!(
                    "--- {} [{}]{} {} ---\n{}{}",
                    path,
                    virtual_read.path_source,
                    staged_suffix,
                    header_suffix,
                    formatted,
                    truncated_notice
                );
                let message = if virtual_read.staged {
                    format!("read: {} (staged)", path)
                } else {
                    format!("read: {} (ok)", path)
                };
                let mut op = with_path_source(
                    make_op(path, "read", OpStatus::Ok, &message, batch),
                    virtual_read.path_source,
                );
                op.output = Some(output);
                op
            }
            Err(e) => with_path_source(make_error_op(path, "read", &e, batch), resolved.source),
        }
    }

    async fn perform_map(
        &self,
        session: &PatchSession,
        path: &str,
        depth: Option<usize>,
        output_limit: Option<usize>,
        batch: Option<(usize, usize)>,
    ) -> OpResult {
        let resolved = session.resolve_path(path);
        match self.execute_map(session, path, depth, output_limit).await {
            Ok(result) => {
                let mut op = with_path_source(
                    make_op(
                        path,
                        "map",
                        OpStatus::Ok,
                        &format!("map: {} (ok)", path),
                        batch,
                    ),
                    resolved.source,
                );
                op.output = Some(format!("--- Map: {} ---\n{}", path, result));
                op
            }
            Err(message) => {
                let error = match session.resolve_virtual_kind(path) {
                    Ok((resolved_path, VirtualPathKind::Missing)) => applier::file_not_found_error(
                        path,
                        &resolved_path.full_path,
                        resolved_path.source,
                    ),
                    Ok((_, VirtualPathKind::File)) => {
                        PatchError::Io(std::io::Error::other("Path is not a directory"))
                    }
                    Ok((_, VirtualPathKind::Directory)) => {
                        PatchError::Io(std::io::Error::other(message))
                    }
                    Err(e) => e,
                };
                with_path_source(make_error_op(path, "map", &error, batch), resolved.source)
            }
        }
    }

    fn skipped_result(
        &self,
        op: &parser::FileOp,
        message: &str,
        batch: Option<(usize, usize)>,
    ) -> OpResult {
        let (op_type, path) = match op {
            parser::FileOp::Add { path, .. } => ("add", path.as_str()),
            parser::FileOp::Write { path, .. } => ("write", path.as_str()),
            parser::FileOp::Delete { path } => ("delete", path.as_str()),
            parser::FileOp::Update { path, .. } => ("update", path.as_str()),
            parser::FileOp::Read { path, .. } => ("read", path.as_str()),
            parser::FileOp::Map { path, .. } => ("map", path.as_str()),
            parser::FileOp::Move { from, .. } => ("move", from.as_str()),
        };
        let mut result = make_op(
            path,
            op_type,
            OpStatus::Skipped,
            "Skipped due to earlier batch failure",
            batch,
        );
        result.rollback_reason = Some(message.to_string());
        result
    }

    fn apply_output_limits(
        &self,
        op: &mut OpResult,
        total_output_bytes: &mut usize,
        output_limit_hit: &mut bool,
    ) {
        const MAX_TOTAL_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
        const MAX_FILE_OUTPUT_BYTES: usize = 512 * 1024;

        let Some(output) = op.output.as_mut() else {
            return;
        };

        if output.len() > MAX_FILE_OUTPUT_BYTES {
            output.truncate(MAX_FILE_OUTPUT_BYTES);
            output.push_str("\n... [truncated: file exceeds 512KB limit]");
        }

        if *output_limit_hit {
            op.output = None;
            return;
        }

        if *total_output_bytes + output.len() > MAX_TOTAL_OUTPUT_BYTES {
            *output = "... [truncated: total output exceeds 2MB limit]".to_string();
            *output_limit_hit = true;
            *total_output_bytes = MAX_TOTAL_OUTPUT_BYTES;
            return;
        }

        *total_output_bytes += output.len();
    }

    fn render_report_payload(
        &self,
        report: &ExecutionReport,
        response_format: ResponseFormat,
    ) -> String {
        match response_format {
            ResponseFormat::Text => self.render_text_report(report),
            ResponseFormat::Json => serde_json::to_string_pretty(report).unwrap_or_else(|e| {
                format!("{{\"ok\":false,\"error\":\"json serialization failed: {e}\"}}")
            }),
        }
    }

    fn render_error_payload(
        &self,
        message: &str,
        dry_run: bool,
        response_format: ResponseFormat,
    ) -> String {
        match response_format {
            ResponseFormat::Text => format!("PATCH ERROR: {message}"),
            ResponseFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
                "ok": false,
                "dry_run": dry_run,
                "committed": false,
                "error": { "message": message },
            }))
            .unwrap_or_else(|_| {
                format!("{{\"ok\":false,\"error\":{{\"message\":\"{message}\"}}}}")
            }),
        }
    }

    fn render_text_report(&self, report: &ExecutionReport) -> String {
        let mut lines = Vec::new();

        if report.dry_run && report.summary.total_operations > 0 {
            lines.push("DRY RUN: preview only; no filesystem changes were committed.".to_string());
        }

        if report.summary.total_operations > 0 {
            lines.push(summary_line(&report.summary, report.ok));
        }

        for op in &report.operations {
            if let Some(section) = format_operation_text(op) {
                lines.push(section);
            }
        }

        if lines.is_empty() {
            "No operations performed.".to_string()
        } else {
            lines.join("\n")
        }
    }

    async fn execute_map(
        &self,
        session: &PatchSession,
        rel_path: &str,
        max_depth: Option<usize>,
        output_limit: Option<usize>,
    ) -> Result<String, String> {
        use tokio::time::{Duration, timeout};

        let max_depth = max_depth.unwrap_or(tool_contract::DEFAULT_MAP_DEPTH);
        let output_limit = output_limit.unwrap_or(tool_contract::DEFAULT_MAP_OUTPUT_LIMIT);
        let (resolved, kind) = session
            .resolve_virtual_kind(rel_path)
            .map_err(|e| e.to_string())?;
        let full_path = resolved.full_path;
        let path_source = resolved.source;

        match kind {
            VirtualPathKind::Missing => {
                return Err(
                    applier::file_not_found_error(rel_path, &full_path, path_source).to_string(),
                );
            }
            VirtualPathKind::File => return Err("Path is not a directory".to_string()),
            VirtualPathKind::Directory => {}
        }

        let map_result = timeout(Duration::from_secs(3), async {
            self.map_directory_with_metrics(session, &full_path, max_depth, 0)
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
    fn map_directory_with_metrics(
        &self,
        session: &PatchSession,
        dir_path: &std::path::Path,
        max_depth: usize,
        current_depth: usize,
    ) -> Result<(Vec<String>, MapMetrics), String> {
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
            ".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".bin", ".lock",
            ".pdf", ".zip", ".tar", ".gz", ".svg", ".mp4", ".mp3", ".wav", ".webm",
        ];

        let dir_name = dir_path.file_name().and_then(|n| n.to_str()).unwrap_or(".");
        let mut lines: Vec<String> = Vec::new();
        let mut metrics = MapMetrics {
            max_depth: current_depth,
            ..Default::default()
        };
        let prefix = "  ".repeat(current_depth);
        let entries = session
            .list_virtual_dir(dir_path)
            .map_err(|e| format!("Cannot read directory: {e}"))?;

        let mut file_count = 0;
        let mut dir_entries: Vec<(String, bool, std::path::PathBuf)> = Vec::new();

        for entry in entries {
            if SKIP_DIRS.contains(&entry.name.as_str()) {
                continue;
            }

            if entry.is_dir {
                dir_entries.push((entry.name.clone(), true, entry.path));
            } else if !SKIP_EXTENSIONS
                .iter()
                .any(|skip| entry.name.ends_with(skip))
            {
                dir_entries.push((entry.name, false, entry.path));
                file_count += 1;
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
                let (sub_lines, sub_metrics) =
                    self.map_directory_with_metrics(session, &path, max_depth, current_depth + 1)?;
                lines.extend(sub_lines);
                metrics.file_count += sub_metrics.file_count;
                metrics.total_lines += sub_metrics.total_lines;
                metrics.max_depth = metrics.max_depth.max(sub_metrics.max_depth);
            } else {
                let (file_info, line_count) =
                    self.describe_file_with_lines(session, &path, current_depth)?;
                lines.push(file_info);
                metrics.file_count += 1;
                metrics.total_lines += line_count;
            }
        }

        Ok((lines, metrics))
    }

    fn describe_file_with_lines(
        &self,
        session: &PatchSession,
        file_path: &std::path::Path,
        current_depth: usize,
    ) -> Result<(String, usize), String> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let prefix = "  ".repeat(current_depth + 1);

        // Read bytes for binary detection
        let bytes = match session.read_virtual_bytes_at(file_path) {
            Ok(Some((bytes, _))) => bytes,
            Err(_) => return Ok((format!("{}{}  [unreadable]", prefix, file_name), 0)),
            Ok(None) => return Ok((format!("{}{}  [unreadable]", prefix, file_name), 0)),
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

fn is_mutating_result(op: &OpResult) -> bool {
    matches!(
        op.op_type.as_str(),
        "add" | "write" | "update" | "delete" | "move"
    )
}

fn build_summary(operations: &[OpResult]) -> ExecutionSummary {
    ExecutionSummary {
        total_operations: operations.len(),
        ok_operations: operations
            .iter()
            .filter(|op| op.status == OpStatus::Ok)
            .count(),
        skipped_operations: operations
            .iter()
            .filter(|op| op.status == OpStatus::Skipped)
            .count(),
        error_operations: operations
            .iter()
            .filter(|op| matches!(op.status, OpStatus::FatalError | OpStatus::RecoverableError))
            .count(),
        add_operations: operations.iter().filter(|op| op.op_type == "add").count(),
        write_operations: operations.iter().filter(|op| op.op_type == "write").count(),
        update_operations: operations
            .iter()
            .filter(|op| op.op_type == "update")
            .count(),
        delete_operations: operations
            .iter()
            .filter(|op| op.op_type == "delete")
            .count(),
        move_operations: operations.iter().filter(|op| op.op_type == "move").count(),
        read_operations: operations.iter().filter(|op| op.op_type == "read").count(),
        map_operations: operations.iter().filter(|op| op.op_type == "map").count(),
    }
}

fn summary_line(summary: &ExecutionSummary, ok: bool) -> String {
    let mut parts = Vec::new();

    if summary.add_operations > 0 {
        parts.push(format!("{} created", summary.add_operations));
    }
    if summary.write_operations > 0 {
        parts.push(format!("{} written", summary.write_operations));
    }
    if summary.update_operations > 0 {
        parts.push(format!("{} updated", summary.update_operations));
    }
    if summary.delete_operations > 0 {
        parts.push(format!("{} deleted", summary.delete_operations));
    }
    if summary.move_operations > 0 {
        parts.push(format!("{} moved", summary.move_operations));
    }
    if summary.read_operations > 0 {
        parts.push(format!("{} read", summary.read_operations));
    }
    if summary.map_operations > 0 {
        parts.push(format!("{} mapped", summary.map_operations));
    }

    let prefix = if ok { "✓" } else { "!" };
    if parts.is_empty() {
        format!("{prefix} {} operations processed", summary.total_operations)
    } else {
        format!(
            "{prefix} {} operations processed ({})",
            summary.total_operations,
            parts.join(", ")
        )
    }
}

fn format_operation_text(op: &OpResult) -> Option<String> {
    if matches!(op.status, OpStatus::FatalError | OpStatus::RecoverableError) {
        let base = format!("[ERROR] {} — {}: {}", op.op_type, op.path, op.message);
        return Some(if let Some(ref llm_json) = op.llm_error {
            format!("{}\n```json\n{}\n```", base, llm_json)
        } else {
            base
        });
    }

    if let Some(ref output) = op.output {
        return Some(output.clone());
    }

    let prefix = match op.status {
        OpStatus::Skipped => "○",
        OpStatus::ValidationWarning => "!",
        _ => "✓",
    };

    let warn_suffix = if op.warnings.is_empty() {
        String::new()
    } else {
        format!(" [warnings: {}]", op.warnings.join("; "))
    };
    let batch_suffix = format_batch(&op.batch_index, &op.batch_total);
    let mut lines = Vec::new();

    let header = match op.op_type.as_str() {
        "update" | "write" => {
            if let Some((before, after)) = op.line_changes {
                format!(
                    "{} {} — {} ({} → {} lines){}{}",
                    prefix, op.op_type, op.path, before, after, warn_suffix, batch_suffix
                )
            } else {
                format!(
                    "{} {} — {}{}{}",
                    prefix, op.op_type, op.path, warn_suffix, batch_suffix
                )
            }
        }
        "add" => {
            if let Some((_, after)) = op.line_changes {
                format!(
                    "{} {} — {} ({} lines){}{}",
                    prefix, op.op_type, op.path, after, warn_suffix, batch_suffix
                )
            } else {
                format!(
                    "{} {} — {}{}{}",
                    prefix, op.op_type, op.path, warn_suffix, batch_suffix
                )
            }
        }
        "delete" => format!(
            "{} {} — {} ({}){}{}",
            prefix, op.op_type, op.path, op.message, warn_suffix, batch_suffix
        ),
        _ => format!(
            "{} {} — {}{}{}",
            prefix, op.op_type, op.path, warn_suffix, batch_suffix
        ),
    };

    lines.push(header);

    if let Some(ref rollback_reason) = op.rollback_reason {
        lines.push(format!("  {rollback_reason}"));
    }

    if let Some(ref match_info) = op.match_info
        && op.op_type == "update"
    {
        if match_info.contains("; ") {
            for (idx, part) in match_info.split("; ").enumerate() {
                lines.push(format!("  Hunk {}: {}", idx + 1, part.trim()));
            }
        } else {
            lines.push(format!("  {match_info}"));
        }
    }

    if let Some(ref diff) = op.diff {
        let diff_lines: Vec<&str> = diff.lines().collect();
        for diff_line in diff_lines.iter().take(5) {
            lines.push(format!("   {diff_line}"));
        }
        if diff_lines.len() > 5 {
            lines.push(format!("   ... (+{} more lines)", diff_lines.len() - 5));
        }
    }

    Some(lines.join("\n"))
}

#[tool_handler]
impl ServerHandler for WeavePatchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(tool_contract::server_instructions())
    }
}

pub async fn run() -> anyhow::Result<()> {
    let service = WeavePatchServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn params(patch: &str) -> BatchExecParams {
        BatchExecParams {
            patch: patch.to_string(),
            threshold: None,
            dry_run: false,
            response_format: None,
        }
    }

    #[tokio::test]
    async fn write_then_read_sees_staged_content_and_commits() {
        let dir = tmp();
        fs::write(dir.path().join("demo.txt"), "old\n").unwrap();
        let server = WeavePatchServer::new();
        let params = params("=== begin\nwrite demo.txt\nnew\nread demo.txt\n=== end");

        let report = server.execute_batch_at(dir.path(), &params).await.unwrap();

        assert!(report.ok);
        assert!(report.committed);
        assert_eq!(report.summary.write_operations, 1);
        assert_eq!(report.summary.read_operations, 1);
        assert_eq!(report.operations[0].status, OpStatus::Ok);
        assert_eq!(report.operations[1].status, OpStatus::Ok);
        assert!(
            report.operations[1]
                .output
                .as_deref()
                .unwrap()
                .contains("[staged]")
        );
        assert!(
            report.operations[1]
                .output
                .as_deref()
                .unwrap()
                .contains("\nnew")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("demo.txt")).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn dry_run_preserves_filesystem_but_reads_staged_content() {
        let dir = tmp();
        fs::write(dir.path().join("preview.txt"), "old\n").unwrap();
        let server = WeavePatchServer::new();
        let mut params = params("=== begin\nwrite preview.txt\npreview\nread preview.txt\n=== end");
        params.dry_run = true;

        let report = server.execute_batch_at(dir.path(), &params).await.unwrap();

        assert!(report.ok);
        assert!(!report.committed);
        assert!(report.dry_run);
        assert_eq!(report.operations[0].status, OpStatus::Ok);
        assert_eq!(
            report.operations[0].rollback_reason.as_deref(),
            Some("Dry run: preview only; no filesystem changes committed")
        );
        assert!(
            report.operations[1]
                .output
                .as_deref()
                .unwrap()
                .contains("[staged]")
        );
        assert!(
            report.operations[1]
                .output
                .as_deref()
                .unwrap()
                .contains("\npreview")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("preview.txt")).unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn json_response_payload_includes_summary_and_operations() {
        let dir = tmp();
        let server = WeavePatchServer::new();
        let mut params = params("=== begin\nwrite report.txt\nhello\n=== end");
        params.response_format = Some(ResponseFormat::Json);

        let report = server.execute_batch_at(dir.path(), &params).await.unwrap();
        let payload = server.render_report_payload(&report, ResponseFormat::Json);
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();

        assert_eq!(json["ok"], true);
        assert_eq!(json["dry_run"], false);
        assert_eq!(json["committed"], true);
        assert_eq!(json["summary"]["total_operations"], 1);
        assert_eq!(json["summary"]["write_operations"], 1);
        assert_eq!(json["operations"][0]["op_type"], "write");
        assert_eq!(json["operations"][0]["status"], "ok");
    }

    #[test]
    fn tool_contract_docs_are_synchronized() {
        let readme = include_str!("../README.md");
        let instructions = tool_contract::server_instructions();

        assert!(
            tool_contract::PATCH_EXEC_DESCRIPTION
                .contains(&format!("Version: {}", tool_contract::VERSION))
        );
        assert!(readme.contains(&format!("version-{}", tool_contract::VERSION)));
        assert!(readme.contains(&tool_contract::readme_defaults_line()));
        assert!(readme.contains("dry_run"));
        assert!(readme.contains("response_format=json"));
        assert!(instructions.contains("dry_run"));
        assert!(instructions.contains("response_format=json"));
        assert!(instructions.contains(&format!(
            "map depth={}, map limit={} chars, read truncation={} lines",
            tool_contract::DEFAULT_MAP_DEPTH,
            tool_contract::DEFAULT_MAP_OUTPUT_LIMIT,
            tool_contract::DEFAULT_READ_LINE_LIMIT
        )));
    }
}
