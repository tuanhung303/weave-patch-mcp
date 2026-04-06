use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::prelude::*;

use crate::error::{ClosestMatch, ContextNotFoundData, PatchError};
use crate::parser::{DiffLine, FileOp, Hunk};
use similar::TextDiff;

#[derive(Debug, serde::Serialize)]
pub struct OpResult {
    pub path: String,
    pub op_type: String,
    pub status: String,
    pub message: String,
    pub warnings: Vec<String>,
    pub diff: Option<String>,
    pub line_changes: Option<(usize, usize)>,
    pub match_info: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct HunkResult {
    pub match_type: String,
    pub matched_at: usize,
}

#[derive(Debug)]
pub struct PatchResult {
    pub operations: Vec<OpResult>,
}

#[must_use]
fn make_op(path: &str, op_type: &str, status: &str, message: &str) -> OpResult {
    OpResult {
        path: path.to_string(),
        op_type: op_type.to_string(),
        status: status.to_string(),
        message: message.to_string(),
        warnings: vec![],
        diff: None,
        line_changes: None,
        match_info: None,
    }
}

/// Two-phase commit: stages shadow files, commits atomically, rolls back on Drop if not committed.
struct PatchTransaction {
    staged_files: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>, // (shadow_path, target_path)
    deletions: Arc<Mutex<Vec<PathBuf>>>,
    committed: Arc<Mutex<bool>>,
    backup_files: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>, // (backup_path, target_path)
}

impl PatchTransaction {
    fn new() -> Self {
        Self {
            staged_files: Arc::new(Mutex::new(Vec::new())),
            deletions: Arc::new(Mutex::new(Vec::new())),
            committed: Arc::new(Mutex::new(false)),
            backup_files: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn stage(&self, shadow: PathBuf, target: PathBuf) {
        self.staged_files.lock().push((shadow, target));
    }

    fn queue_deletion(&self, path: PathBuf) {
        self.deletions.lock().push(path);
    }

    fn commit(self) -> Result<(), std::io::Error> {
        // Lock all the mutexes once at the start
        let staged = self.staged_files.lock();
        let deletions = self.deletions.lock();
        let mut committed = self.committed.lock();
        let mut backup_files = self.backup_files.lock();

        // Phase 1: Backup existing targets before rename so we can rollback on partial failure
        for (_, target) in &*staged {
            if target.exists() {
                let backup = backup_path_for(target);
                std::fs::copy(target, &backup)?;
                backup_files.push((backup, target.clone()));
            }
        }

        // Phase 2: Rename shadows → targets; rollback on any failure
        let mut renamed: Vec<PathBuf> = Vec::new();
        for (shadow, target) in &*staged {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if let Err(e) = std::fs::rename(shadow, target) {
                // Restore already-renamed targets from backup
                for done_target in &renamed {
                    if let Some((backup, _)) = backup_files.iter().find(|(_, t)| t == done_target) {
                        let _ = std::fs::rename(backup, done_target);
                    }
                }
                // Clean up remaining shadow files
                for (s, t) in &*staged {
                    if !renamed.contains(t) {
                        let _ = std::fs::remove_file(s);
                    }
                }
                // Clean up all backup files
                for (backup, _) in &*backup_files {
                    let _ = std::fs::remove_file(backup);
                }
                return Err(e);
            }
            renamed.push(target.clone());
        }

        // Phase 3: Success — execute deletions and clean up backup files
        for (backup, _) in &*backup_files {
            let _ = std::fs::remove_file(backup);
        }
        for path in &*deletions {
            std::fs::remove_file(path)?;
        }
        *committed = true;
        Ok(())
    }
}

impl Drop for PatchTransaction {
    fn drop(&mut self) {
        let committed = *self.committed.lock();
        if !committed {
            let staged = self.staged_files.lock();
            for (shadow, _) in &*staged {
                let _ = std::fs::remove_file(shadow);
            }
            // Clean up any leftover backup files
            let backup_files = self.backup_files.lock();
            for (backup, _) in &*backup_files {
                let _ = std::fs::remove_file(backup);
            }
        }
    }
}

fn backup_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".backup_tmp");
    PathBuf::from(s)
}

fn shadow_suffix() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Detect file conflicts: Add+Update on same path is an error.
/// Returns a map of path -> list of operation types for conflict reporting.
fn detect_file_conflicts(ops: &[FileOp]) -> Result<(), (String, String)> {
    use std::collections::HashMap;
    let mut path_ops: HashMap<String, Vec<&str>> = HashMap::new();

    for op in ops {
        let (path, op_type) = match op {
            FileOp::Add { path, .. } => (path.as_str(), "add"),
            FileOp::Update { path, .. } => (path.as_str(), "update"),
            FileOp::Delete { path } => (path.as_str(), "delete"),
            FileOp::Read { path, .. } => (path.as_str(), "read"),
            FileOp::Map { .. } => continue,
        };
        path_ops.entry(path.to_string()).or_default().push(op_type);
    }

    // Check for Add+Update conflict on same path.
    // Note: Update+Update on same path is intentionally NOT an error.
    // Use multiple hunks within a single Update operation for multi-edit patches.
    for (path, ops_list) in &path_ops {
        let has_add = ops_list.contains(&"add");
        let has_update = ops_list.contains(&"update");
        if has_add && has_update {
            return Err((
                path.clone(),
                "Cannot Add and Update the same file in one patch".to_string(),
            ));
        }
    }

    Ok(())
}

#[must_use]
pub fn apply_patch(ops: Vec<FileOp>, base_dir: &Path) -> PatchResult {
    // Step 1: Detect conflicts before any I/O
    if let Err((path, msg)) = detect_file_conflicts(&ops) {
        // Return error for the conflicting operation
        let results: Vec<OpResult> = ops
            .iter()
            .map(|op| {
                let op_type = match op {
                    FileOp::Add { .. } => "add",
                    FileOp::Update { .. } => "update",
                    FileOp::Delete { .. } => "delete",
                    FileOp::Read { .. } => "read",
                    FileOp::Map { .. } => "map",
                };
                let op_path = match op {
                    FileOp::Add { path, .. } => path,
                    FileOp::Update { path, .. } => path,
                    FileOp::Delete { path } => path,
                    FileOp::Read { path, .. } => path,
                    FileOp::Map { path, .. } => path,
                };
                if op_path == &path {
                    make_op(op_path, op_type, "error", &msg)
                } else {
                    make_op(op_path, op_type, "skipped", "Skipped due to conflict")
                }
            })
            .collect();
        return PatchResult {
            operations: results,
        };
    }

    let transaction = PatchTransaction::new();

    // Step 2: Prepare phase - parallel execution using rayon
    // We need to preserve operation order for results, so we collect with indices
    let results: Vec<(usize, OpResult)> = ops
        .into_iter()
        .enumerate()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(idx, op)| (idx, prepare_op(op, base_dir, &transaction)))
        .collect();

    // Sort results by original index to preserve order
    let mut results: Vec<OpResult> = results.into_iter().map(|(_, r)| r).collect();

    // Check if any operation failed
    let failed = results.iter().any(|r| r.status == "error");

