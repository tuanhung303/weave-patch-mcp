use std::fmt;

fn fmt_positions(positions: &[usize]) -> String {
    positions
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClosestMatch {
    pub line_number: usize,
    pub similarity: f32,
    pub actual_content: String,
    pub suggestion: String,
}
/// LLM-readable error output for structured error recovery.
/// Designed for programmatic consumption by AI agents.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LLMErrorOutput {
    /// File path where the error occurred
    pub file: String,
    /// Index of the failed hunk (0-based), if applicable
    pub failed_hunk: Option<usize>,
    /// Number of hunks successfully applied before failure
    pub applied_hunks: usize,
    /// Expected context pattern that was not found
    pub expected_context: Vec<String>,
    /// Actual file content at the nearest location
    pub actual_content: String,
    /// Similarity score of the closest match (0.0-1.0), if any
    pub similarity_score: Option<f32>,
    /// Suggested action for recovery (e.g., "re-read file", "adjust context")
    pub suggested_action: String,
    /// Human-readable recovery hint
    pub recovery_hint: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ContextNotFoundData {
    pub path: String,
    pub pattern: Vec<String>,
    pub hint: Option<String>,
    pub total_lines: usize,
    pub file_excerpt: String,
    pub closest_matches: Vec<ClosestMatch>,
}

#[derive(Debug, serde::Serialize)]
pub enum PatchError {
    Parse(String),
    FileNotFound(String),
    ContextNotFound(Box<ContextNotFoundData>),
    AmbiguousContext {
        path: String,
        count: usize,
        match_positions: Vec<usize>,
        context_at_each: Vec<String>,
    },
    Io(
        #[serde(serialize_with = "ser_io_error")]
        #[allow(dead_code)]
        std::io::Error,
    ),
    FileAlreadyExists(String),
    SymlinkRejected(String),
}

fn ser_io_error<S: serde::Serializer>(e: &std::io::Error, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&e.to_string())
}

impl PatchError {
    /// Convert error to LLM-readable JSON output.
    /// Provides structured information for programmatic error recovery.
    pub fn to_json(&self) -> LLMErrorOutput {
        match self {
            PatchError::ContextNotFound(data) => {
                let similarity = data.closest_matches.first().map(|m| m.similarity);
                let actual = data
                    .closest_matches
                    .first()
                    .map(|m| m.actual_content.clone())
                    .unwrap_or_else(|| data.file_excerpt.clone());
                let action = if similarity.map(|s| s >= 0.8).unwrap_or(false) {
                    "fuzzy_match_available".to_string()
                } else {
                    "re_read_file".to_string()
                };
                let hint = format!(
                    "File '{}' has {} lines. Check if context was modified or moved.",
                    data.path, data.total_lines
                );
                LLMErrorOutput {
                    file: data.path.clone(),
                    failed_hunk: None,
                    applied_hunks: 0,
                    expected_context: data.pattern.clone(),
                    actual_content: actual,
                    similarity_score: similarity,
                    suggested_action: action,
                    recovery_hint: hint,
                }
            }
            PatchError::AmbiguousContext {
                path,
                count,
                match_positions,
                context_at_each,
            } => LLMErrorOutput {
                file: path.clone(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![format!("Ambiguous: {} matches found", count)],
                actual_content: context_at_each.join("\n---\n"),
                similarity_score: None,
                suggested_action: "add_disambiguating_context_lines".to_string(),
                recovery_hint: format!(
                    "Add more context lines to disambiguate {} matches at lines: {}",
                    count,
                    match_positions
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            PatchError::FileNotFound(path) => LLMErrorOutput {
                file: path.clone(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![],
                actual_content: String::new(),
                similarity_score: None,
                suggested_action: "create_target_file_before_patch".to_string(),
                recovery_hint: format!(
                    "File '{}' does not exist. Create it first or check path.",
                    path
                ),
            },
            PatchError::FileAlreadyExists(path) => LLMErrorOutput {
                file: path.clone(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![],
                actual_content: String::new(),
                similarity_score: None,
                suggested_action: "use_update_operation_for_existing_file".to_string(),
                recovery_hint: format!(
                    "File '{}' already exists. Use Update instead of Add.",
                    path
                ),
            },
            PatchError::Parse(msg) => LLMErrorOutput {
                file: String::new(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![],
                actual_content: msg.clone(),
                similarity_score: None,
                suggested_action: "correct_syntax_in_patch_block".to_string(),
                recovery_hint: format!("Patch syntax error: {}", msg),
            },
            PatchError::SymlinkRejected(path) => LLMErrorOutput {
                file: path.clone(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![],
                actual_content: String::new(),
                similarity_score: None,
                suggested_action: "resolve_symlink".to_string(),
                recovery_hint: format!("Path '{}' is a symlink. Resolve to real path first.", path),
            },
            PatchError::Io(e) => LLMErrorOutput {
                file: String::new(),
                failed_hunk: None,
                applied_hunks: 0,
                expected_context: vec![],
                actual_content: e.to_string(),
                similarity_score: None,
                suggested_action: "check_permissions".to_string(),
                recovery_hint: format!("IO error: {}", e),
            },
        }
    }
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::Parse(msg) => write!(f, "parse error: {msg}"),
            PatchError::FileNotFound(msg) => write!(f, "file not found: {msg}"),
            PatchError::ContextNotFound(d) => {
                writeln!(f, "context not found: {}", d.path)?;
                writeln!(
                    f,
                    "  file has {} lines, expected {} line pattern",
                    d.total_lines,
                    d.pattern.len()
                )?;
                if let Some(ref hint) = d.hint {
                    writeln!(f, "  Hint attempted: {}", hint)?;
                }
                if !d.file_excerpt.is_empty() {
                    writeln!(f, "  Nearest file content:")?;
                    for line in d.file_excerpt.lines().take(3) {
                        writeln!(f, "    {}", line)?;
                    }
                }
                Ok(())
            }
            PatchError::AmbiguousContext {
                path,
                count,
                match_positions,
                context_at_each: _,
            } => {
                let positions_str = fmt_positions(match_positions);
                write!(
                    f,
                    "ambiguous context: {} ({} matches at lines: {})",
                    path, count, positions_str
                )
            }
            PatchError::Io(e) => write!(f, "io error: {e}"),
            PatchError::FileAlreadyExists(msg) => write!(f, "file already exists: {msg}"),
            PatchError::SymlinkRejected(msg) => write!(f, "symlink rejected: {msg}"),
        }
    }
}

impl std::error::Error for PatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PatchError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PatchError {
    fn from(e: std::io::Error) -> Self {
        PatchError::Io(e)
    }
}
