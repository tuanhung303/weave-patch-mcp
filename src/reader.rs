//! reader.rs — Pure utility functions for workspace file reading.
//!
//! Three public functions:
//!   * `expand_globs`    — resolve a glob pattern to a sorted list of relative paths
//!   * `apply_line_range` — slice file content by offset/limit
//!   * `extract_symbols` — extract named symbols using regex + brace/indent counting

use std::path::Path;

use glob::glob;
use regex::Regex;

use crate::applier;

// ---------------------------------------------------------------------------
// 1.  expand_globs
// ---------------------------------------------------------------------------

/// Expand a glob pattern relative to `base_dir`.
///
/// Returns a sorted `Vec<String>` of relative paths (forward-slash separated).
/// Symlinks and directories are excluded. Paths failing security validation are
/// silently skipped. Returns `Ok(vec![])` when nothing matches.
pub fn expand_globs(base_dir: &Path, pattern: &str) -> Result<Vec<String>, String> {
    // Canonicalize base first — macOS routes /var → /private/var
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|e| format!("Cannot canonicalize base dir: {e}"))?;

    let glob_pattern = canonical_base.join(pattern);
    let pattern_str = glob_pattern.to_string_lossy();

    let entries = glob(&pattern_str).map_err(|e| format!("Invalid glob pattern: {e}"))?;

    let mut paths: Vec<String> = Vec::new();

    for entry in entries {
        let matched = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Reject symlinks
        if let Ok(meta) = matched.symlink_metadata() {
            if meta.file_type().is_symlink() {
                continue;
            }
            // Reject directories
            if meta.is_dir() {
                continue;
            }
        } else {
            continue;
        }

        // Security: validate path is within base
        let canonical_match = match matched.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if !canonical_match.starts_with(&canonical_base) {
            continue;
        }

        // Validate via existing applier logic using the relative path
        let rel = match canonical_match.strip_prefix(&canonical_base) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Re-validate through applier (covers traversal checks)
        if applier::validate_path(&canonical_base, &rel_str).is_err() {
            continue;
        }

        paths.push(rel_str);
    }

    paths.sort();
    Ok(paths)
}

// ---------------------------------------------------------------------------
// 2.  apply_line_range
// ---------------------------------------------------------------------------