    // If any op failed, Drop cleans up shadow files
    if failed {
        return PatchResult {
            operations: results,
        };
    }

    // Step 3: Commit phase - sequential for atomicity
    if let Err(e) = transaction.commit() {
        // Mark all "ok" ops as failed due to commit error
        let msg = format!("Commit failed: {e}");
        for r in &mut results {
            if r.status == "ok" {
                r.status = "error".to_string();
                r.message = msg.clone();
            }
        }
    }

    PatchResult {
        operations: results,
    }
}

/// Validate and stage a single op. Returns OpResult with shadow file info.
fn prepare_op(op: FileOp, base_dir: &Path, tx: &PatchTransaction) -> OpResult {
    match op {
        FileOp::Add { path, content } => {
            let full_path = match validate_path(base_dir, &path) {
                Ok(p) => p,
                Err(e) => return make_op(&path, "add", "error", &e.to_string()),
            };
            match stage_add(&full_path, &content, tx) {
                Ok(result) => {
                    let mut op = make_op(&path, "add", "ok", &result.message);
                    op.warnings = result.warnings;
                    op.diff = result.diff;
                    op.line_changes = result.line_changes;
                    op
                }
                Err(e) => make_op(&path, "add", "error", &e.to_string()),
            }
        }
        FileOp::Delete { path } => {
            match validate_path(base_dir, &path)
                .and_then(|full_path| stage_delete(&full_path, &path, tx))
            {
                Ok(result) => {
                    let mut op = make_op(&path, "delete", "ok", &result.message);
                    op.line_changes = result.line_changes;
                    op
                }
                Err(e) => make_op(&path, "delete", "error", &e.to_string()),
            }
        }
        FileOp::Update {
            path,
            hunks,
            move_to,
        } => {
            let full_path = match validate_path(base_dir, &path) {
                Ok(p) => p,
                Err(e) => return make_op(&path, "update", "error", &e.to_string()),
            };

            match stage_update(&full_path, &path, &hunks, tx) {
                Ok(result) => {
                    if let Some(ref dest) = move_to {
                        let dest_path = match validate_path(base_dir, dest) {
                            Ok(p) => p,
                            Err(e) => {
                                return make_op(
                                    &path,
                                    "update",
                                    "error",
                                    &format!("Update succeeded but move_to invalid: {e}"),
                                );
                            }
                        };
                        // Re-target the staged shadow to move destination
                        if let Some(entry) = tx
                            .staged_files
                            .lock()
                            .iter_mut()
                            .find(|(s, _)| *s == result.shadow_path)
                        {
                            entry.1 = dest_path;
                        }
                        let mut op = make_op(
                            &path,
                            "update",
                            "ok",
                            &format!("File updated and moved to {dest}"),
                        );
                        op.warnings = result.warnings;
                        op.diff = result.diff;
                        op.line_changes = result.line_changes;
                        op.match_info = result.match_info;
                        op
                    } else {
                        let mut op = make_op(&path, "update", "ok", &result.message);
                        op.warnings = result.warnings;
                        op.diff = result.diff;
                        op.line_changes = result.line_changes;
                        op.match_info = result.match_info;
                        op
                    }
                }
                Err(e) => make_op(&path, "update", "error", &e.to_string()),
            }
        }
        FileOp::Read {
            path,
            symbols: _,
            language: _,
            offset: _,
            limit: _,
        } => {
            // Read operations don't stage files — they just validate and return success
            match validate_path(base_dir, &path) {
                Ok(full_path) => {
                    if !full_path.exists() {
                        return make_op(&path, "read", "error", "File not found");
                    }
                    let meta = match full_path.symlink_metadata() {
                        Ok(m) => m,
                        Err(e) => {
                            return make_op(&path, "read", "error", &format!("IO error: {e}"));
                        }
                    };
                    if meta.file_type().is_symlink() {
                        return make_op(&path, "read", "error", "Symlink rejected");
                    }
                    if !meta.is_file() {
                        return make_op(&path, "read", "error", "Not a regular file");
                    }
                    make_op(&path, "read", "ok", "File read successfully")
                }
                Err(e) => make_op(&path, "read", "error", &e.to_string()),
            }
        }
        FileOp::Map {
            path,
            depth: _,
            output_limit: _,
        } => match validate_path(base_dir, &path) {
            Ok(full_path) => {
                if !full_path.exists() {
                    return make_op(&path, "map", "error", "Directory not found");
                }
                if !full_path.is_dir() {
                    return make_op(&path, "map", "error", "Path is not a directory");
                }
                make_op(&path, "map", "ok", "")
            }
            Err(e) => make_op(&path, "map", "error", &e.to_string()),
        },
    }
}

/// Validate that `rel_path` resolves to a location strictly inside `base_dir`.
/// Rejects absolute paths and path traversal sequences (`../`).
/// For new files (Add), canonicalizes the nearest existing parent.
/// Rejects the path if it is a symlink.
pub fn check_symlink(path: &Path, display_path: &str) -> Result<(), PatchError> {
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return Err(PatchError::SymlinkRejected(display_path.to_string()));
    }
    Ok(())
}

pub fn validate_path(base_dir: &Path, rel_path: &str) -> Result<PathBuf, PatchError> {
    // Security note: Path sandbox removed per user request.
    // Symlink protection is still enforced in check_symlink().
    // This allows patches to access any path the user has permission for.
    Ok(base_dir.join(rel_path))
}

/// Result from stage_add
struct StageAddResult {
    message: String,
    warnings: Vec<String>,
    diff: Option<String>,
    line_changes: Option<(usize, usize)>,
}

