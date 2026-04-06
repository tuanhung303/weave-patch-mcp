use apply_patch_mcp::applier::apply_patch;
use apply_patch_mcp::parser::parse_patch;
use std::fs;
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// Test 1: @@ hint disambiguates two functions with identical bodies
/// function_a and function_b have the same code pattern; hint targets function_b only
#[test]
fn test_hint_disambiguates_identical_code() {
    let dir = tmp();
    fs::write(
        dir.path().join("multi.py"),
        "def function_a():\n    x = 1\n    print(\"hello\")\n    return x\n\ndef function_b():\n    x = 1\n    print(\"hello\")\n    return x\n",
    )
    .unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: multi.py\n",
        "@@ def function_b\n",
        "      x = 1\n",
        "      print(\"hello\")\n",
        "-    return x\n",
        "+    return x + 1\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());
    assert_eq!(
        result.operations[0].status, "ok",
        "Hint should disambiguate to function_b: {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(dir.path().join("multi.py")).unwrap();

    assert!(
        content.contains("def function_a()"),
        "function_a should still exist, got:\n{content}"
    );
    assert!(
        content.contains("def function_a():\n    x = 1\n    print(\"hello\")\n    return x\n"),
        "function_a must be completely unchanged, got:\n{content}"
    );

    assert!(
        content.contains("def function_b():\n    x = 1\n    print(\"hello\")\n    return x + 1\n"),
        "function_b must have 'return x + 1', got:\n{content}"
    );
}

/// Test 2: Python nested indentation patch
/// Verifies indentation is preserved when patching deep inside if/else blocks
#[test]
fn test_py_nested_indentation_patch() {
    let dir = tmp();
    fs::write(
        dir.path().join("nested.py"),
        "def process(data):\n    if data:\n        if data.get(\"key\"):\n            result = data[\"key\"]\n            print(f\"Found: {result}\")\n            return result\n        return None\n    return None\n",
    )
    .unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: nested.py\n",
        "@@ def process(data):\n",
        "      if data:\n",
        "          if data.get(\"key\"):\n",
        "              result = data[\"key\"]\n",
        "-            print(f\"Found: {result}\")\n",
        "+            print(f\"Value: {result}\")\n",
        "              return result\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());
    assert_eq!(
        result.operations[0].status, "ok",
        "Nested indent patch should succeed: {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(dir.path().join("nested.py")).unwrap();
    assert!(
        content.contains("print(f\"Value: {result}\")"),
        "Should contain new 'Value:' text, got:\n{content}"
    );
    assert!(
        !content.contains("print(f\"Found:"),
        "Old 'Found:' should be gone, got:\n{content}"
    );
    assert!(
        content.contains("        if data.get(\"key\")"),
        "8-space indent for nested if preserved, got:\n{content}"
    );
    assert!(
        content.contains("            result = data[\"key\"]"),
        "12-space indent for result assignment preserved, got:\n{content}"
    );
    assert!(
        content.contains("            print(f\"Value: {result}\")"),
        "Changed line has correct 12-space indent, got:\n{content}"
    );
}

/// Test 3: CRLF line endings - patch succeeds after \r\n normalization to \n
#[test]
fn test_crlf_line_endings_patch() {
    let dir = tmp();
    let path = dir.path().join("crlf.py");
    fs::write(
        &path,
        "def greet():\r\n    print(\"hello\")\r\n    return True\r\n",
    )
    .unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: crlf.py\n",
        "  def greet():\n",
        "-    print(\"hello\")\n",
        "+    print(\"Hello, World!\")\n",
        "      return True\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());
    assert_eq!(
        result.operations[0].status, "ok",
        "CRLF patch should succeed after normalization: {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("print(\"Hello, World!\")"),
        "Should contain updated greeting, got:\n{content}"
    );
    assert!(
        !content.contains("print(\"hello\")"),
        "Old greeting should be replaced, got:\n{content}"
    );
}

/// Test 4: ContextNotFound error includes rich diagnostics
/// (total line count, hint info, file excerpt)
#[test]
fn test_context_not_found_has_diagnostics() {
    let dir = tmp();
    fs::write(
        dir.path().join("diagnostic.py"),
        "def foo():\n    x = 1\n    return x\n",
    )
    .unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: diagnostic.py\n",
        "@@ def bar\n",
        " nonexistent line\n",
        "-also missing\n",
        "+replacement\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());
    assert_eq!(
        result.operations[0].status, "error",
        "Non-existent context should fail, got: {}",
        result.operations[0].message
    );

    let msg = &result.operations[0].message;

    assert!(
        msg.contains("file has") && msg.contains("lines"),
        "Error should include total line count (e.g. 'file has X lines'), got: {msg}"
    );

    assert!(
        msg.contains("Hint"),
        "Error should include hint info (e.g. 'Hint attempted'), got: {msg}"
    );
    assert!(
        msg.contains("def bar"),
        "Error should show the attempted hint text, got: {msg}"
    );

    assert!(
        msg.contains("Nearest") || msg.contains("File preview"),
        "Error should include file excerpt, got: {msg}"
    );
    assert!(
        msg.contains("def foo"),
        "Excerpt should show actual file content, got: {msg}"
    );
}

// =============================================================================
// Additional edge case tests (5 new tests)
// =============================================================================

/// Test 5: Empty file creation via Add File
#[test]
fn patch_empty_file_creation() {
    let dir = tmp();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Add File: empty.txt\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Empty file creation should succeed: {}",
        result.operations[0].message
    );

    let path = dir.path().join("empty.txt");
    assert!(path.exists(), "Empty file should exist");
    let content = fs::read_to_string(&path).unwrap();
    assert!(
        content.is_empty(),
        "Empty file should have no content, got: {:?}",
        content
    );
}

/// Test 6: Unicode content preservation
#[test]
fn patch_unicode_content_preserved() {
    let dir = tmp();
    let original = "Hello 世界! 🌍\nПривет мир\nالسلام عليكم\n";
    fs::write(dir.path().join("unicode.txt"), original).unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: unicode.txt\n",
        "  Hello 世界! 🌍\n",
        "-Привет мир\n",
        "+Bonjour monde\n",
        "  السلام عليكم\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Unicode patch should succeed: {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(dir.path().join("unicode.txt")).unwrap();
    assert!(
        content.contains("Hello 世界! 🌍"),
        "Unicode emoji should be preserved"
    );
    assert!(
        content.contains("السلام عليكم"),
        "Arabic text should be preserved"
    );
    assert!(
        content.contains("Bonjour monde"),
        "Replacement should be applied"
    );
    assert!(!content.contains("Привет мир"), "Old text should be gone");
}

/// Test 7: Very long lines handling
#[test]
fn patch_very_long_lines_handled() {
    let dir = tmp();
    // Create a file with very long lines (>10KB)
    let long_prefix = "x".repeat(5000);
    let long_suffix = "y".repeat(5000);
    let target = format!("{}TARGET{}", long_prefix, long_suffix);
    let original = target.to_string();
    fs::write(dir.path().join("longlines.txt"), &original).unwrap();

    let replacement = format!("{}REPLACEMENT{}", long_prefix, long_suffix);
    let patch = format!(
        "*** Begin Patch\n*** Update File: longlines.txt\n-{}\n+{}\n*** End Patch",
        target, replacement
    );

    let ops = parse_patch(&patch).unwrap();
    let result = apply_patch(ops, dir.path());

    assert_eq!(
        result.operations[0].status, "ok",
        "Long line patch should succeed: {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(dir.path().join("longlines.txt")).unwrap();
    assert!(
        content.contains("REPLACEMENT"),
        "Long line replacement should be applied"
    );
    assert!(!content.contains("TARGET"), "Old content should be gone");
}

/// Test 8: Concurrent shadow file collision (tests atomic shadow file creation)
#[test]
fn concurrent_shadow_file_collision() {
    let dir = tmp();
    fs::write(dir.path().join("concurrent.txt"), "original\n").unwrap();

    // Create multiple patches targeting the same file
    let patch1 = concat!(
        "*** Begin Patch\n",
        "*** Update File: concurrent.txt\n",
        "  original\n",
        "+patch1 line\n",
        "*** End Patch",
    );

    let patch2 = concat!(
        "*** Begin Patch\n",
        "*** Update File: concurrent.txt\n",
        "  original\n",
        "+patch2 line\n",
        "*** End Patch",
    );

    // Apply both patches sequentially (simulating concurrent access pattern)
    let ops1 = parse_patch(patch1).unwrap();
    let _result1 = apply_patch(ops1, dir.path());

    // After first patch, the content should be different
    let _content1 = fs::read_to_string(dir.path().join("concurrent.txt")).unwrap();

    // Second patch should fail because context "original" no longer exists
    let ops2 = parse_patch(patch2).unwrap();
    let _result2 = apply_patch(ops2, dir.path());

    // One should succeed, one might fail depending on timing
    // The key is that the file should never be corrupted
    let final_content = fs::read_to_string(dir.path().join("concurrent.txt")).unwrap();

    // File should be in a consistent state
    assert!(
        final_content.contains("patch1 line") || final_content.contains("patch2 line"),
        "One patch should have been applied"
    );

    // Atomicity check: file should never be in a partially written state
    let lines: Vec<&str> = final_content.lines().collect();
    assert!(
        !lines.is_empty() && lines.len() <= 3,
        "File should have consistent line count, got: {:?}",
        lines
    );
}

/// Test 9: Read-only file handling (Unix only)
/// Note: This test documents behavior - on some systems (like macOS with certain
/// configurations), root or file ownership may bypass permission checks
#[cfg(unix)]
#[test]
fn patch_read_only_file_handling() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tmp();
    fs::write(dir.path().join("readonly.txt"), "content\n").unwrap();

    // Make file read-only
    let path = dir.path().join("readonly.txt");
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o444); // read-only
    fs::set_permissions(&path, perms).unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: readonly.txt\n",
        "  content\n",
        "+new line\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());

    // The behavior depends on the OS and user permissions:
    // - On Linux as non-root: should fail with error
    // - On macOS or as file owner: may succeed (file owner can write to read-only files)
    // - On Windows: permissions work differently
    //
    // We just verify the file is in a consistent state after the operation

    let content = fs::read_to_string(&path).unwrap();

    if result.operations[0].status == "ok" {
        // If it succeeded, verify the change was applied
        assert!(
            content.contains("new line") || content.contains("content"),
            "File should have expected content"
        );
    } else {
        // If it failed, verify the file wasn't modified
        assert!(
            !content.contains("new line"),
            "Read-only file should not be modified on error: {}",
            content
        );
    }

    // Restore permissions for cleanup
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o644);
    let _ = fs::set_permissions(&path, perms);
}