/// Slice `content` by `offset` (0-based) and `limit`.
///
/// Returns `(sliced_content, start_line_1based, end_line_1based)`.
///
/// * Both `None` → full content, `(1, total_lines)`.
/// * `offset` beyond EOF → `("", offset+1, offset+1)`.
pub fn apply_line_range(
    content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> (String, usize, usize) {
    let all_lines: Vec<&str> = content.split('\n').collect();
    let total = all_lines.len();

    let start_idx = offset.unwrap_or(0);

    if start_idx >= total {
        return (String::new(), start_idx + 1, start_idx + 1);
    }

    let remaining = &all_lines[start_idx..];
    let taken: Vec<&str> = match limit {
        Some(n) => remaining.iter().take(n).copied().collect(),
        None => remaining.to_vec(),
    };

    let end_idx = start_idx + taken.len().saturating_sub(1);
    let sliced = taken.join("\n");
    (sliced, start_idx + 1, end_idx + 1)
}

// ---------------------------------------------------------------------------
// 3.  extract_symbols
// ---------------------------------------------------------------------------

/// Extract named symbols from `content` using `language`-specific opener regexes.
///
/// Returns one block per symbol, separated by `\n\n`:
/// ```text
/// // symbol: foo
/// fn foo() { ... }
///
/// // symbol bar: NOT FOUND
/// ```
pub fn extract_symbols(content: &str, language: &str, symbols: &[String]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut blocks: Vec<String> = Vec::new();

    for sym in symbols {
        let extracted = extract_one_symbol(&lines, language, sym);
        blocks.push(extracted);
    }

    blocks.join("\n\n")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn extract_one_symbol(lines: &[&str], language: &str, name: &str) -> String {
    let openers = build_opener_patterns(language, name);

    // For each line, check if any opener matches (with 5-line lookahead)
    let total = lines.len();

    for start in 0..total {
        // Build a 2-line lookahead buffer for multi-line signatures.
        // A 5-line window causes false positives when adjacent functions
        // appear within the lookahead range of each other.
        let lookahead_end = (start + 2).min(total);
        let window = lines[start..lookahead_end].join("\n");

        let matched = openers.iter().any(|re| re.is_match(&window));
        if !matched {
            continue;
        }

        // Found the opener at `start`. Now collect the body.
        let extracted = if language == "python" {
            collect_python_block(lines, start)
        } else {
            collect_brace_block(lines, start)
        };

        return format!("// symbol: {}\n{}", name, extracted);
    }

    format!("// symbol {}: NOT FOUND", name)
}

/// Build opener regex patterns for a given language and symbol name.
fn build_opener_patterns(language: &str, name: &str) -> Vec<Regex> {
    let escaped = regex::escape(name);

    let raw_patterns: &[&str] = match language {
        "rust" => &[
            &format!(r"(?m)(pub\s+)?(async\s+)?fn\s+{escaped}\s*[(<]"),
            &format!(r"(?m)(pub\s+)?struct\s+{escaped}\s*[\{{<]"),
            &format!(r"(?m)(pub\s+)?impl(\s+\S+\s+for)?\s+{escaped}\s*[\{{<]"),
            &format!(r"(?m)(pub\s+)?impl\s+\S+\s+for\s+{escaped}\s*\{{"),
            &format!(r"(?m)(pub\s+)?enum\s+{escaped}\s*[\{{<]"),
            &format!(r"(?m)(pub\s+)?type\s+{escaped}\s*="),
            &format!(r"(?m)(pub\s+)?trait\s+{escaped}\s*\{{"),
        ],
        "python" => &[
            &format!(r"(?m)(async\s+)?def\s+{escaped}\s*\("),
            &format!(r"(?m)class\s+{escaped}\s*[:(]"),
        ],
        "typescript" | "javascript" => &[
            &format!(r"(?m)(export\s+)?(async\s+)?function\s+{escaped}\s*[\(<]"),
            &format!(r"(?m)(export\s+)?class\s+{escaped}\s*[\{{<(]"),
            &format!(r"(?m)(const|let|var)\s+{escaped}\s*=\s*(async\s+)?\("),
            &format!(r"(?m)(const|let|var)\s+{escaped}\s*=\s*(async\s+)?function"),
            &format!(r"(?m)(export\s+)?(const|let|var)\s+{escaped}\s*="),
        ],
        "go" => &[&format!(r"(?m)func\s+(\([^)]+\)\s+)?{escaped}\s*\(")],
        _ => &[
            // Generic: look for anything that looks like a definition
            &format!(r"(?m)(function|fn|def|class|struct|impl|type|func)\s+{escaped}"),
        ],
    };

    // Build statically compiled patterns using OnceLock per invocation
    // (We can't use OnceLock for dynamic patterns, so compile fresh each call.
    // For hot paths, callers should cache; this is called infrequently.)
    raw_patterns
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
}

/// Collect a brace-delimited block starting at `start_line`.
/// Handles the case where the opening `{` may be on a later line.
fn collect_brace_block(lines: &[&str], start_line: usize) -> String {
    let mut result: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut found_open = false;

    for line in lines[start_line..].iter().copied() {
        result.push(line);

        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    found_open = true;
                }
                '}' => {
                    if found_open {
                        depth -= 1;
                    }
                }
                _ => {}
            }
        }

        if found_open && depth == 0 {
            break;
        }

        // Safety: don't collect more than 1000 lines per symbol
        if result.len() > 1000 {
            result.push("// ... [symbol body truncated at 1000 lines]");
            break;
        }
    }

    result.join("\n")
}

/// Collect an indent-delimited block starting at `start_line` (Python-style).
///
/// Stops when a non-empty line returns to the opener's indentation level or less.
fn collect_python_block(lines: &[&str], start_line: usize) -> String {
    let mut result: Vec<&str> = Vec::new();

    result.push(lines[start_line]);

    if start_line + 1 >= lines.len() {
        return result.join("\n");
    }

    let opener_indent = leading_spaces(lines[start_line]);

    for &line in lines[(start_line + 1)..].iter() {
        if line.trim().is_empty() {
            result.push(line);
            continue;
        }
        let indent = leading_spaces(line);
        // Stop when a non-empty line returns to the opener's indent level or less
        if indent <= opener_indent {
            break;
        }
        result.push(line);

        // Safety cap
        if result.len() > 1000 {
            result.push("    # ... [symbol body truncated at 1000 lines]");
            break;
        }
    }

    result.join("\n")
}