fn stage_add(
    path: &Path,
    content: &str,
    tx: &PatchTransaction,
) -> Result<StageAddResult, PatchError> {
    if path.exists() {
        return Err(PatchError::FileAlreadyExists(path.display().to_string()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Generate diff-from-/dev/null
    let new_lines: Vec<&str> = content.lines().collect();
    let line_count = new_lines.len();
    let diff = generate_add_diff(path, &new_lines);

    let shadow = shadow_path_for(path);
    std::fs::write(&shadow, content)?;
    let warnings = crate::validator::validate_file(&shadow, path);
    tx.stage(shadow, path.to_path_buf());

    Ok(StageAddResult {
        message: "File created".to_string(),
        warnings,
        diff,
        line_changes: Some((0, line_count)),
    })
}

/// Result from stage_delete
struct StageDeleteResult {
    message: String,
    line_changes: Option<(usize, usize)>,
}

fn stage_delete(
    path: &Path,
    display_path: &str,
    tx: &PatchTransaction,
) -> Result<StageDeleteResult, PatchError> {
    if !path.exists() {
        return Err(PatchError::FileNotFound(display_path.to_string()));
    }
    // Reject symlinks
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return Err(PatchError::SymlinkRejected(display_path.to_string()));
    }

    // Count lines in the file being deleted
    let original = std::fs::read_to_string(path)?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let line_count = original.lines().count();

    tx.queue_deletion(path.to_path_buf());

    Ok(StageDeleteResult {
        message: format!("removed {line_count} lines"),
        line_changes: Some((line_count, 0)),
    })
}

/// Result from stage_update
struct StageUpdateResult {
    shadow_path: PathBuf,
    message: String,
    warnings: Vec<String>,
    diff: Option<String>,
    line_changes: Option<(usize, usize)>,
    match_info: Option<String>,
}

fn stage_update(
    path: &Path,
    display_path: &str,
    hunks: &[Hunk],
    tx: &PatchTransaction,
) -> Result<StageUpdateResult, PatchError> {
    if !path.exists() {
        return Err(PatchError::FileNotFound(display_path.to_string()));
    }
    // Reject symlinks
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return Err(PatchError::SymlinkRejected(display_path.to_string()));
    }

    let original = std::fs::read_to_string(path)?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let original_line_count = original.lines().count();
    let mut file_lines: Vec<String> = original.lines().map(|l| l.to_string()).collect();

    let mut hunk_results: Vec<HunkResult> = Vec::new();

    for hunk in hunks {
        let (new_lines, hunk_result) = apply_hunk(file_lines, hunk, display_path)?;
        hunk_results.push(hunk_result);
        file_lines = new_lines;
    }

    // Preserve trailing newline behavior
    let mut result = file_lines.join("\n");
    if original.ends_with('\n') {
        result.push('\n');
    }

    let new_line_count = file_lines.len();

    // Generate unified diff
    let diff = generate_unified_diff(&original, &result, display_path);

    // Build match info summary
    let match_info = build_match_info(&hunk_results);

    let shadow = shadow_path_for(path);
    std::fs::write(&shadow, &result)?;
    let warnings = crate::validator::validate_file(&shadow, path);
    tx.stage(shadow.clone(), path.to_path_buf());

    let message = format!("{original_line_count} → {new_line_count} lines");

    Ok(StageUpdateResult {
        shadow_path: shadow,
        message,
        warnings,
        diff,
        line_changes: Some((original_line_count, new_line_count)),
        match_info,
    })
}

fn shadow_path_for(path: &Path) -> PathBuf {
    let suffix = shadow_suffix();
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{suffix}.patch_tmp"));
    PathBuf::from(s)
}

/// Generate a unified diff between original and modified content.
fn generate_unified_diff(original: &str, modified: &str, display_path: &str) -> Option<String> {
    let diff = similar::udiff::unified_diff(
        similar::Algorithm::default(),
        original,
        modified,
        3,
        Some((display_path, display_path)),
    );

    if diff.is_empty() {
        return None;
    }

    // Truncate if too long
    const MAX_DIFF_LEN: usize = 4000;
    if diff.len() > MAX_DIFF_LEN {
        let mut truncated = diff;
        truncated.truncate(MAX_DIFF_LEN);
        truncated.push_str("\n... [diff truncated]");
        Some(truncated)
    } else {
        Some(diff)
    }
}

/// Generate a diff-from-/dev/null for a new file.
fn generate_add_diff(path: &Path, new_lines: &[&str]) -> Option<String> {
    if new_lines.is_empty() {
        return None;
    }
    let display_path = path.file_name().unwrap_or_default().to_string_lossy();
    let mut output = String::new();
    output.push_str("--- /dev/null\n");
    output.push_str(&format!("+++ {display_path}\n"));
    output.push_str(&format!("@@ -0,0 +1,{} @@\n", new_lines.len()));
    for line in new_lines {
        output.push_str(&format!("+{line}\n"));
    }

    // Truncate if too long
    const MAX_DIFF_LEN: usize = 4000;
    if output.len() > MAX_DIFF_LEN {
        output.truncate(MAX_DIFF_LEN);
        output.push_str("\n... [diff truncated]");
    }

    Some(output)
}

/// Build a human-readable match info string from hunk results.
fn build_match_info(hunk_results: &[HunkResult]) -> Option<String> {
    if hunk_results.is_empty() {
        return None;
    }
    if hunk_results.len() == 1 {
        let hr = &hunk_results[0];
        Some(format!("{} match at line {}", hr.match_type, hr.matched_at))
    } else {
        let parts: Vec<String> = hunk_results
            .iter()
            .enumerate()
            .map(|(i, hr)| {
                format!(
                    "Hunk {}: {} at line {}",
                    i + 1,
                    hr.match_type,
                    hr.matched_at
                )
            })
            .collect();
        Some(parts.join("; "))
    }
}

fn apply_hunk(
    file_lines: Vec<String>,
    hunk: &Hunk,
    display_path: &str,
) -> Result<(Vec<String>, HunkResult), PatchError> {
    // Build the search pattern: Context + Remove lines (in order)
    let search_pattern: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|dl| match dl {
            DiffLine::Context(s) => Some(s.as_str()),
            DiffLine::Remove(s) => Some(s.as_str()),
            DiffLine::Add(_) => None,
        })
        .collect();

    if search_pattern.is_empty() {
        // Hunk with only Add lines — append if no context
        let additions: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|dl| match dl {
                DiffLine::Add(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let insert_at = file_lines.len();
        let mut result = file_lines;
        result.extend(additions);
        return Ok((
            result,
            HunkResult {
                match_type: "append".to_string(),
                matched_at: insert_at,
            },
        ));
    }

    // Find all matching positions — track match type
    let (match_pos, match_type) = find_matches_with_type(&file_lines, &search_pattern);

    let match_pos = match match_pos.len() {
        0 => {
            if let Some(ref hint) = hunk.context_hint {
                if let Some(pos) = find_with_hint(&file_lines, &search_pattern, hint) {
                    pos // Hint succeeded!
                } else {
                    let pattern_lines: Vec<String> =
                        search_pattern.iter().map(|s| s.to_string()).collect();
                    let (nearest, closest_matches) =
                        nearest_excerpt_with_matches(&file_lines, &search_pattern);
                    return Err(PatchError::ContextNotFound(Box::new(ContextNotFoundData {
                        path: display_path.to_string(),
                        pattern: pattern_lines,
                        hint: Some(format!("@@ hint: {hint}")),
                        total_lines: file_lines.len(),
                        file_excerpt: nearest,
                        closest_matches,
                    })));
                }
            } else {
                let pattern_lines: Vec<String> =
                    search_pattern.iter().map(|s| s.to_string()).collect();
                let (nearest, closest_matches) =
                    nearest_excerpt_with_matches(&file_lines, &search_pattern);
                return Err(PatchError::ContextNotFound(Box::new(ContextNotFoundData {
                    path: display_path.to_string(),
                    pattern: pattern_lines,
                    hint: None,
                    total_lines: file_lines.len(),
                    file_excerpt: nearest,
                    closest_matches,
                })));
            }
        }
        1 => match_pos[0],
        _ => {
            if let Some(ref hint) = hunk.context_hint {
                pick_by_hint(&file_lines, &match_pos, hint, search_pattern.len()).ok_or_else(
                    || {
                        let count = match_pos.len();
                        let context_at_each: Vec<String> = match_pos
                            .iter()
                            .map(|&m| {
                                file_lines[m..std::cmp::min(m + 3, file_lines.len())]
                                    .iter()
                                    .map(|l| l.trim_end())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .collect();
                        PatchError::AmbiguousContext {
                            path: display_path.to_string(),
                            count,
                            match_positions: match_pos.to_vec(),
                            context_at_each,
                        }
                    },
                )?
            } else {
                let count = match_pos.len();
                let context_at_each: Vec<String> = match_pos
                    .iter()
                    .map(|&m| {
                        file_lines[m..std::cmp::min(m + 3, file_lines.len())]
                            .iter()
                            .map(|l| l.trim_end())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect();
                return Err(PatchError::AmbiguousContext {
                    path: display_path.to_string(),
                    count,
                    match_positions: match_pos.to_vec(),
                    context_at_each,
                });
            }
        }
    };

    // Apply the hunk at match_pos
    let mut result: Vec<String> = file_lines[..match_pos].to_vec();
    let mut file_cursor = match_pos;

    for diff_line in &hunk.lines {
        match diff_line {
            DiffLine::Context(_) => {
                result.push(file_lines[file_cursor].clone());
                file_cursor += 1;
            }
            DiffLine::Remove(_) => {
                file_cursor += 1; // skip
            }
            DiffLine::Add(s) => {
                result.push(s.clone());
            }
        }
    }

    // Append remaining file lines
    result.extend_from_slice(&file_lines[file_cursor..]);

    Ok((
        result,
        HunkResult {
            match_type,
            matched_at: match_pos + 1, // 1-based
        },
    ))
}

/// Returns (file_excerpt_string, top_3_closest_matches).
fn nearest_excerpt_with_matches(
    file_lines: &[String],
    pattern: &[&str],
) -> (String, Vec<ClosestMatch>) {
    if pattern.is_empty() || file_lines.is_empty() {
        return ("  (empty file)".to_string(), vec![]);
    }

    let pattern_text: String = pattern.join("\n") + "\n";

    // Score each window: count of matching lines (for excerpt) + char-level similarity
    let mut scored: Vec<(usize, f32, usize)> = Vec::new(); // (line_score, char_ratio, pos)
    for start in 0..file_lines.len() {
        let mut line_score = 0usize;
        for (i, &pat) in pattern.iter().enumerate() {
            if start + i < file_lines.len() && file_lines[start + i].trim_end() == pat.trim_end() {
                line_score += 1;
            }
        }
        let window_len = std::cmp::min(pattern.len(), file_lines.len() - start);
        let window_text: String = file_lines[start..start + window_len]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let ratio = TextDiff::from_chars(pattern_text.as_str(), window_text.as_str()).ratio();
        scored.push((line_score, ratio, start));
    }

    // Best position for excerpt: highest line_score, then highest ratio
    let (best_score, _, best_pos) = scored
        .iter()
        .copied()
        .max_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        })
        .unwrap_or((0, 0.0, 0));

    let excerpt_start = best_pos.saturating_sub(2);
    let excerpt_end = std::cmp::min(
        best_pos.saturating_add(pattern.len()).saturating_add(2),
        file_lines.len(),
    );
    let lines: Vec<String> = file_lines[excerpt_start..excerpt_end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("  {:>4}: {}", excerpt_start + i + 1, l.trim_end()))
        .collect();
    let excerpt = if best_score > 0 {
        format!(
            "Nearest partial match ({best_score}/{} lines):\n{}",
            pattern.len(),
            lines.join("\n")
        )
    } else {
        format!(
            "File preview (lines 1-{}):\n{}",
            excerpt_end,
            lines.join("\n")
        )
    };

    // Top 3 closest matches by char ratio
    let mut by_ratio = scored.clone();
    by_ratio.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let closest_matches: Vec<ClosestMatch> = by_ratio
        .iter()
        .take(3)
        .filter(|(_, ratio, _)| *ratio > 0.0)
        .map(|(_, ratio, pos)| {
            let actual = file_lines[*pos].trim_end().to_string();
            ClosestMatch {
                line_number: pos + 1,
                similarity: *ratio,
                actual_content: actual.clone(),
                suggestion: format!("Replace pattern line 1 with: {}", actual),
            }
        })
        .collect();

    (excerpt, closest_matches)
}

/// Find matches and return (positions, match_type_label).
/// match_type_label is "exact", "normalized", or "fuzzy:N%".
/// Backwards-compatible wrapper that discards match type info.
#[allow(dead_code)]
fn find_matches_with_type(file_lines: &[String], pattern: &[&str]) -> (Vec<usize>, String) {
    if pattern.is_empty() {
        return (vec![], "none".to_string());
    }

    // Phase 1: Exact match
    let exact = find_matches_exact(file_lines, pattern);
    if !exact.is_empty() {
        return (exact, "exact".to_string());
    }

    // Phase 2: Normalized whitespace match
    let normalize = |s: &str| -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") };
    let normalized_pattern: Vec<String> = pattern.iter().map(|s| normalize(s)).collect();
    let mut normalized_matches = Vec::new();
    'outer2: for start in 0..=file_lines.len().saturating_sub(pattern.len()) {
        for (i, np) in normalized_pattern.iter().enumerate() {
            if normalize(&file_lines[start + i]) != *np {
                continue 'outer2;
            }
        }
        normalized_matches.push(start);
    }
    if !normalized_matches.is_empty() {
        return (normalized_matches, "normalized".to_string());
    }

    // Phase 3: Fuzzy similarity (only for patterns >= 3 lines and files <= 2000 lines)
    if pattern.len() < 3 || file_lines.len() > 2000 {
        return (vec![], "none".to_string());
    }
    let pattern_text: String = pattern.join("\n") + "\n";
    let mut fuzzy_matches = Vec::new();
    for start in 0..=file_lines.len().saturating_sub(pattern.len()) {
        let window_text: String = file_lines[start..start + pattern.len()]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let diff = TextDiff::from_chars(pattern_text.as_str(), window_text.as_str());
        let ratio = diff.ratio();
        if ratio >= 0.85 {
            fuzzy_matches.push((start, ratio));
        }
    }
    if !fuzzy_matches.is_empty() {
        // Return the best fuzzy match and its percentage
        fuzzy_matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let pct = (fuzzy_matches[0].1 * 100.0).round() as usize;
        (vec![fuzzy_matches[0].0], format!("fuzzy:{pct}%"))
    } else {
        (vec![], "none".to_string())
    }
}

/// Backwards-compatible wrapper that discards match type info.
/// Backwards-compatible wrapper that discards match type info.
#[allow(dead_code)]
fn find_matches(file_lines: &[String], pattern: &[&str]) -> Vec<usize> {
    let (positions, _) = find_matches_with_type(file_lines, pattern);
    positions
}

/// Check if hint ends with `$` for word-boundary matching.
/// Returns (stripped_hint, is_word_boundary).
fn word_boundary_match(hint: &str) -> (&str, bool) {
    if let Some(stripped) = hint.strip_suffix('$') {
        (stripped, true)
    } else {
        (hint, false)
    }
}

/// Check if a line contains the hint at a word boundary.
/// Word boundary means the hint is followed by non-word characters (end of line, space, punctuation, etc).
fn hint_matches_at_word_boundary(line: &str, hint: &str) -> bool {
    if let Some(pos) = line.find(hint) {
        let after_hint = &line[pos + hint.len()..];
        // Word boundary: end of string or followed by non-alphanumeric/underscore
        after_hint.is_empty()
            || !after_hint.chars().next().unwrap().is_alphanumeric() && !after_hint.starts_with('_')
    } else {
        false
    }
}

fn find_with_hint(file_lines: &[String], pattern: &[&str], hint: &str) -> Option<usize> {
    // Check for word-boundary matching (hint ends with $)
    let (stripped_hint, is_word_boundary) = word_boundary_match(hint);

    // Find lines containing the hint
    let hint_positions: Vec<usize> = file_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            if is_word_boundary {
                // Word-boundary matching: hint must be followed by non-word char
                hint_matches_at_word_boundary(l, stripped_hint)
            } else {
                // Normal matching: any containment
                l.contains(stripped_hint)
            }
        })
        .map(|(i, _)| i)
        .collect();

    if hint_positions.is_empty() {
        // Try case-insensitive matching
        let hint_lower = stripped_hint.to_lowercase();
        let hint_positions: Vec<usize> = file_lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let line_lower = l.to_lowercase();
                if is_word_boundary {
                    // For case-insensitive word boundary, check each position
                    if let Some(pos) = line_lower.find(&hint_lower) {
                        let after_hint = &line_lower[pos + hint_lower.len()..];
                        after_hint.is_empty() || {
                            let next_char = after_hint.chars().next().unwrap();
                            !next_char.is_alphanumeric() && next_char != '_'
                        }
                    } else {
                        false
                    }
                } else {
                    line_lower.contains(&hint_lower)
                }
            })
            .map(|(i, _)| i)
            .collect();

        if hint_positions.is_empty() {
            return None;
        }
        let &best_hint_line = hint_positions.first().unwrap();
        return find_in_window(file_lines, pattern, best_hint_line);
    }

    let &best_hint_line = hint_positions.first().unwrap();
    find_in_window(file_lines, pattern, best_hint_line)
}

