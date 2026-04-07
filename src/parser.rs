use crate::error::PatchError;

#[derive(Debug, PartialEq)]
pub enum DiffLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Debug, PartialEq)]
pub struct Hunk {
    pub context_hint: Option<String>,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, PartialEq)]
pub enum FileOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        hunks: Vec<Hunk>,
        move_to: Option<String>,
    },
    Read {
        path: String,
        symbols: Option<Vec<String>>,
        language: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    Map {
        path: String,
        depth: Option<usize>,
        output_limit: Option<usize>,
    },
}

/// Result of parsing a patch with optional threshold from begin marker.
#[derive(Debug)]
pub struct ParseResult {
    pub ops: Vec<FileOp>,
    pub threshold: Option<f32>,
}

pub fn parse_patch(input: &str) -> Result<ParseResult, PatchError> {
    // Auto-wrap missing begin/end markers for forgiving parsing.
    let has_begin = input.lines().any(|l| l.trim().starts_with("=== begin"));
    let has_end = input.lines().any(|l| l.trim() == "=== end");
    let has_ops = input.lines().any(|l| {
        l.starts_with("create ")
            || l.starts_with("update ")
            || l.starts_with("delete ")
            || l.starts_with("read ")
            || l.starts_with("map ")
    });

    let input = if !has_begin && has_ops {
        format!("=== begin\n{input}")
    } else {
        input.to_string()
    };
    let input = if !has_end {
        format!("{input}\n=== end")
    } else {
        input
    };

    let lines: Vec<&str> = input.lines().collect();

    // Find begin/end boundaries and extract threshold
    let begin_line = lines
        .iter()
        .find(|l| l.trim().starts_with("=== begin"))
        .ok_or_else(|| PatchError::Parse("Missing '=== begin' marker".to_string()))?;

    let begin_idx = lines
        .iter()
        .position(|l| l.trim().starts_with("=== begin"))
        .ok_or_else(|| PatchError::Parse("Missing '=== begin' marker".to_string()))?;

    let end_idx = lines
        .iter()
        .position(|l| l.trim() == "=== end")
        .ok_or_else(|| PatchError::Parse("Missing '=== end' marker".to_string()))?;

    if end_idx <= begin_idx {
        return Err(PatchError::Parse(
            "'=== end' must come after '=== begin'".to_string(),
        ));
    }

    // Parse threshold from begin line: "=== begin threshold=0.95"
    let threshold = parse_threshold_from_begin(begin_line)?;

    let body_lines = &lines[begin_idx + 1..end_idx];

    // Split into file sections
    let mut ops: Vec<FileOp> = Vec::new();

    // Find indices of file operation headers
    let file_op_indices: Vec<usize> = body_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.starts_with("create ")
                || l.starts_with("update ")
                || l.starts_with("delete ")
                || l.starts_with("read ")
                || l.starts_with("map ")
        })
        .map(|(i, _)| i)
        .collect();

    for (section_idx, &op_start) in file_op_indices.iter().enumerate() {
        let op_end = if section_idx + 1 < file_op_indices.len() {
            file_op_indices[section_idx + 1]
        } else {
            body_lines.len()
        };

        let header = body_lines[op_start];
        let section = &body_lines[op_start + 1..op_end];

        if let Some(path) = header.strip_prefix("create ") {
            let path = path.trim().to_string();
            let content = parse_add_content(section);
            ops.push(FileOp::Add { path, content });
        } else if let Some(path) = header.strip_prefix("delete ") {
            let path = path.trim().to_string();
            ops.push(FileOp::Delete { path });
        } else if let Some(path) = header.strip_prefix("update ") {
            let path = path.trim().to_string();
            let (hunks, move_to) = parse_update_section(section)?;
            ops.push(FileOp::Update {
                path,
                hunks,
                move_to,
            });
        } else if let Some(read_spec) = header.strip_prefix("read ") {
            let spec = parse_read_spec(read_spec);
            ops.push(FileOp::Read {
                path: spec.path,
                symbols: spec.symbols,
                language: spec.language,
                offset: spec.offset,
                limit: spec.limit,
            });
        } else if let Some(map_spec) = header.strip_prefix("map ") {
            let spec = parse_map_spec(map_spec);
            ops.push(FileOp::Map {
                path: spec.path,
                depth: spec.depth,
                output_limit: spec.output_limit,
            });
        }
    }

    Ok(ParseResult { ops, threshold })
}

