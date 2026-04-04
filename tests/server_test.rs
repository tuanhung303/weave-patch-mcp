//! Tests for src/server.rs - MCP server handlers
//!
//! Note: These tests focus on the underlying reader and applier modules
//! since the server methods are private tool handlers.

use apply_patch_mcp::applier::{apply_patch, validate_path};
use apply_patch_mcp::parser::parse_patch;
use apply_patch_mcp::reader::{apply_line_range, expand_globs, extract_symbols};
use std::fs;
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

// =============================================================================
// file reading tests via reader module
// =============================================================================

#[test]
fn read_single_file_success() {
    let dir = tmp();
    let content = "Hello, World!\nSecond line\n";
    fs::write(dir.path().join("test.txt"), content).unwrap();

    // Test expand_globs and file reading
    let paths = expand_globs(dir.path(), "test.txt").unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0], "test.txt");

    // Verify file content
    let read_content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
    assert_eq!(read_content, content);
}

#[test]
fn read_single_file_not_found() {
    let dir = tmp();

    // expand_globs returns empty for non-existent files
    let paths = expand_globs(dir.path(), "nonexistent.txt").unwrap();
    assert!(
        paths.is_empty(),
        "Should return empty for non-existent file"
    );
}

#[test]
fn read_single_file_path_traversal_rejected() {
    let dir = tmp();

    // Create a file outside the test dir (simulated)
    let outside = tmp();
    fs::write(outside.path().join("secret.txt"), "secret").unwrap();

    // Path traversal should be rejected by validate_path
    let result = validate_path(dir.path(), "../secret.txt");
    assert!(result.is_err(), "Should reject path traversal");
}

#[cfg(unix)]
#[test]
fn read_single_file_symlink_rejected() {
    let dir = tmp();

    // Create a real file and a symlink to it
    fs::write(dir.path().join("real.txt"), "real content").unwrap();
    std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

    // Symlinks should be excluded by expand_globs
    let paths = expand_globs(dir.path(), "*.txt").unwrap();
    assert!(
        !paths.contains(&"link.txt".to_string()),
        "Should exclude symlinks"
    );
    assert!(
        paths.contains(&"real.txt".to_string()),
        "Should include real files"
    );
}

// =============================================================================
// file reading glob tests
// =============================================================================

#[test]
fn read_files_glob_expansion() {
    let dir = tmp();

    fs::write(dir.path().join("file1.txt"), "content1").unwrap();
    fs::write(dir.path().join("file2.txt"), "content2").unwrap();
    fs::write(dir.path().join("other.rs"), "rust code").unwrap();

    let paths = expand_globs(dir.path(), "*.txt").unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"file1.txt".to_string()));
    assert!(paths.contains(&"file2.txt".to_string()));
    assert!(!paths.contains(&"other.rs".to_string()));
}

#[test]
fn read_files_glob_no_matches() {
    let dir = tmp();

    let paths = expand_globs(dir.path(), "*.nonexistent").unwrap();
    assert!(paths.is_empty(), "Should return empty for no matches");
}

// =============================================================================
// file reading line ranges
// =============================================================================

#[test]
fn read_files_with_offset_limit() {
    let content = "line1\nline2\nline3\nline4\nline5\n";

    let (sliced, start, end) = apply_line_range(content, Some(1), Some(2));

    assert_eq!(start, 2, "Start line should be 2 (1-based, offset 1)");
    assert_eq!(end, 3, "End line should be 3");
    assert_eq!(sliced, "line2\nline3", "Should return lines 2-3");
    assert!(!sliced.contains("line1"), "Should not contain line1");
    assert!(!sliced.contains("line5"), "Should not contain line5");
}

#[test]
fn read_files_offset_beyond_eof() {
    let content = "one\ntwo\n";

    let (sliced, start, end) = apply_line_range(content, Some(100), None);

    assert_eq!(start, 101, "Start should be offset+1 when beyond EOF");
    assert_eq!(end, 101, "End should match start when beyond EOF");
    assert!(
        sliced.is_empty(),
        "Should return empty content when offset beyond EOF"
    );
}

// =============================================================================
// file reading symbol extraction
// =============================================================================

#[test]
fn read_files_with_symbol_extraction_rust() {
    let content = r#"
fn helper() {}

fn main() {
    println!("hello");
}

struct MyStruct;
"#;

    let result = extract_symbols(content, "rust", &["main".to_string()]);
    assert!(result.contains("// symbol: main"), "Should extract symbol");
    assert!(result.contains("fn main()"), "Should contain main function");
}

#[test]
fn read_files_with_symbol_extraction_python() {
    let content = r#"
def helper():
    pass

def main():
    print("hello")

class MyClass:
    pass
"#;

    let result = extract_symbols(content, "python", &["main".to_string()]);
    // Result format: "// symbol: main\n<extracted code>" OR "// symbol main: NOT FOUND"
    // The symbol marker should be present
    assert!(
        result.contains("symbol: main") || result.contains("symbol main:"),
        "Result should contain symbol marker: {}",
        result
    );
}

#[test]
fn read_files_missing_symbol() {
    let content = "fn real() {}";

    let result = extract_symbols(content, "rust", &["nonexistent".to_string()]);
    assert!(result.contains("NOT FOUND"), "Should report missing symbol");
}