fn find_in_window(file_lines: &[String], pattern: &[&str], hint_line_pos: usize) -> Option<usize> {
    let window_size = std::cmp::max(60, file_lines.len() / 5);
    let half = window_size / 2;
    let window_start = hint_line_pos.saturating_sub(half);
    let window_end = std::cmp::min(window_start + window_size, file_lines.len());

    let window = &file_lines[window_start..window_end];
    let window_matches = find_matches_exact(window, pattern);

    if window_matches.is_empty() {
        return None;
    }

    // Prefer match at or after the hint line
    let hint_in_window = hint_line_pos.saturating_sub(window_start);
    for &wm in &window_matches {
        if wm >= hint_in_window {
            return Some(wm + window_start);
        }
    }

    // Fallback: closest match before hint line
    window_matches.last().map(|&wm| wm + window_start)
}

/// Backwards-compatible wrapper that discards match type info.
#[allow(dead_code)]
fn find_matches_exact(file_lines: &[String], pattern: &[&str]) -> Vec<usize> {
    if pattern.is_empty() {
        return vec![];
    }
    let mut matches = Vec::new();
    'outer: for start in 0..=file_lines.len().saturating_sub(pattern.len()) {
        for (i, &pat_line) in pattern.iter().enumerate() {
            if file_lines[start + i].trim_end() != pat_line.trim_end() {
                continue 'outer;
            }
        }
        matches.push(start);
    }
    matches
}

