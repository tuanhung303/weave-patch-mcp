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
    PathTraversal(String),
    SymlinkRejected(String),
}

fn ser_io_error<S: serde::Serializer>(e: &std::io::Error, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&e.to_string())
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatchError::Parse(msg) => write!(f, "Parse error: {msg}"),
            PatchError::FileNotFound(msg) => write!(f, "File not found: {msg}"),
            PatchError::ContextNotFound(d) => {
                let pattern_len = d.pattern.len();
                let pattern_preview = d
                    .pattern
                    .iter()
                    .take(5)
                    .map(|l| l.trim_end())
                    .collect::<Vec<_>>()
                    .join("\n  ");
                let truncated = if d.pattern.len() > 5 {
                    "\n  ... (truncated)"
                } else {
                    ""
                };
                let hint_info = match &d.hint {
                    Some(h) => format!("Hint attempted: {h}"),
                    None => "No hint provided".to_string(),
                };
                let closest_info = if d.closest_matches.is_empty() {
                    String::new()
                } else {
                    let items: Vec<String> = d
                        .closest_matches
                        .iter()
                        .map(|m| {
                            format!(
                                "  line {}: {:.0}% — {}",
                                m.line_number,
                                m.similarity * 100.0,
                                m.actual_content.trim_end()
                            )
                        })
                        .collect();
                    format!("\nClosest matches:\n{}", items.join("\n"))
                };
                write!(
                    f,
                    "Context not found in {path} (file has {total_lines} lines)\n\
                     Expected pattern ({pattern_len} lines):\n  {pattern_preview}{truncated}\n\
                     {hint_info}\n\
                     Nearest file content:\n{file_excerpt}{closest_info}",
                    path = d.path,
                    total_lines = d.total_lines,
                    file_excerpt = d.file_excerpt,
                )
            }
            PatchError::AmbiguousContext {
                path,
                count,
                match_positions,
                context_at_each,
            } => {
                let positions_str = fmt_positions(match_positions);
                let context_preview = context_at_each
                    .iter()
                    .enumerate()
                    .map(|(i, ctx)| {
                        format!(
                            "  Position {} (line {})\n{}",
                            i + 1,
                            match_positions[i],
                            ctx
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                write!(
                    f,
                    "Ambiguous context in {path}: matched {count} locations at lines: {positions_str}\n\
                     Content at each position:\n{context_preview}"
                )
            }
            PatchError::Io(e) => write!(f, "IO error: {e}"),
            PatchError::FileAlreadyExists(msg) => write!(f, "File already exists: {msg}"),
            PatchError::PathTraversal(msg) => write!(f, "Path traversal rejected: {msg}"),
            PatchError::SymlinkRejected(msg) => write!(f, "Symlink target rejected: {msg}"),
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