/// Parse threshold from begin line: "=== begin" or "=== begin threshold=0.95"
fn parse_threshold_from_begin(line: &str) -> Result<Option<f32>, PatchError> {
    let line = line.trim();
    // Strip "=== begin" prefix
    let rest = line
        .strip_prefix("=== begin")
        .ok_or_else(|| PatchError::Parse("Invalid begin marker".to_string()))?;

    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(None);
    }

    // Look for threshold=X
    for part in rest.split_whitespace() {
        if let Some(val) = part.strip_prefix("threshold=") {
            let parsed: f32 = val
                .parse()
                .map_err(|_| PatchError::Parse(format!("Invalid threshold value: '{val}'")))?;
            if !(0.0..=1.0).contains(&parsed) {
                return Err(PatchError::Parse(format!(
                    "Threshold must be between 0.0 and 1.0, got {parsed}"
                )));
            }
            return Ok(Some(parsed));
        }
    }

    Ok(None)
}

pub struct MapSpec {
    pub path: String,
    pub depth: Option<usize>,
    pub output_limit: Option<usize>,
}

fn parse_map_spec(spec: &str) -> MapSpec {
    let parts: Vec<&str> = spec.split_whitespace().collect();
    let path = parts[0].to_string();

    let mut depth: Option<usize> = None;
    let mut output_limit: Option<usize> = None;

    for part in &parts[1..] {
        if let Some(val) = part.strip_prefix("depth=") {
            depth = val.parse::<usize>().ok();
        } else if let Some(val) = part.strip_prefix("limit=") {
            output_limit = val.parse::<usize>().ok();
        }
    }

    MapSpec {
        path,
        depth,
        output_limit,
    }
}

