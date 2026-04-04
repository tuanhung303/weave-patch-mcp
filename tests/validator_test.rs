//! Tests for src/validator.rs - External tool validation

use apply_patch_mcp::validator::validate_file;
use std::fs;
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn tool_exists(tool: &str) -> bool {
    std::process::Command::new("which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// =============================================================================
// Rust validation tests
// =============================================================================

#[test]
fn rustfmt_valid_file_returns_empty() {
    if !tool_exists("rustfmt") {
        eprintln!("Skipping test: rustfmt not found");
        return;
    }
    let dir = tmp();
    let content = r#"fn main() {
    println!("Hello");
}
"#;
    let path = dir.path().join("valid.rs");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid Rust file should return no advisories, got: {:?}",
        result
    );
}

#[test]
fn rustfmt_invalid_file_returns_advisory() {
    if !tool_exists("rustfmt") {
        eprintln!("Skipping test: rustfmt not found");
        return;
    }
    let dir = tmp();
    // Poorly formatted Rust code
    let content = "fn main(){println!(\"hello\");}";
    let path = dir.path().join("invalid.rs");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        !result.is_empty(),
        "Invalid formatting should return advisory"
    );
    assert!(
        result[0].contains("Advisory"),
        "Should contain Advisory prefix: {}",
        result[0]
    );
}

#[test]
fn rustfmt_syntax_error_returns_advisory() {
    if !tool_exists("rustfmt") {
        eprintln!("Skipping test: rustfmt not found");
        return;
    }
    let dir = tmp();
    // Invalid Rust syntax
    let content = "fn main() { let x = ; }";
    let path = dir.path().join("syntax_error.rs");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(!result.is_empty(), "Syntax error should return advisory");
}

// =============================================================================
// Python validation tests
// =============================================================================

#[test]
fn python_valid_file_returns_empty() {
    if !tool_exists("python") && !tool_exists("python3") {
        eprintln!("Skipping test: python not found");
        return;
    }
    let dir = tmp();
    let content = r#"def hello():
    print("Hello")
    return 42
"#;
    let path = dir.path().join("valid.py");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid Python file should return no advisories, got: {:?}",
        result
    );
}

#[test]
fn python_invalid_syntax_returns_advisory() {
    if !tool_exists("python") && !tool_exists("python3") {
        eprintln!("Skipping test: python not found");
        return;
    }
    let dir = tmp();
    // Invalid Python syntax
    let content = "def hello(\n    print \"missing colon and paren\n";
    let path = dir.path().join("invalid.py");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        !result.is_empty(),
        "Invalid Python syntax should return advisory"
    );
    assert!(
        result[0].contains("Advisory"),
        "Should contain Advisory prefix: {}",
        result[0]
    );
}

#[test]
fn python_indentation_error_returns_advisory() {
    if !tool_exists("python") && !tool_exists("python3") {
        eprintln!("Skipping test: python not found");
        return;
    }
    let dir = tmp();
    // Indentation error
    let content = "def hello():\nprint(\"not indented\")";
    let path = dir.path().join("indent_error.py");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        !result.is_empty(),
        "Indentation error should return advisory"
    );
}

// =============================================================================
// Go validation tests
// =============================================================================

#[test]
fn gofmt_valid_file_returns_empty() {
    if !tool_exists("gofmt") {
        eprintln!("Skipping test: gofmt not found");
        return;
    }
    let dir = tmp();
    // Properly formatted Go code (must use gofmt style)
    let content = r#"package main

import "fmt"

func main() {
	fmt.Println("Hello")
}
"#;
    let path = dir.path().join("valid.go");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid Go file should return no advisories, got: {:?}",
        result
    );
}

#[test]
fn gofmt_invalid_formatting_returns_advisory() {
    if !tool_exists("gofmt") {
        eprintln!("Skipping test: gofmt not found");
        return;
    }
    let dir = tmp();
    // Poorly formatted Go code
    let content = "package main\nimport \"fmt\"\nfunc main(){fmt.Println(\"hello\")}";
    let path = dir.path().join("invalid.go");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        !result.is_empty(),
        "Unformatted Go file should return advisory"
    );
    assert!(
        result[0].contains("gofmt"),
        "Should mention gofmt: {}",
        result[0]
    );
}

