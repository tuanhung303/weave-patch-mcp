# apply-patch-mcp

MCP server for structured file patching using V4A diffs. Create, update, and delete files in one atomic operation.

## Why This Over Edit/Write?

| | Traditional (Edit + Write) | apply-patch-mcp |
|---|---|---|
| **Calls per change** | 1 tool call per file | 1 call for all files |
| **Multi-file atomicity** | No — broken intermediate states | Yes — all-or-nothing |
| **Context anchoring** | Line numbers (drift after edits) | Pattern matching (survives drift) |
| **Token cost** | High — full file content per call | Low — only diffs sent |
| **Reviewer clarity** | Opaque (full file dumps) | Standard diff format |
| **Scale** | Painful at 5+ files | Handles 100+ files per call |
| **Error recovery** | Manual retry | Fuzzy matching + structured diagnostics |

### Recommended: Disable Traditional Tools

For best results, deny Edit/Write in your Claude Code settings so the model always uses apply-patch:

Add to `~/.claude/settings.json` under `permissions.deny`:
```json
["Edit(*)", "Write(*)"]
```

This eliminates context pollution from unused tool descriptions and forces consistent patch-based editing.

## Installation

```bash
npm install -g mcp-apply-patch
```

Or use directly with npx (no install needed):
```bash
npx -y mcp-apply-patch
```

## MCP Configuration

### Claude Code (Recommended)

```bash
claude mcp add -s user patch -- npx -y mcp-apply-patch
```

Or add manually to `~/.claude.json`:
```json
{
  "mcpServers": {
    "patch": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "mcp-apply-patch"]
    }
  }
}
```

### Qwen

Add to `~/.qwen/settings.json` under `mcpServers`:
```json
"patch": {
  "command": "npx",
  "args": ["-y", "mcp-apply-patch"]
}
```

### Gemini CLI

Add to `~/.gemini/settings.json`:
```json
{
  "mcpServers": {
    "patch": {
      "command": "npx",
      "args": ["-y", "mcp-apply-patch"]
    }
  }
}
```

### OpenCode

Add to `~/.config/opencode/config.toml` or project `.opencode.json`:
```json
{
  "mcpServers": {
    "patch": {
      "command": "npx",
      "args": ["-y", "mcp-apply-patch"]
    }
  }
}
```

## Patch Format (V4A)

All patches are wrapped in `*** Begin Patch` / `*** End Patch` markers.

### Create a file

```
*** Begin Patch
*** Add File: src/hello.rs
+fn hello() {
+    println!("Hello, world!");
+}
*** End Patch
```

### Update a file

Context lines (space-prefixed) anchor the edit. `-` removes, `+` adds.

```
*** Begin Patch
*** Update File: src/lib.rs
@@ impl Server
 pub fn handle(&self, req: Request) -> Response {
-    self.old_handler(req)
+    self.new_handler(req)
 }
*** End Patch
```

### Multiple hunks in one file

```
*** Begin Patch
*** Update File: src/lib.rs
@@ fn setup
 fn setup() {
-    old_init();
+    new_init();
 }
@@ fn teardown
 fn teardown() {
-    old_cleanup();
+    new_cleanup();
 }
*** End Patch
```

### Delete a file

```
*** Begin Patch
*** Delete File: src/deprecated.rs
*** End Patch
```

### Multi-file patch

```
*** Begin Patch
*** Add File: src/greet.rs
+pub fn greet(name: &str) -> String {
+    format!("Hello, {}!", name)
+}
*** Update File: src/main.rs
@@ fn main
 fn main() {
-    println!("Hello");
+    println!("{}", greet::greet("World"));
 }
*** Delete File: src/old_greet.rs
*** End Patch
```

## Key Concepts

- **Context lines** (space prefix): Anchor the edit location. The leading space is a format delimiter — it is stripped before matching.
- **@@ hints**: Optional. Substring-matched against file lines to disambiguate when context appears in multiple locations. Use a function name, class name, or any nearby unique text.
- **Fuzzy matching**: Context matching uses a 3-phase pipeline — exact match first, then whitespace-normalized match, then similarity-based matching (≥85% threshold). Short patterns (< 3 lines) use exact matching only to prevent false positives.
- **Atomicity**: Multi-file patches use two-phase commit with shadow files. If any file operation fails, all changes are rolled back — no partial writes.
- **Structured errors**: When context matching fails, error responses include the top-3 closest matches with similarity scores and line numbers, enabling LLMs to self-correct their next attempt.
- **Advisory validation**: After patching, files are optionally validated against language-specific formatters (rustfmt, terraform fmt, python, json.tool, node --check, gofmt, bash -n). Validation issues appear as warnings without blocking the patch.

  **Supported validators:**

  | Language   | Tool                         |
  |------------|------------------------------|
  | Rust       | `rustfmt`                    |
  | Python     | `python -m py_compile`       |
  | Go         | `gofmt`                      |
  | JSON       | `python3 -m json.tool`       |
  | Bash       | `bash -n`                    |
  | JavaScript | `node --check`               |
  | Terraform  | `terraform fmt`              |

## Reading Files

The `batch__exec` tool accepts a `files` param for workspace reads — use it instead of shell `cat`/`grep` for better token efficiency.

- **Glob patterns**: `src/**/*.rs`, `**/*.{ts,tsx}`
- **Line ranges**: `offset` and `limit` parameters for partial reads
- **Symbol extraction**: Extract code symbols (function, struct, class) — language-specific support
- **Language support**: Rust, Python, TypeScript, JavaScript, Go

**Examples** (pass as `files` array in a `batch__exec` call):
- Read all Rust files: `[{"path_or_glob": "**/*.rs"}]`
- Read lines 50-100 of a file: `[{"path_or_glob": "src/main.rs", "offset": 50, "limit": 50}]`
- Extract all function definitions: `[{"path_or_glob": "src/lib.rs", "symbols": ["function"]}]`

Both `patch` and `files` can be passed in a single `batch__exec` call — reads run first (safe/read-only), then the patch is applied.

## Development

Build from source:

```bash
cargo build --release
```

Binary: `target/release/apply-patch-mcp`

Run tests:

```bash
cargo test
```

### Test Suites

| File                          | Coverage                                                        |
|-------------------------------|-----------------------------------------------------------------|
| `tests/integration_test.rs`   | Core patch operations, edge cases (Unicode, empty files, long lines, concurrent shadow collision, multi-op atomicity) |
| `tests/server_test.rs`        | MCP server, `batch__exec` (globs, line ranges, symbol extraction, patch operations), error handling |
| `tests/validator_test.rs`     | Language-specific advisory validation for all 7 supported validators |

## License

MIT