fn parse_add_content(lines: &[&str]) -> String {
    // Trim trailing empty lines (artifacts from input ending with \n)
    let lines = match lines.iter().rposition(|l| !l.is_empty()) {
        Some(idx) => &lines[..=idx],
        None => return String::new(),
    };

    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

/// Parsed read file specification from `read` header.
pub struct ReadSpec {
    pub path: String,
    pub symbols: Option<Vec<String>>,
    pub language: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Parse a Read specification: "read <path> [symbols=a,b] [language=rust] [offset=0] [limit=100]"
fn parse_read_spec(spec: &str) -> ReadSpec {
    // Split by whitespace to separate path from key=value params
    let parts: Vec<&str> = spec.split_whitespace().collect();
    let path = parts[0].to_string();

    let mut symbols: Option<Vec<String>> = None;
    let mut language: Option<String> = None;
    let mut offset: Option<usize> = None;
    let mut limit: Option<usize> = None;

    for part in &parts[1..] {
        if let Some(kv) = part.strip_prefix("symbols=") {
            symbols = Some(kv.split(',').map(|s| s.to_string()).collect());
        } else if let Some(val) = part.strip_prefix("language=") {
            language = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("offset=")
            && let Ok(n) = val.parse::<usize>()
        {
            offset = Some(n);
        } else if let Some(val) = part.strip_prefix("limit=")
            && let Ok(n) = val.parse::<usize>()
        {
            limit = Some(n);
        }
    }

    ReadSpec {
        path,
        symbols,
        language,
        offset,
        limit,
    }
}

fn parse_update_section(lines: &[&str]) -> Result<(Vec<Hunk>, Option<String>), PatchError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut move_to: Option<String> = None;
    let mut current_hunk: Option<Hunk> = None;

    for &line in lines {
        // Move directive: "move_to dest/path.rs"
        if let Some(dest) = line.strip_prefix("move_to ") {
            move_to = Some(dest.trim().to_string());
            continue;
        }

        if line.trim() == "*** End of File" {
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }
            continue;
        }

        // Accept @@ with optional leading whitespace
        let trimmed = line.trim();
        if let Some(hint_part) = trimmed.strip_prefix("@@") {
            // Flush current hunk
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }
            let hint = hint_part.trim();
            let context_hint = if hint.is_empty() {
                None
            } else {
                Some(hint.to_string())
            };
            current_hunk = Some(Hunk {
                context_hint,
                lines: Vec::new(),
            });
            continue;
        }

        // Parse diff lines - check for +/- FIRST before context
        // Order matters: check prefixed +/- before plain context
        let diff_line = if let Some(rest) = line.strip_prefix("  -") {
            // Two-space then minus (indented remove)
            Some(DiffLine::Remove(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix("  +") {
            // Two-space then plus (indented add)
            Some(DiffLine::Add(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix('-') {
            // Plain remove
            Some(DiffLine::Remove(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix('+') {
            // Plain add
            Some(DiffLine::Add(rest.to_string()))
        } else if let Some(rest) = line.strip_prefix("  ") {
            // Two-space prefix (context)
            Some(DiffLine::Context(rest.to_string()))
        } else if line.starts_with(' ') && line.len() > 1 {
            // Single space prefix
            Some(DiffLine::Context(line[1..].to_string()))
        } else if line == " " || line == "  " {
            // Empty context line
            Some(DiffLine::Context(String::new()))
        } else {
            None
        };

        if let Some(dl) = diff_line {
            if current_hunk.is_none() {
                // No @@ seen yet — create implicit hunk
                current_hunk = Some(Hunk {
                    context_hint: None,
                    lines: Vec::new(),
                });
            }
            if let Some(ref mut hunk) = current_hunk {
                hunk.lines.push(dl);
            }
        }
    }

    // Flush last hunk
    if let Some(hunk) = current_hunk.take() {
        hunks.push(hunk);
    }

    Ok((hunks, move_to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_add_file() {
        let input =
            "=== begin\ncreate src/hello.rs\nfn hello() {\n    println!(\"Hello\");\n}\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        assert!(result.threshold.is_none());
        match &result.ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "src/hello.rs");
                assert_eq!(content, "fn hello() {\n    println!(\"Hello\");\n}\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_add_file_indented() {
        let input =
            "=== begin\ncreate src/hello.rs\nfn hello() {\n    println!(\"Hello\");\n}\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "src/hello.rs");
                assert_eq!(content, "fn hello() {\n    println!(\"Hello\");\n}\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_delete_file() {
        let input = "=== begin\ndelete src/old.rs\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Delete { path } => assert_eq!(path, "src/old.rs"),
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_parse_update_single_hunk() {
        let input =
            "=== begin\nupdate src/lib.rs\n@@\n old_line\n-remove_me\n+add_me\n new_line\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Update {
                path,
                hunks,
                move_to,
            } => {
                assert_eq!(path, "src/lib.rs");
                assert!(move_to.is_none());
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].context_hint, None);
                assert_eq!(hunks[0].lines.len(), 4);
                assert_eq!(hunks[0].lines[0], DiffLine::Context("old_line".to_string()));
                assert_eq!(hunks[0].lines[1], DiffLine::Remove("remove_me".to_string()));
                assert_eq!(hunks[0].lines[2], DiffLine::Add("add_me".to_string()));
                assert_eq!(hunks[0].lines[3], DiffLine::Context("new_line".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_update_multiple_hunks() {
        let input = "=== begin\nupdate src/lib.rs\n@@\n ctx1\n-old1\n+new1\n@@ second\n ctx2\n-old2\n+new2\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks.len(), 2);
                assert_eq!(hunks[0].context_hint, None);
                assert_eq!(hunks[1].context_hint, Some("second".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_update_with_context_hint() {
        let input = "=== begin\nupdate src/lib.rs\n@@ impl Server\n pub fn handle(&self) {\n-    old()\n  +    new()\n }\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("impl Server".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_multi_file_patch() {
        let input = "=== begin\ncreate a.rs\n+content\ndelete b.rs\nupdate c.rs\n@@\n ctx\n-old\n+new\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 3);
        assert!(matches!(result.ops[0], FileOp::Add { .. }));
        assert!(matches!(result.ops[1], FileOp::Delete { .. }));
        assert!(matches!(result.ops[2], FileOp::Update { .. }));
    }

    #[test]
    fn test_parse_update_with_move_to() {
        let input =
            "=== begin\nupdate src/old.rs\nmove_to src/new.rs\n@@\n ctx\n-old\n+new\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { path, move_to, .. } => {
                assert_eq!(path, "src/old.rs");
                assert_eq!(move_to.as_deref(), Some("src/new.rs"));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_error_missing_begin_no_ops() {
        // Input has no begin and no operation headers — still an error
        let input = "some content\n=== end";
        let result = parse_patch(input);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("begin"));
    }

    #[test]
    fn test_parse_hint_with_class() {
        let input = "=== begin\nupdate src/lib.rs\n@@ class Server\n pub struct Server {\n-    old_field: i32,\n    new_field: i32,\n }\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("class Server".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_hint_with_punctuation() {
        let input = "=== begin\nupdate src/lib.rs\n@@ fn main():\n fn main() {\n-    old()\n  +    new()\n }\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("fn main():".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_indented_context() {
        let input = "=== begin\nupdate script.py\n@@\n def hello():\n     print(\"hi\")\n-    old_call()\n    new_call()\n     return\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                let ctx = &hunks[0].lines[1];
                assert!(matches!(ctx, DiffLine::Context(s) if s.contains("print")));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_read_file_basic() {
        let input = "=== begin\nread src/main.rs\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Read {
                path,
                symbols,
                language,
                offset,
                limit,
            } => {
                assert_eq!(path, "src/main.rs");
                assert!(symbols.is_none());
                assert!(language.is_none());
                assert!(offset.is_none());
                assert!(limit.is_none());
            }
            _ => panic!("Expected Read"),
        }
    }

    #[test]
    fn test_parse_read_file_with_symbols() {
        let input = "=== begin\nread src/lib.rs symbols=Server,handle language=rust\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Read {
                path,
                symbols,
                language,
                ..
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(
                    symbols,
                    &Some(vec!["Server".to_string(), "handle".to_string()])
                );
                assert_eq!(language, &Some("rust".to_string()));
            }
            _ => panic!("Expected Read"),
        }
    }

    #[test]
    fn test_parse_read_file_with_offset_limit() {
        let input = "=== begin\nread config.py offset=10 limit=50\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Read {
                path,
                offset,
                limit,
                ..
            } => {
                assert_eq!(path, "config.py");
                assert_eq!(offset, &Some(10));
                assert_eq!(limit, &Some(50));
            }
            _ => panic!("Expected Read"),
        }
    }

    #[test]
    fn test_parse_read_file_mixed_with_operations() {
        let input = "=== begin\nread src/main.rs\nupdate src/lib.rs\n@@\n ctx\n-old\n+new\ndelete old.rs\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 3);
        assert!(matches!(result.ops[0], FileOp::Read { .. }));
        assert!(matches!(result.ops[1], FileOp::Update { .. }));
        assert!(matches!(result.ops[2], FileOp::Delete { .. }));
    }

    #[test]
    fn test_parse_auto_wrap_missing_begin() {
        // Input has end but NOT begin — should still work
        let input = "create test.txt\nhello\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "hello\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_auto_wrap_missing_end() {
        // Input has begin but NOT end — should still work
        let input = "=== begin\ndelete old.txt";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Delete { path } => assert_eq!(path, "old.txt"),
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_parse_auto_wrap_both_missing() {
        // Input has NEITHER marker, just raw ops — should still work
        let input = "create test.txt\nhello\n";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "hello\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_threshold_valid() {
        let input = "=== begin threshold=0.95\ncreate test.txt\nhello\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.threshold, Some(0.95));
    }

    #[test]
    fn test_parse_threshold_missing() {
        let input = "=== begin\ncreate test.txt\nhello\n=== end";
        let result = parse_patch(input).unwrap();
        assert!(result.threshold.is_none());
    }

    #[test]
    fn test_parse_threshold_invalid_value() {
        let input = "=== begin threshold=invalid\ncreate test.txt\nhello\n=== end";
        let result = parse_patch(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid threshold"));
    }

    #[test]
    fn test_parse_threshold_out_of_range() {
        let input = "=== begin threshold=1.5\ncreate test.txt\nhello\n=== end";
        let result = parse_patch(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("between 0.0 and 1.0"));
    }

    #[test]
    fn test_parse_map_directory() {
        let input = "=== begin\nmap src/ depth=2\n=== end";
        let result = parse_patch(input).unwrap();
        assert_eq!(result.ops.len(), 1);
        match &result.ops[0] {
            FileOp::Map {
                path,
                depth,
                output_limit,
            } => {
                assert_eq!(path, "src/");
                assert_eq!(depth, &Some(2));
                assert!(output_limit.is_none());
            }
            _ => panic!("Expected Map"),
        }
    }

    #[test]
    fn test_parse_update_indented_diff() {
        // Test with 2-space indented diff lines
        let input = "=== begin\nupdate src/lib.rs\n  @@ fn main\n   fn main() {\n  -    old()\n  +    new()\n   }\n=== end";
        let result = parse_patch(input).unwrap();
        match &result.ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("fn main".to_string()));
                // Check that indented lines are parsed correctly
                assert_eq!(hunks[0].lines.len(), 4);
                // Line 0: "  fn main() {" -> Context(" fn main() {")
                assert!(matches!(&hunks[0].lines[0], DiffLine::Context(s) if s == " fn main() {"));
                // Line 1: "  -    old()" -> Remove("    old()")
                assert!(matches!(&hunks[0].lines[1], DiffLine::Remove(s) if s == "    old()"));
                // Line 2: "  +    new()" -> Add("    new()")
                assert!(matches!(&hunks[0].lines[2], DiffLine::Add(s) if s == "    new()"));
                // Line 3: "   }" -> Context(" }")
                assert!(matches!(&hunks[0].lines[3], DiffLine::Context(s) if s == " }"));
            }
            _ => panic!("Expected Update"),
        }
    }
}
