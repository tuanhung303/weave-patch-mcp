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

pub fn parse_patch(input: &str) -> Result<Vec<FileOp>, PatchError> {
    // Auto-wrap missing Begin/End Patch markers for forgiving parsing.
    let has_begin = input.lines().any(|l| l.trim() == "*** Begin Patch");
    let has_end = input.lines().any(|l| l.trim() == "*** End Patch");
    let has_ops = input.lines().any(|l| {
        l.starts_with("*** Add File: ")
            || l.starts_with("*** Update File: ")
            || l.starts_with("*** Delete File: ")
            || l.starts_with("*** Read File: ")
            || l.starts_with("*** Map Directory: ")
    });

    let input = if !has_begin && has_ops {
        format!("*** Begin Patch\n{input}")
    } else {
        input.to_string()
    };
    let input = if !has_end {
        format!("{input}\n*** End Patch")
    } else {
        input
    };

    let lines: Vec<&str> = input.lines().collect();

    // Find Begin/End Patch boundaries
    let begin_idx = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or_else(|| PatchError::Parse("Missing '*** Begin Patch' marker".to_string()))?;

    let end_idx = lines
        .iter()
        .position(|l| l.trim() == "*** End Patch")
        .ok_or_else(|| PatchError::Parse("Missing '*** End Patch' marker".to_string()))?;

    if end_idx <= begin_idx {
        return Err(PatchError::Parse(
            "'*** End Patch' must come after '*** Begin Patch'".to_string(),
        ));
    }

    let body_lines = &lines[begin_idx + 1..end_idx];

    // Split into file sections
    let mut ops: Vec<FileOp> = Vec::new();

    // Find indices of file operation headers
    let file_op_indices: Vec<usize> = body_lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.starts_with("*** Add File: ")
                || l.starts_with("*** Update File: ")
                || l.starts_with("*** Delete File: ")
                || l.starts_with("*** Read File: ")
                || l.starts_with("*** Map Directory: ")
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

        if let Some(path) = header.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            let content = parse_add_content(section);
            ops.push(FileOp::Add { path, content });
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            let path = path.trim().to_string();
            ops.push(FileOp::Delete { path });
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            let (hunks, move_to) = parse_update_section(section)?;
            ops.push(FileOp::Update {
                path,
                hunks,
                move_to,
            });
        } else if let Some(read_spec) = header.strip_prefix("*** Read File: ") {
            let spec = parse_read_spec(read_spec);
            ops.push(FileOp::Read {
                path: spec.path,
                symbols: spec.symbols,
                language: spec.language,
                offset: spec.offset,
                limit: spec.limit,
            });
        } else if let Some(map_spec) = header.strip_prefix("*** Map Directory: ") {
            let spec = parse_map_spec(map_spec);
            ops.push(FileOp::Map {
                path: spec.path,
                depth: spec.depth,
                output_limit: spec.output_limit,
            });
        }
    }

    Ok(ops)
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
    let mut content_lines: Vec<String> = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix('+') {
            content_lines.push(rest.to_string());
        }
    }
    if content_lines.is_empty() {
        String::new()
    } else {
        content_lines.join("\n") + "\n"
    }
}

/// Parsed read file specification from `*** Read File:` header.
pub struct ReadSpec {
    pub path: String,
    pub symbols: Option<Vec<String>>,
    pub language: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Parse a Read File specification: "*** Read File: <path> [symbols=a,b] [language=rust] [offset=0] [limit=100]"
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
        if let Some(dest) = line.strip_prefix("*** Move to: ") {
            move_to = Some(dest.trim().to_string());
            continue;
        }