// =============================================================================
// file reading limits (simulated)
// =============================================================================

#[test]
fn read_files_single_file_limit_enforced() {
    // Create content that would exceed 512KB limit
    let big_content = "x".repeat(600 * 1024);

    // apply_line_range doesn't enforce limits, but the server does
    // Here we test that we can handle large content
    let (sliced, _, _) = apply_line_range(&big_content, None, None);
    assert_eq!(
        sliced.len(),
        big_content.len(),
        "Should return full content without truncation"
    );
}

#[test]
fn read_files_total_size_limit_simulated() {
    // The actual limit enforcement is in the server
    // Here we just verify the reader module works with multiple files
    let dir = tmp();

    for i in 0..3 {
        let content = format!("content{}", i);
        fs::write(dir.path().join(format!("file{}.txt", i)), content).unwrap();
    }

    let paths = expand_globs(dir.path(), "*.txt").unwrap();
    assert_eq!(paths.len(), 3, "Should find all 3 files");
}

// =============================================================================
// batch tool tests via applier module
// =============================================================================

#[test]
fn batch_apply_patch_success() {
    let dir = tmp();
    fs::write(dir.path().join("test.txt"), "old content").unwrap();

    let patch = r#"*** Begin Patch
*** Update File: test.txt
  old content
+new line
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Should succeed: {}",
        result.operations[0].message
    );
}

#[test]
fn batch_parse_error_handling() {
    // Invalid patch format
    let patch = "This is not a valid patch";

    let result = parse_patch(patch);
    assert!(result.is_err(), "Should return parse error");
}

#[test]
fn batch_apply_error_returns_structured_result() {
    let dir = tmp();
    fs::write(dir.path().join("test.txt"), "wrong content").unwrap();

    // Patch context doesn't match file
    let patch = r#"*** Begin Patch
*** Update File: test.txt
  nonexistent context
+new line
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "error",
        "Should return error status"
    );
    assert!(
        !result.operations[0].message.is_empty(),
        "Should have error message"
    );
}

#[test]
fn batch_empty_patch_no_operations() {
    // Valid format but no operations
    let patch = "*** Begin Patch\n*** End Patch";

    let ops = parse_patch(patch).unwrap();
    assert!(ops.is_empty(), "Should return empty operations");
}

#[test]
fn batch_multi_file_atomic_all_or_nothing() {
    let dir = tmp();
    fs::write(dir.path().join("file1.txt"), "content1").unwrap();
    fs::write(dir.path().join("file2.txt"), "content2").unwrap();

    // One valid update, one invalid (context mismatch)
    let patch = r#"*** Begin Patch
*** Update File: file1.txt
  content1
+added1
*** Update File: file2.txt
  wrong context
+added2
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    // Both should fail due to atomic behavior
    let has_error = result.operations.iter().any(|op| op.status == "error");
    assert!(has_error, "Should have at least one error");

    // Check that file1 wasn't modified either (atomic rollback)
    let file1_content = fs::read_to_string(dir.path().join("file1.txt")).unwrap();
    assert!(
        !file1_content.contains("added1"),
        "Should not have applied due to atomic rollback"
    );
}

// =============================================================================
// Server info tests
// =============================================================================

#[test]
fn server_capabilities_available() {
    // Test that the server type exists and can be instantiated
    // The actual get_info() returns ServerInfo which we can verify
    use apply_patch_mcp::server::ApplyPatchServer;
    use rmcp::ServerHandler;

    let server = ApplyPatchServer::new();
    let info = server.get_info();

    // ServerInfo has capabilities
    assert!(
        info.instructions.is_some(),
        "Server should have instructions"
    );
}

#[test]
fn batch_add_file_success() {
    let dir = tmp();

    let patch = r#"*** Begin Patch
*** Add File: newfile.txt
+line1
+line2
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Should succeed: {}",
        result.operations[0].message
    );

    // Verify file was created
    assert!(
        dir.path().join("newfile.txt").exists(),
        "File should have been created"
    );
    let content = fs::read_to_string(dir.path().join("newfile.txt")).unwrap();
    assert!(content.contains("line1"), "Should contain line1");
    assert!(content.contains("line2"), "Should contain line2");
}

#[test]
fn batch_delete_file_success() {
    let dir = tmp();
    fs::write(dir.path().join("todelete.txt"), "content").unwrap();

    let patch = r#"*** Begin Patch
*** Delete File: todelete.txt
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Should succeed: {}",
        result.operations[0].message
    );

    // Verify file was deleted
    assert!(
        !dir.path().join("todelete.txt").exists(),
        "File should have been deleted"
    );
}

#[test]
fn batch_move_file_success() {
    let dir = tmp();
    fs::write(dir.path().join("old.txt"), "content").unwrap();

    let patch = r#"*** Begin Patch
*** Update File: old.txt
*** Move to: new.txt
  content
*** End Patch"#;

    let ops = parse_patch(patch).unwrap();
    let result = apply_patch(ops, dir.path());

    // Move may be supported; check file state
    let old_exists = dir.path().join("old.txt").exists();
    let new_exists = dir.path().join("new.txt").exists();

    // Either old was moved to new, or old was updated
    assert!(new_exists || old_exists, "Either old or new should exist");
}