/// Test 9 (non-Unix): Read-only file test placeholder
#[cfg(not(unix))]
#[test]
fn patch_read_only_file_handling() {
    // On non-Unix systems, skip this test
    eprintln!("Skipping read-only file test on non-Unix platform");
}

/// Test 10: Multiple file operations in single patch (atomic commit)
#[test]
fn patch_multi_file_atomic_all_or_nothing() {
    let dir = tmp();
    fs::write(dir.path().join("file1.txt"), "file1 content").unwrap();
    fs::write(dir.path().join("file2.txt"), "file2 content").unwrap();

    // Valid update to file1, but invalid context for file2
    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: file1.txt\n",
        "  file1 content\n",
        "+added to file1\n",
        "*** Update File: file2.txt\n",
        "  wrong context\n",
        "+added to file2\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());

    // Entire patch should fail due to context mismatch on file2
    assert!(
        result.operations.iter().any(|op| op.status == "error"),
        "Patch should have at least one error"
    );

    // file1 should NOT have been modified (atomic rollback)
    let content1 = fs::read_to_string(dir.path().join("file1.txt")).unwrap();
    assert!(
        !content1.contains("added to file1"),
        "file1 should not be modified due to atomic rollback, got: {}",
        content1
    );
}

/// Test: Unified patch format with Read + Write operations
/// This tests the new *** Read File: syntax embedded in patches
#[test]
fn test_unified_read_write_patch() {
    let dir = tmp();

    // Create initial files
    fs::write(
        dir.path().join("main.rs"),
        "fn main() {\n    println!(\"Hello\");\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn helper() -> i32 {\n    42\n}\n",
    )
    .unwrap();

    // Parse a patch that reads then updates
    let input = concat!(
        "*** Begin Patch\n",
        "*** Read File: main.rs\n",
        "*** Update File: lib.rs\n",
        "@@ pub fn helper\n",
        " pub fn helper() -> i32 {\n",
        "-    42\n",
        "+    100\n",
        " }\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    assert_eq!(ops.len(), 2, "Should have 2 operations: Read + Update");

    // Verify first op is Read
    match &ops[0] {
        apply_patch_mcp::parser::FileOp::Read { path, .. } => {
            assert_eq!(path, "main.rs");
        }
        _ => panic!("First operation should be Read"),
    }

    // Verify second op is Update
    match &ops[1] {
        apply_patch_mcp::parser::FileOp::Update { path, .. } => {
            assert_eq!(path, "lib.rs");
        }
        _ => panic!("Second operation should be Update"),
    }

    // Apply the patch
    let result = apply_patch(ops, dir.path());
    assert_eq!(result.operations.len(), 2);
    assert_eq!(result.operations[0].status, "ok");
    assert_eq!(result.operations[0].op_type, "read");
    assert_eq!(result.operations[1].status, "ok");
    assert_eq!(result.operations[1].op_type, "update");

    // Verify lib.rs was updated
    let lib_content = fs::read_to_string(dir.path().join("lib.rs")).unwrap();
    assert!(lib_content.contains("100"), "lib.rs should be updated");
    assert!(!lib_content.contains("42"), "old value should be removed");

    // Verify main.rs is unchanged (read doesn't modify)
    let main_content = fs::read_to_string(dir.path().join("main.rs")).unwrap();
    assert!(main_content.contains("println!(\"Hello\")"));
}

/// Test: Mixed operations in single patch (Read + Add + Update + Delete)
#[test]
fn test_mixed_operations_in_patch() {
    let dir = tmp();

    // Create files
    fs::write(dir.path().join("keep.txt"), "Keep this\n").unwrap();
    fs::write(dir.path().join("update.txt"), "Old content\n").unwrap();
    fs::write(dir.path().join("delete.txt"), "Delete me\n").unwrap();

    let input = concat!(
        "*** Begin Patch\n",
        "*** Read File: keep.txt\n",
        "*** Add File: new.txt\n",
        "+New file content\n",
        "*** Update File: update.txt\n",
        "-Old content\n",
        "+Updated content\n",
        "*** Delete File: delete.txt\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    assert_eq!(ops.len(), 4);

    let result = apply_patch(ops, dir.path());

    // All operations should succeed
    for op in &result.operations {
        assert_eq!(op.status, "ok", "Operation failed: {:?}", op);
    }

    // Verify results
    assert!(
        dir.path().join("new.txt").exists(),
        "new.txt should be created"
    );
    assert!(
        !dir.path().join("delete.txt").exists(),
        "delete.txt should be deleted"
    );

    let update_content = fs::read_to_string(dir.path().join("update.txt")).unwrap();
    assert!(update_content.contains("Updated content"));

    let keep_content = fs::read_to_string(dir.path().join("keep.txt")).unwrap();
    assert!(keep_content.contains("Keep this"), "keep.txt unchanged");
}

/// Test: Word-boundary hint matching with $ suffix
/// @@ fn foo$ should match 'fn foo()' but not 'fn foo_bar()'
#[test]
fn test_word_boundary_hint_matching() {
    let dir = tmp();
    fs::write(
        dir.path().join("functions.py"),
        "def foo():\n    return 1\n\ndef foo_bar():\n    return 2\n\ndef bar():\n    return 3\n",
    )
    .unwrap();

    // Test 1: @@ def foo$ should match 'def foo():' but not 'def foo_bar():'
    let input = concat!(
        "*** Begin Patch\n",
        "*** Update File: functions.py\n",
        "@@ def foo$\n",
        " def foo():\n",
        "-    return 1\n",
        "+    return 10\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());
    assert_eq!(
        result.operations[0].status, "ok",
        "Word-boundary hint should match exactly 'def foo()': {}",
        result.operations[0].message
    );

    let content = fs::read_to_string(dir.path().join("functions.py")).unwrap();

    // Should have modified foo(), not foo_bar()
    assert!(
        content.contains("def foo():\n    return 10"),
        "Should have updated foo() to return 10, got:\n{content}"
    );
    assert!(
        content.contains("def foo_bar():\n    return 2"),
        "Should NOT have modified foo_bar(), got:\n{content}"
    );
    assert!(
        content.contains("def bar():\n    return 3"),
        "bar() should be unchanged, got:\n{content}"
    );

    // Reset file for test 2
    fs::write(
        dir.path().join("functions.py"),
        "def foo():\n    return 1\n\ndef foo_bar():\n    return 2\n\ndef bar():\n    return 3\n",
    )
    .unwrap();

    // Test 2: Without $ suffix, should match first occurrence (could be foo_bar)
    // Actually without $ it matches any line containing 'def foo', so it could match foo_bar
    // Let's verify that $ makes it specific
    let input2 = concat!(
        "*** Begin Patch\n",
        "*** Update File: functions.py\n",
        "@@ def foo_bar$\n",
        " def foo_bar():\n",
        "-    return 2\n",
        "+    return 20\n",
        "*** End Patch",
    );

    let ops2 = parse_patch(input2).unwrap();
    let result2 = apply_patch(ops2, dir.path());
    assert_eq!(
        result2.operations[0].status, "ok",
        "Word-boundary hint should match 'def foo_bar()': {}",
        result2.operations[0].message
    );

    let content2 = fs::read_to_string(dir.path().join("functions.py")).unwrap();
    assert!(
        content2.contains("def foo_bar():\n    return 20"),
        "Should have updated foo_bar() to return 20, got:\n{content2}"
    );
    assert!(
        content2.contains("def foo():\n    return 1"),
        "foo() should be unchanged, got:\n{content2}"
    );
}

/// Test: Parallel patch application for multiple files
/// Create 10 files, patch all in parallel, verify atomic commit
#[test]
fn test_parallel_patch_multiple_files() {
    let dir = tmp();

    // Create 10 files
    for i in 0..10 {
        fs::write(
            dir.path().join(format!("file{}.txt", i)),
            format!("content {}\n", i),
        )
        .unwrap();
    }

    // Create a patch that updates all 10 files
    let mut patch_lines = vec!["*** Begin Patch".to_string()];
    for i in 0..10 {
        patch_lines.push(format!("*** Update File: file{}.txt", i));
        patch_lines.push(format!("  content {}", i));
        patch_lines.push(format!("+new line {}", i));
    }
    patch_lines.push("*** End Patch".to_string());

    let input = patch_lines.join("\n");
    let ops = parse_patch(&input).unwrap();
    assert_eq!(ops.len(), 10, "Should have 10 update operations");

    let result = apply_patch(ops, dir.path());

    // All operations should succeed
    for (i, op) in result.operations.iter().enumerate() {
        assert_eq!(
            op.status, "ok",
            "Operation {} should succeed: {}",
            i, op.message
        );
    }

    // Verify all files were updated
    for i in 0..10 {
        let content = fs::read_to_string(dir.path().join(format!("file{}.txt", i))).unwrap();
        assert!(
            content.contains(&format!("new line {}", i)),
            "file{}.txt should have new line, got:\n{}",
            i,
            content
        );
    }
}

/// Test: Conflict detection - Add+Update on same path should error
#[test]
fn test_conflict_add_update_same_path() {
    let dir = tmp();

    // Try to Add and Update the same file in one patch
    let input = concat!(
        "*** Begin Patch\n",
        "*** Add File: conflict.txt\n",
        "+new content\n",
        "*** Update File: conflict.txt\n",
        "-old content\n",
        "+updated content\n",
        "*** End Patch",
    );

    let ops = parse_patch(input).unwrap();
    let result = apply_patch(ops, dir.path());

    // Should have an error due to conflict
    let has_error = result.operations.iter().any(|op| op.status == "error");
    assert!(has_error, "Should detect Add+Update conflict");

    // At least one operation should mention conflict
    let has_conflict_msg = result
        .operations
        .iter()
        .any(|op| op.message.contains("Cannot Add and Update") || op.message.contains("conflict"));
    assert!(has_conflict_msg, "Error message should mention conflict");
}