fn leading_spaces(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    // ---- apply_line_range ----

    #[test]
    fn test_line_range_basic() {
        let content = (1..=10)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, start, end) = apply_line_range(&content, Some(0), Some(3));
        assert_eq!(out, "line1\nline2\nline3");
        assert_eq!(start, 1);
        assert_eq!(end, 3);
    }

    #[test]
    fn test_line_range_offset_beyond_eof() {
        let content = "a\nb\nc";
        let (out, start, end) = apply_line_range(content, Some(100), Some(5));
        assert_eq!(out, "");
        assert_eq!(start, 101);
        assert_eq!(end, 101);
    }

    #[test]
    fn test_line_range_no_params() {
        let content = "a\nb\nc";
        let (out, start, end) = apply_line_range(content, None, None);
        assert_eq!(out, "a\nb\nc");
        assert_eq!(start, 1);
        assert_eq!(end, 3);
    }

    #[test]
    fn test_line_range_with_offset_mid() {
        let content = "a\nb\nc\nd\ne";
        let (out, start, end) = apply_line_range(content, Some(2), Some(2));
        assert_eq!(out, "c\nd");
        assert_eq!(start, 3);
        assert_eq!(end, 4);
    }

    // ---- expand_globs ----

    #[test]
    fn test_glob_expansion() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        fs::write(dir.path().join("c.txt"), "text").unwrap();

        let matches = expand_globs(dir.path(), "*.rs").unwrap();
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().any(|p| p.ends_with("a.rs")));
        assert!(matches.iter().any(|p| p.ends_with("b.rs")));
    }

    #[test]
    fn test_glob_rejects_symlink() {
        let dir = TempDir::new().unwrap();
        let real_file = dir.path().join("real.rs");
        fs::write(&real_file, "fn real() {}").unwrap();
        let link_file = dir.path().join("link.rs");
        symlink(&real_file, &link_file).unwrap();

        let matches = expand_globs(dir.path(), "*.rs").unwrap();
        // Only real.rs should be returned; link.rs is a symlink
        assert!(!matches.iter().any(|p| p.contains("link")));
        assert!(matches.iter().any(|p| p.contains("real")));
    }

    #[test]
    fn test_glob_no_matches() {
        let dir = TempDir::new().unwrap();
        let matches = expand_globs(dir.path(), "*.xyz").unwrap();
        assert!(matches.is_empty());
    }

    // ---- extract_symbols ----

    #[test]
    fn test_extract_rust_fn() {
        let content = r#"
fn foo(x: i32) -> i32 {
    x + 1
}

fn bar() {}
"#;
        let result = extract_symbols(content, "rust", &["foo".to_string()]);
        assert!(result.contains("// symbol: foo"));
        assert!(result.contains("fn foo"));
        assert!(result.contains("x + 1"));
        assert!(!result.contains("fn bar"));
    }

    #[test]
    fn test_extract_rust_impl() {
        let content = r#"
struct Foo {
    x: i32,
}

impl Foo {
    fn new() -> Self {
        Self { x: 0 }
    }
}
"#;
        let result = extract_symbols(content, "rust", &["Foo".to_string()]);
        assert!(result.contains("// symbol: Foo"));
        // Should find either struct or impl
        assert!(result.contains("Foo"));
    }

    #[test]
    fn test_extract_python_def() {
        let content =
            "def greet(name):\n    msg = 'Hello'\n    return msg\n\ndef other():\n    pass\n";

        let result = extract_symbols(content, "python", &["greet".to_string()]);
        assert!(result.contains("// symbol: greet"));
        assert!(result.contains("def greet"));
        assert!(result.contains("Hello"));
        // Should not include other()
        assert!(!result.contains("def other"));
    }

    #[test]
    fn test_extract_missing_symbol() {
        let content = "fn existing() {}";
        let result = extract_symbols(content, "rust", &["nonexistent".to_string()]);
        assert!(result.contains("// symbol nonexistent: NOT FOUND"));
    }

    #[test]
    fn test_extract_multiple_symbols() {
        let content = r#"
fn alpha() {
    println!("alpha");
}

fn beta() {
    println!("beta");
}
"#;
        let result = extract_symbols(content, "rust", &["alpha".to_string(), "beta".to_string()]);
        assert!(result.contains("// symbol: alpha"));
        assert!(result.contains("// symbol: beta"));
        assert!(result.contains("println!(\"alpha\")"));
        assert!(result.contains("println!(\"beta\")"));
    }

    #[test]
    fn test_line_range_limit_zero() {
        let content = "a\nb\nc";
        let (out, start, end) = apply_line_range(content, Some(0), Some(0));
        assert_eq!(out, "");
        assert_eq!(start, 1);
        assert_eq!(end, 1);
    }

    #[test]
    fn test_glob_rejects_traversal() {
        let dir = TempDir::new().unwrap();
        // Attempt to escape the base via traversal pattern — should return nothing
        let matches = expand_globs(dir.path(), "../*.rs");
        // Either an error or empty vec — traversal must not succeed
        if let Ok(paths) = matches {
            assert!(paths.is_empty(), "traversal should yield no paths");
        }
    }

    #[test]
    fn test_extract_typescript_function() {
        let content = r#"
export async function fetchData(url: string): Promise<string> {
    const res = await fetch(url);
    return res.text();
}

export function unused() {}
"#;
        let result = extract_symbols(content, "typescript", &["fetchData".to_string()]);
        assert!(result.contains("// symbol: fetchData"));
        assert!(result.contains("fetchData"));
        assert!(result.contains("fetch(url)"));
        assert!(!result.contains("unused"));
    }

    #[test]
    fn test_extract_go_function() {
        let content = r#"
func Greet(name string) string {
    return "Hello, " + name
}

func Other() {}
"#;
        let result = extract_symbols(content, "go", &["Greet".to_string()]);
        assert!(result.contains("// symbol: Greet"));
        assert!(result.contains("Hello"));
        assert!(!result.contains("Other"));
    }
}