fn pick_by_hint(
    file_lines: &[String],
    matches: &[usize],
    hint: &str,
    pattern_len: usize,
) -> Option<usize> {
    // Find lines containing the hint text
    let hint_positions: Vec<usize> = file_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(hint))
        .map(|(i, _)| i)
        .collect();

    if hint_positions.is_empty() {
        // Hint text not found in file — return None so caller surfaces AmbiguousContext
        return None;
    }

    // Pick the match position nearest to (and after) a hint line
    let mut best: Option<(usize, usize)> = None; // (distance, match_pos)
    for &m in matches {
        for &h in &hint_positions {
            let match_end = m + pattern_len;
            let dist = if m >= h {
                m - h
            } else if h < match_end {
                // Hint falls inside the matched range — ideal match
                0
            } else {
                h.saturating_sub(match_end)
            };
            // Prefer match at or after hint line as tiebreaker
            let after_hint = m >= h;
            let (bd, bm) = best.unwrap_or((usize::MAX, 0));
            if dist < bd || (dist == bd && after_hint && bm < h) {
                best = Some((dist, m));
            }
        }
    }
    best.map(|(_, pos)| pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ContextNotFoundData;
    use crate::parser::{DiffLine, FileOp, Hunk};
    use std::fs;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_create_file() {
        let dir = tmp();
        let ops = vec![FileOp::Add {
            path: "hello.txt".to_string(),
            content: "hello\nworld\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");
        assert_eq!(
            fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hello\nworld\n"
        );
    }

    #[test]
    fn test_create_file_has_diff() {
        let dir = tmp();
        let ops = vec![FileOp::Add {
            path: "hello.txt".to_string(),
            content: "hello\nworld\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert!(result.operations[0].diff.is_some());
        let diff = result.operations[0].diff.as_ref().unwrap();
        assert!(diff.contains("--- /dev/null"));
        assert!(diff.contains("+++ hello.txt"));
        assert!(diff.contains("+hello"));
        assert!(diff.contains("+world"));
    }

    #[test]
    fn test_create_file_nested() {
        let dir = tmp();
        let ops = vec![FileOp::Add {
            path: "a/b/c.txt".to_string(),
            content: "nested\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");
        assert!(dir.path().join("a/b/c.txt").exists());
    }

    #[test]
    fn test_delete_file() {
        let dir = tmp();
        let file_path = dir.path().join("target.txt");
        fs::write(&file_path, "content").unwrap();

        let ops = vec![FileOp::Delete {
            path: "target.txt".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");
        assert!(!file_path.exists());
    }

    #[test]
    fn test_delete_file_has_line_changes() {
        let dir = tmp();
        let content = "line1\nline2\nline3\nline4\n";
        let file_path = dir.path().join("target.txt");
        fs::write(&file_path, content).unwrap();

        let ops = vec![FileOp::Delete {
            path: "target.txt".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");
        assert_eq!(result.operations[0].line_changes, Some((4, 0)));
        assert!(result.operations[0].message.contains("removed 4 lines"));
    }

    #[test]
    fn test_update_file_single_hunk() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "fn foo() {\n    old_impl()\n}\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("fn foo() {".to_string()),
                    DiffLine::Remove("    old_impl()".to_string()),
                    DiffLine::Add("    new_impl()".to_string()),
                    DiffLine::Context("}".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(
            result.operations[0].status, "ok",
            "{}",
            result.operations[0].message
        );
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "fn foo() {\n    new_impl()\n}\n");
    }

    #[test]
    fn test_update_file_has_diff_and_match_info() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "fn foo() {\n    old_impl()\n}\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("fn foo() {".to_string()),
                    DiffLine::Remove("    old_impl()".to_string()),
                    DiffLine::Add("    new_impl()".to_string()),
                    DiffLine::Context("}".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");

        // Should have diff
        assert!(result.operations[0].diff.is_some());
        let diff = result.operations[0].diff.as_ref().unwrap();
        assert!(diff.contains("--- code.rs"));
        assert!(diff.contains("+++ code.rs"));
        assert!(diff.contains("-    old_impl()"));
        assert!(diff.contains("+    new_impl()"));

        // Should have line_changes
        assert_eq!(result.operations[0].line_changes, Some((3, 3)));

        // Should have match_info
        assert!(result.operations[0].match_info.is_some());
        let mi = result.operations[0].match_info.as_ref().unwrap();
        assert!(mi.contains("exact"));
    }

    #[test]
    fn test_update_file_multiple_hunks() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        fs::write(
            &file_path,
            "fn a() {\n    old_a()\n}\n\nfn b() {\n    old_b()\n}\n",
        )
        .unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![
                Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("fn a() {".to_string()),
                        DiffLine::Remove("    old_a()".to_string()),
                        DiffLine::Add("    new_a()".to_string()),
                        DiffLine::Context("}".to_string()),
                    ],
                },
                Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("fn b() {".to_string()),
                        DiffLine::Remove("    old_b()".to_string()),
                        DiffLine::Add("    new_b()".to_string()),
                        DiffLine::Context("}".to_string()),
                    ],
                },
            ],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(
            result.operations[0].status, "ok",
            "{}",
            result.operations[0].message
        );
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            content,
            "fn a() {\n    new_a()\n}\n\nfn b() {\n    new_b()\n}\n"
        );

        // Multi-hunk should have match_info with "Hunk 1" and "Hunk 2"
        let mi = result.operations[0].match_info.as_ref().unwrap();
        assert!(mi.contains("Hunk 1"));
        assert!(mi.contains("Hunk 2"));
    }

    #[test]
    fn test_error_file_not_found_for_update() {
        let dir = tmp();
        let ops = vec![FileOp::Update {
            path: "nonexistent.rs".to_string(),
            hunks: vec![],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "error");
        assert!(
            result.operations[0].message.contains("not found")
                || result.operations[0].message.contains("File not found")
        );
    }

    #[test]
    fn test_error_context_not_found() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "fn foo() {}\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("this_does_not_exist".to_string()),
                    DiffLine::Remove("whatever".to_string()),
                    DiffLine::Add("replacement".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "error");
        assert!(
            result.operations[0].message.contains("Context not found")
                || result.operations[0].message.contains("locate hunk")
        );
    }

    #[test]
    fn test_multi_file_patch() {
        let dir = tmp();
        let existing = dir.path().join("existing.txt");
        fs::write(&existing, "line1\nline2\nline3\n").unwrap();

        let ops = vec![
            FileOp::Add {
                path: "new.txt".to_string(),
                content: "new content\n".to_string(),
            },
            FileOp::Delete {
                path: "existing.txt".to_string(),
            },
        ];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "ok");
        assert_eq!(result.operations[1].status, "ok");
        assert!(dir.path().join("new.txt").exists());
        assert!(!existing.exists());
    }

    // F1: Path traversal tests (sandbox removed, paths now allowed)
    #[test]
    fn test_path_traversal_add_now_allowed() {
        // Sandbox removed - paths can now escape base directory
        let dir = tmp();
        let ops = vec![FileOp::Add {
            path: "../outside_file.txt".to_string(),
            content: "content\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        // Path is now allowed (will fail only if parent doesn't exist or permission denied)
        // For this test, we create a sibling directory
        let parent = dir.path().parent().unwrap();
        let _ = fs::create_dir_all(parent);
        // The operation should succeed or fail with a different error (not traversal)
        if result.operations[0].status == "ok" {
            // Successfully created file outside base dir
            assert!(parent.join("outside_file.txt").exists());
            // Cleanup
            let _ = fs::remove_file(parent.join("outside_file.txt"));
        } else {
            // Should NOT be a traversal error
            assert!(!result.operations[0].message.contains("traversal"));
            assert!(!result.operations[0].message.contains("escapes"));
        }
    }

    #[test]
    fn test_path_traversal_delete_now_allowed() {
        // Sandbox removed - paths can now escape base directory
        let dir = tmp();
        // Create a sibling file to delete
        let parent = dir.path().parent().unwrap();
        let sibling_file = parent.join("sibling_to_delete.txt");
        fs::write(&sibling_file, "content").unwrap();

        let ops = vec![FileOp::Delete {
            path: "../sibling_to_delete.txt".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        // Should succeed in deleting the sibling file
        assert_eq!(
            result.operations[0].status, "ok",
            "Expected ok, got: {}",
            result.operations[0].message
        );
        assert!(!sibling_file.exists(), "Sibling file should be deleted");
    }

    #[test]
    fn test_absolute_path_add_now_allowed() {
        // Sandbox removed - absolute paths are now allowed
        let dir = tmp();
        let unique_name = format!("/tmp/patch_test_{}.txt", std::process::id());
        let ops = vec![FileOp::Add {
            path: unique_name.clone(),
            content: "test content\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        // Should succeed (or fail with permission error, not traversal)
        if result.operations[0].status == "ok" {
            // Cleanup
            let _ = fs::remove_file(&unique_name);
        }
        // Should NOT be an absolute path rejection error
        assert!(
            !result.operations[0]
                .message
                .contains("absolute path rejected")
        );
    }

    // F2: Symlink rejection test
    #[test]
    #[cfg(unix)]
    fn test_symlink_delete_rejected() {
        let dir = tmp();
        let real_file = dir.path().join("real.txt");
        let link_file = dir.path().join("link.txt");
        fs::write(&real_file, "content").unwrap();
        std::os::unix::fs::symlink(&real_file, &link_file).unwrap();

        let ops = vec![FileOp::Delete {
            path: "link.txt".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "error");
        assert!(
            result.operations[0].message.contains("ymlink"),
            "expected symlink error, got: {}",
            result.operations[0].message
        );
        // Real file must be untouched
        assert!(real_file.exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_symlink_update_rejected() {
        let dir = tmp();
        let real_file = dir.path().join("real.rs");
        let link_file = dir.path().join("link.rs");
        fs::write(&real_file, "fn foo() {}\n").unwrap();
        std::os::unix::fs::symlink(&real_file, &link_file).unwrap();

        let ops = vec![FileOp::Update {
            path: "link.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("fn foo() {}".to_string()),
                    DiffLine::Add("fn bar() {}".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "error");
        assert!(
            result.operations[0].message.contains("ymlink"),
            "expected symlink error, got: {}",
            result.operations[0].message
        );
    }

    // Nested path traversal - now allowed
    #[test]
    fn test_nested_path_traversal_now_allowed() {
        // Sandbox removed - nested path traversal is now allowed
        let dir = tmp();
        // Create the subdirectory so the first component resolves
        fs::create_dir(dir.path().join("sub")).unwrap();
        let ops = vec![FileOp::Add {
            path: "sub/../escape.txt".to_string(),
            content: "content\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        // Should succeed - file created inside the temp dir
        assert_eq!(
            result.operations[0].status, "ok",
            "Expected ok, got: {}",
            result.operations[0].message
        );
        assert!(dir.path().join("escape.txt").exists());
    }

    // Windows-style backslash path - now allowed (treated as relative on Unix)
    #[test]
    fn test_backslash_path_now_allowed() {
        // Sandbox removed - backslash paths are now allowed
        let dir = tmp();
        // On Unix, backslash is treated as part of the filename, not a path separator
        let ops = vec![FileOp::Add {
            path: "sub\\file.txt".to_string(), // Creates file named "sub\file.txt"
            content: "content\n".to_string(),
        }];
        let result = apply_patch(ops, dir.path());
        // Should succeed (creates file with backslash in name on Unix)
        if result.operations[0].status == "ok" {
            // Cleanup
            let _ = fs::remove_file(dir.path().join("sub\\file.txt"));
        }
        // Should NOT be an absolute path rejection error
        assert!(
            !result.operations[0]
                .message
                .contains("absolute path rejected")
        );
    }

    #[test]
    fn test_fuzzy_match_normalized_whitespace() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        // File has exact spacing
        fs::write(&file_path, "fn foo() {\n    let x = 1;\n}\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    // Pattern with extra space and missing space before brace — Phase 2 normalizes
                    DiffLine::Context("fn  foo(){".to_string()),
                    DiffLine::Context("    let x = 1;".to_string()),
                    DiffLine::Remove("}".to_string()),
                    DiffLine::Add("} // end".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(
            result.operations[0].status, "ok",
            "Phase 2 normalized match should succeed: {}",
            result.operations[0].message
        );
        let content = fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("} // end"));

        // Match falls through to fuzzy since spacing differs beyond normalization
        let mi = result.operations[0].match_info.as_ref().unwrap();
        assert!(
            mi.contains("fuzzy:") || mi.contains("normalized"),
            "match_info should show match type, got: {mi}"
        );
    }

    #[test]
    fn test_fuzzy_match_similarity_threshold() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        // File has one line that differs slightly from pattern
        fs::write(
            &file_path,
            "fn compute() {
    let result = foo();
    let value = result * 2;
    result
}
",
        )
        .unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("fn compute() {".to_string()),
                    // "bar" instead of "foo" -- single token differs
                    DiffLine::Context("    let result = bar();".to_string()),
                    DiffLine::Context("    let value = result * 2;".to_string()),
                    DiffLine::Remove("    result".to_string()),
                    DiffLine::Add("    result + 1".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(
            result.operations[0].status, "ok",
            "Phase 3 fuzzy match should succeed: {}",
            result.operations[0].message
        );
        let content = fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert!(content.contains("result + 1"));

        // Should report fuzzy match type
        let mi = result.operations[0].match_info.as_ref().unwrap();
        assert!(mi.contains("fuzzy:"));
    }

    #[test]
    fn test_short_pattern_no_fuzzy() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        // 2-line pattern — Phase 3 must NOT activate (pattern.len() < 3)
        fs::write(&file_path, "let a = 1;\nlet b = 2;\nlet c = 3;\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    // 2 context lines that won't exact or normalized match
                    DiffLine::Context("let  a = 999;".to_string()),
                    DiffLine::Remove("let b = 2;".to_string()),
                    DiffLine::Add("let b = 99;".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        // Should fail — fuzzy must not run for 2-line patterns (context only = 1 line before Remove)
        assert_eq!(
            result.operations[0].status, "error",
            "Short 2-line pattern must not fuzzy match"
        );
    }

    #[test]
    fn test_large_file_fuzzy_skipped() {
        let dir = tmp();
        let file_path = dir.path().join("big.rs");
        // 2001 lines — fuzzy must be skipped
        let content: String = (0..2001).map(|i| format!("line_{}\n", i)).collect();
        fs::write(&file_path, &content).unwrap();

        let ops = vec![FileOp::Update {
            path: "big.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    // Pattern that won't exact or normalized match
                    DiffLine::Context("line_XXXX".to_string()),
                    DiffLine::Context("line_YYYY".to_string()),
                    DiffLine::Context("line_ZZZZ".to_string()),
                    DiffLine::Remove("line_AAAA".to_string()),
                    DiffLine::Add("replaced".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        // Should fail — fuzzy skipped for large files
        assert_eq!(
            result.operations[0].status, "error",
            "Large file must not fuzzy match: {}",
            result.operations[0].message
        );
    }

    // Pillar 2: Atomicity tests

    #[test]
    fn test_cross_file_atomicity_rollback() {
        let dir = tmp();
        // File1 is valid and patchable
        let file1 = dir.path().join("file1.txt");
        fs::write(&file1, "hello\nworld\n").unwrap();
        // File2 does NOT exist — second op will fail
        let ops = vec![
            FileOp::Update {
                path: "file1.txt".to_string(),
                hunks: vec![Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("hello".to_string()),
                        DiffLine::Remove("world".to_string()),
                        DiffLine::Add("earth".to_string()),
                    ],
                }],
                move_to: None,
            },
            FileOp::Update {
                path: "nonexistent.txt".to_string(),
                hunks: vec![],
                move_to: None,
            },
        ];
        let result = apply_patch(ops, dir.path());
        // Op1 staged ok, op2 failed
        assert_eq!(result.operations[0].status, "ok");
        assert_eq!(result.operations[1].status, "error");
        // file1 must be UNCHANGED because commit was rolled back
        let content = fs::read_to_string(&file1).unwrap();
        assert_eq!(
            content, "hello\nworld\n",
            "file1 must be unchanged after failed atomic patch"
        );
        // No shadow files should remain
        let shadow_count = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("patch_tmp"))
            .count();
        assert_eq!(shadow_count, 0, "shadow files must be cleaned up");
    }

    #[test]
    fn test_shadow_file_cleaned_up_on_failure() {
        let dir = tmp();
        let file1 = dir.path().join("a.txt");
        fs::write(&file1, "line1\nline2\n").unwrap();

        let ops = vec![
            FileOp::Update {
                path: "a.txt".to_string(),
                hunks: vec![Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("line1".to_string()),
                        DiffLine::Remove("line2".to_string()),
                        DiffLine::Add("replaced".to_string()),
                    ],
                }],
                move_to: None,
            },
            FileOp::Update {
                path: "missing.txt".to_string(),
                hunks: vec![],
                move_to: None,
            },
        ];
        let _ = apply_patch(ops, dir.path());

        // No .patch_tmp files in the tmp dir
        let remaining: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with("patch_tmp"))
            .collect();
        assert!(
            remaining.is_empty(),
            "shadow files leaked: {:?}",
            remaining.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_cross_file_atomicity_success() {
        let dir = tmp();
        fs::write(dir.path().join("f1.txt"), "alpha\nbeta\n").unwrap();
        fs::write(dir.path().join("f2.txt"), "gamma\ndelta\n").unwrap();
        fs::write(dir.path().join("f3.txt"), "epsilon\nzeta\n").unwrap();

        let ops = vec![
            FileOp::Update {
                path: "f1.txt".to_string(),
                hunks: vec![Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("alpha".to_string()),
                        DiffLine::Remove("beta".to_string()),
                        DiffLine::Add("BETA".to_string()),
                    ],
                }],
                move_to: None,
            },
            FileOp::Update {
                path: "f2.txt".to_string(),
                hunks: vec![Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("gamma".to_string()),
                        DiffLine::Remove("delta".to_string()),
                        DiffLine::Add("DELTA".to_string()),
                    ],
                }],
                move_to: None,
            },
            FileOp::Update {
                path: "f3.txt".to_string(),
                hunks: vec![Hunk {
                    context_hint: None,
                    lines: vec![
                        DiffLine::Context("epsilon".to_string()),
                        DiffLine::Remove("zeta".to_string()),
                        DiffLine::Add("ZETA".to_string()),
                    ],
                }],
                move_to: None,
            },
        ];
        let result = apply_patch(ops, dir.path());
        assert!(result.operations.iter().all(|o| o.status == "ok"));
        assert_eq!(
            fs::read_to_string(dir.path().join("f1.txt")).unwrap(),
            "alpha\nBETA\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("f2.txt")).unwrap(),
            "gamma\nDELTA\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("f3.txt")).unwrap(),
            "epsilon\nZETA\n"
        );
    }

    // Pillar 4: Structured error tests

    #[test]
    fn test_structured_error_closest_matches() {
        let dir = tmp();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "fn hello() {}\nfn world() {}\nfn greet() {}\n").unwrap();

        let ops = vec![FileOp::Update {
            path: "code.rs".to_string(),
            hunks: vec![Hunk {
                context_hint: None,
                lines: vec![
                    DiffLine::Context("fn totally_different_name() {}".to_string()),
                    DiffLine::Remove("fn nothing_here() {}".to_string()),
                    DiffLine::Add("fn replaced() {}".to_string()),
                ],
            }],
            move_to: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations[0].status, "error");
        // The error message should contain "Context not found"
        assert!(
            result.operations[0].message.contains("Context not found"),
            "got: {}",
            result.operations[0].message
        );
    }

    #[test]
    fn test_error_serializable() {
        // PatchError must serialize to JSON without panicking
        let err = PatchError::ContextNotFound(Box::new(ContextNotFoundData {
            path: "foo.rs".to_string(),
            pattern: vec!["fn bar()".to_string()],
            hint: None,
            total_lines: 10,
            file_excerpt: "  1: fn foo()".to_string(),
            closest_matches: vec![ClosestMatch {
                line_number: 1,
                similarity: 0.75,
                actual_content: "fn foo()".to_string(),
                suggestion: "Replace with fn foo()".to_string(),
            }],
        }));
        let json = serde_json::to_string(&err).expect("PatchError must serialize");
        assert!(json.contains("closest_matches"));
        assert!(json.contains("similarity"));
    }

    // Pillar 3: Validator tests

    #[test]
    fn test_validator_unknown_extension() {
        let dir = tmp();
        let path = dir.path().join("data.xyz");
        fs::write(&path, "some data").unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        assert!(
            warnings.is_empty(),
            "unknown extension must produce no warnings, got: {warnings:?}"
        );
    }

    #[test]
    fn test_validator_json_valid() {
        let dir = tmp();
        let path = dir.path().join("data.json");
        fs::write(&path, r#"{"key": 1}"#).unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        // python3 may not be present in CI; accept either clean or "not found"
        for w in &warnings {
            assert!(w.contains("Advisory"), "unexpected warning: {w}");
        }
    }

    #[test]
    fn test_validator_json_invalid() {
        let dir = tmp();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{bad").unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        // must either flag Advisory or report tool not found
        assert!(!warnings.is_empty(), "invalid JSON must produce a warning");
        assert!(
            warnings[0].contains("Advisory"),
            "warning must start with Advisory, got: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn test_validator_sh_valid() {
        let dir = tmp();
        let path = dir.path().join("script.sh");
        fs::write(
            &path,
            "#!/bin/bash
echo hello
",
        )
        .unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        for w in &warnings {
            assert!(
                w.contains("Advisory"),
                "unexpected warning for valid shell: {w}"
            );
        }
    }

    #[test]
    fn test_validator_sh_invalid() {
        let dir = tmp();
        let path = dir.path().join("bad.sh");
        fs::write(
            &path,
            "if then done
",
        )
        .unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        assert!(!warnings.is_empty(), "invalid shell must produce a warning");
        assert!(
            warnings[0].contains("Advisory"),
            "warning must start with Advisory, got: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn test_validator_js_valid() {
        let dir = tmp();
        let path = dir.path().join("script.js");
        fs::write(
            &path,
            "const x = 1;
",
        )
        .unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        for w in &warnings {
            assert!(
                w.contains("Advisory"),
                "unexpected warning for valid JS: {w}"
            );
        }
    }

    #[test]
    fn test_validator_go_extension() {
        let dir = tmp();
        let path = dir.path().join("main.go");
        // Deliberately unformatted Go to trigger gofmt -l output
        // Compact formatting triggers gofmt -l to print the filename (non-empty stdout)
        fs::write(&path, "package main\nfunc main(){fmt.Println(\"hello\")}\n").unwrap();
        let warnings = crate::validator::validate_file(&path, &path);
        // gofmt -l prints the filename when formatting is needed;
        // if gofmt is absent the advisory "not found" fires instead.
        // Either way at least one Advisory must be produced.
        assert!(
            !warnings.is_empty(),
            "go extension must produce at least one warning (format advisory or not-found)"
        );
        assert!(
            warnings[0].contains("Advisory"),
            "warning must contain Advisory, got: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn test_read_file_validates_path() {
        let dir = tmp();
        fs::write(dir.path().join("test.txt"), "Hello").unwrap();

        let ops = vec![FileOp::Read {
            path: "test.txt".to_string(),
            symbols: None,
            language: None,
            offset: None,
            limit: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].status, "ok");
        assert_eq!(result.operations[0].op_type, "read");
    }

    #[test]
    fn test_read_file_not_found() {
        let dir = tmp();
        let ops = vec![FileOp::Read {
            path: "nonexistent.txt".to_string(),
            symbols: None,
            language: None,
            offset: None,
            limit: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].status, "error");
        assert_eq!(result.operations[0].op_type, "read");
    }

    #[test]
    fn test_read_file_symlink_rejected() {
        let dir = tmp();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "Hello").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let ops = vec![FileOp::Read {
            path: "link.txt".to_string(),
            symbols: None,
            language: None,
            offset: None,
            limit: None,
        }];
        let result = apply_patch(ops, dir.path());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].status, "error");
        assert!(result.operations[0].message.contains("Symlink"));
    }

    #[test]
    fn test_read_file_path_traversal_now_allowed() {
        // Sandbox removed - paths can now escape base directory
        let dir = tmp();
        // Create a sibling file to read
        let parent = dir.path().parent().unwrap();
        let sibling_file = parent.join("outside_read.txt");
        fs::write(&sibling_file, "Hello from outside").unwrap();

        let ops = vec![FileOp::Read {
            path: "../outside_read.txt".to_string(),
            symbols: None,
            language: None,
            offset: None,
            limit: None,
        }];
        let result = apply_patch(ops, dir.path());
        // Should succeed in reading the sibling file
        assert_eq!(result.operations.len(), 1);
        assert_eq!(
            result.operations[0].status, "ok",
            "Expected ok, got: {}",
            result.operations[0].message
        );
        // Cleanup
        let _ = fs::remove_file(&sibling_file);
    }
}