        if line.trim() == "*** End of File" {
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }
            continue;
        }

        if let Some(hint_part) = line.strip_prefix("@@") {
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

        // Parse diff lines
        let diff_line = if let Some(rest) = line.strip_prefix("  ") {
            // Space prefix (two spaces — one for the format, one for the actual content)
            Some(DiffLine::Context(rest.to_string()))
        } else if line.starts_with(' ') && line.len() > 1 {
            // Single space prefix
            Some(DiffLine::Context(line[1..].to_string()))
        } else if line == " " || line == "  " {
            // Empty context line
            Some(DiffLine::Context(String::new()))
        } else if let Some(rest) = line.strip_prefix('-') {
            Some(DiffLine::Remove(rest.to_string()))
        } else {
            line.strip_prefix('+')
                .map(|rest| DiffLine::Add(rest.to_string()))
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
        let input = "*** Begin Patch\n*** Add File: src/hello.rs\n+fn hello() {\n+    println!(\"Hello\");\n+}\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "src/hello.rs");
                assert_eq!(content, "fn hello() {\n    println!(\"Hello\");\n}\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_delete_file() {
        let input = "*** Begin Patch\n*** Delete File: src/old.rs\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Delete { path } => assert_eq!(path, "src/old.rs"),
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_parse_update_single_hunk() {
        let input = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n old_line\n-remove_me\n+add_me\n new_line\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
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
        let input = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n ctx1\n-old1\n+new1\n@@ second\n ctx2\n-old2\n+new2\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
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
        let input = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ impl Server\n pub fn handle(&self) {\n-    old()\n+    new()\n }\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("impl Server".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_multi_file_patch() {
        let input = "*** Begin Patch\n*** Add File: a.rs\n+content\n*** Delete File: b.rs\n*** Update File: c.rs\n@@\n ctx\n-old\n+new\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], FileOp::Add { .. }));
        assert!(matches!(ops[1], FileOp::Delete { .. }));
        assert!(matches!(ops[2], FileOp::Update { .. }));
    }

    #[test]
    fn test_parse_update_with_move_to() {
        let input = "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n ctx\n-old\n+new\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
            FileOp::Update { path, move_to, .. } => {
                assert_eq!(path, "src/old.rs");
                assert_eq!(move_to.as_deref(), Some("src/new.rs"));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_error_missing_begin_patch_no_ops() {
        // Input has no Begin Patch and no operation headers — still an error
        let input = "some content\n*** End Patch";
        let result = parse_patch(input);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Begin Patch"));
    }

    #[test]
    fn test_parse_hint_with_class() {
        let input = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ class Server\n pub struct Server {\n-    old_field: i32,\n+    new_field: i32,\n }\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("class Server".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_hint_with_punctuation() {
        let input = "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main():\n fn main() {\n-    old()\n+    new()\n }\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks[0].context_hint, Some("fn main():".to_string()));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_indented_context() {
        let input = "*** Begin Patch\n*** Update File: script.py\n@@\n def hello():\n     print(\"hi\")\n-    old_call()\n+    new_call()\n     return\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                let ctx = &hunks[0].lines[1];
                assert!(matches!(ctx, DiffLine::Context(s) if s.contains("print")));
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn test_parse_read_file_basic() {
        let input = "*** Begin Patch\n*** Read File: src/main.rs\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
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
        let input = "*** Begin Patch\n*** Read File: src/lib.rs symbols=Server,handle language=rust\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
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
        let input = "*** Begin Patch\n*** Read File: config.py offset=10 limit=50\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        match &ops[0] {
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
        let input = "*** Begin Patch\n*** Read File: src/main.rs\n*** Update File: src/lib.rs\n@@\n ctx\n-old\n+new\n*** Delete File: old.rs\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], FileOp::Read { .. }));
        assert!(matches!(ops[1], FileOp::Update { .. }));
        assert!(matches!(ops[2], FileOp::Delete { .. }));
    }

    #[test]
    fn test_parse_auto_wrap_missing_begin() {
        // Input has *** End Patch but NOT *** Begin Patch — should still work
        let input = "*** Add File: test.txt\n+hello\n*** End Patch";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "hello\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn test_parse_auto_wrap_missing_end() {
        // Input has *** Begin Patch but NOT *** End Patch — should still work
        let input = "*** Begin Patch\n*** Delete File: old.txt";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Delete { path } => assert_eq!(path, "old.txt"),
            _ => panic!("Expected Delete"),
        }
    }

    #[test]
    fn test_parse_auto_wrap_both_missing() {
        // Input has NEITHER marker, just raw ops — should still work
        let input = "*** Add File: test.txt\n+hello\n";
        let ops = parse_patch(input).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            FileOp::Add { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "hello\n");
            }
            _ => panic!("Expected Add"),
        }
    }
}