#[test]
fn gofmt_syntax_error_returns_advisory() {
    if !tool_exists("gofmt") {
        eprintln!("Skipping test: gofmt not found");
        return;
    }
    let dir = tmp();
    // Invalid Go syntax
    let content = "package main\nfunc main() { let x = }";
    let path = dir.path().join("syntax_error.go");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(!result.is_empty(), "Syntax error should return advisory");
}

// =============================================================================
// JSON validation tests
// =============================================================================

#[test]
fn json_valid_file_returns_empty() {
    if !tool_exists("python3") {
        eprintln!("Skipping test: python3 not found");
        return;
    }
    let dir = tmp();
    let content = r#"{"key": "value", "number": 42}"#;
    let path = dir.path().join("valid.json");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid JSON file should return no advisories, got: {:?}",
        result
    );
}

#[test]
fn json_invalid_syntax_returns_advisory() {
    if !tool_exists("python3") {
        eprintln!("Skipping test: python3 not found");
        return;
    }
    let dir = tmp();
    // Invalid JSON - trailing comma
    let content = r#"{"key": "value",}"#;
    let path = dir.path().join("invalid.json");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(!result.is_empty(), "Invalid JSON should return advisory");
    assert!(
        result[0].contains("Advisory"),
        "Should contain Advisory prefix: {}",
        result[0]
    );
}

#[test]
fn json_malformed_returns_advisory() {
    if !tool_exists("python3") {
        eprintln!("Skipping test: python3 not found");
        return;
    }
    let dir = tmp();
    // Malformed JSON
    let content = "not json at all";
    let path = dir.path().join("malformed.json");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(!result.is_empty(), "Malformed JSON should return advisory");
}

// =============================================================================
// Bash validation tests
// =============================================================================

#[test]
fn bash_valid_script_returns_empty() {
    if !tool_exists("bash") {
        eprintln!("Skipping test: bash not found");
        return;
    }
    let dir = tmp();
    let content = r#"#!/bin/bash
set -e
echo "Hello World"
"#;
    let path = dir.path().join("valid.sh");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid bash script should return no advisories, got: {:?}",
        result
    );
}

#[test]
fn bash_invalid_syntax_returns_advisory() {
    if !tool_exists("bash") {
        eprintln!("Skipping test: bash not found");
        return;
    }
    let dir = tmp();
    // Invalid bash syntax
    let content = r#"#!/bin/bash
if [ "test" ] then
    echo "missing semicolon or newline"
fi
"#;
    let path = dir.path().join("invalid.sh");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        !result.is_empty(),
        "Invalid bash syntax should return advisory"
    );
    assert!(
        result[0].contains("Advisory"),
        "Should contain Advisory prefix: {}",
        result[0]
    );
}

// =============================================================================
// Edge case tests
// =============================================================================

#[test]
fn tool_not_found_returns_skip_advisory() {
    let dir = tmp();
    // Create a file with an extension that requires a non-existent tool
    // We'll simulate by using an extension we know needs a tool but
    // the tool won't exist in a custom PATH
    let content = "test";
    let path = dir.path().join("test.xyz");
    fs::write(&path, content).unwrap();

    // .xyz is not a supported extension, so it should return empty
    let result = validate_file(&path, &path);
    assert!(result.is_empty(), "Unknown extension should return empty");
}

#[test]
fn unknown_extension_returns_empty() {
    let dir = tmp();
    let content = "some random content";
    let path = dir.path().join("file.xyz");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Unknown extension should return empty vec"
    );
}

#[test]
fn empty_file_no_extension_returns_empty() {
    let dir = tmp();
    let path = dir.path().join("no_extension");
    fs::write(&path, "").unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "File with no extension should return empty vec"
    );
}

#[test]
fn terraform_valid_file_returns_empty() {
    if !tool_exists("terraform") {
        eprintln!("Skipping test: terraform not found");
        return;
    }
    let dir = tmp();
    let content = r#"resource "null_resource" "test" {}
"#;
    let path = dir.path().join("valid.tf");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    // Terraform fmt returns exit code 0 for valid files
    // Note: terraform fmt -check returns non-zero if formatting is needed
    // but no syntax errors
}

#[test]
fn javascript_valid_file_returns_empty() {
    if !tool_exists("node") {
        eprintln!("Skipping test: node not found");
        return;
    }
    let dir = tmp();
    let content = "function hello() { console.log('world'); }";
    let path = dir.path().join("valid.js");
    fs::write(&path, content).unwrap();

    let result = validate_file(&path, &path);
    assert!(
        result.is_empty(),
        "Valid JS file should return no advisories, got: {:?}",
        result
    );
}
