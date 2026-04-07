# apply-patch-mcp

**v1.0** — Production-ready MCP server for structured file patching using V4A diffs.  
One tool, five operations. Create, read, map, update, and delete files in a single atomic call.

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

For best results, deny Edit/Write in your MCP client settings so the model always uses apply-patch:

Add to your client's deny list:
```json
["Edit(*)", "Write(*)"]
```

This eliminates context pollution from unused tool descriptions and forces consistent patch-based editing.

## Installation

```bash
npx -y mcp-apply-patch
```

No install needed — npx downloads the latest release automatically. The binary is cached at `~/.mcp-apply-patch/bin/` and version-checked on every launch (auto-reinstalls if stale).

### Supported Platforms

| OS | Architecture |
|---|---|
| macOS | arm64, x64 |
| Linux | x64, arm64 |
| Windows | x64 |

## MCP Configuration

### Claude Code

```bash
claude mcp add -s user patch -- npx -y mcp-apply-patch
```

### Qwen Code

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

Add to your config:
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

## Tool: `batch__exec`

One tool, one parameter (`patch`). Five operations available in a single atomic call.

All patches are wrapped in `*** Begin Patch` / `*** End Patch` markers.

### 1. Read a file

```
*** Begin Patch
*** Read File: src/main.rs
*** End Patch
```

**Read with symbol extraction** (Rust, Python, TypeScript, JS, Go):
```
*** Begin Patch
*** Read File: src/lib.rs symbols=Server,handle_request language=rust
*** End Patch
```

**Read with line range**:
```
*** Begin Patch
*** Read File: config.py offset=10 limit=50
*** End Patch
```


**Read multiple files** (batch read):
```
*** Begin Patch
*** Read File: src/main.rs
*** Read File: src/lib.rs
*** Read File: src/config.rs
*** End Patch
```
### 2. Map a directory

Scan directory structure recursively. Returns files with sizes, line counts, and function signatures (`name: start:end`) at depth 1-3. Skips `node_modules`, `.git`, `target`, binaries.

```
*** Begin Patch
*** Map Directory: src/ depth=2
*** End Patch
```

Defaults: `depth=3`, `limit=6000` chars.

### 3. Add a file

```
*** Begin Patch
*** Add File: src/hello.rs
+pub fn hello() { println!("Hello!"); }
*** End Patch
```

### 4. Update a file

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

**Multiple hunks in one file**:
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

**Rename a file** (update + move):
```
*** Begin Patch
*** Update File: src/old.rs
*** Move to: src/new.rs
@@ fn foo
 fn foo() { ... }
*** End Patch
```

**Update multiple files** (batch update):
```
*** Begin Patch
*** Update File: src/api.rs
@@ fn handle
 fn handle() {
-    old();
+    new();
 }
*** Update File: src/db.rs
@@ fn connect
 fn connect() {
-    let url = "old";
+    let url = "new";
 }
*** End Patch
```

### 5. Delete a file

```
*** Begin Patch
*** Delete File: src/deprecated.rs
*** End Patch
```


**Delete multiple files** (batch delete):
```
*** Begin Patch
*** Delete File: src/deprecated1.rs
*** Delete File: src/deprecated2.rs
*** Delete File: src/deprecated3.rs
*** End Patch
```

### Combined: all operations in one call

```
*** Begin Patch
*** Read File: src/main.rs
*** Map Directory: src/ depth=1
*** Update File: src/lib.rs
@@ fn main
 fn main() {
-    old();
+    new();
 }
*** Add File: src/greet.rs
+pub fn greet() { println!("hi"); }
*** Delete File: src/deprecated.rs
*** End Patch
```

Read operations execute first (safe/read-only), then write operations are applied atomically.

## Key Concepts

- **Context matching**: 3-phase pipeline — exact match → whitespace-normalized → fuzzy (≥85% similarity). Short patterns (< 3 lines) use exact matching only to prevent false positives.
- **@@ hints**: Optional substring matched against file lines to disambiguate when context appears in multiple locations. Use a function name, class name, or any nearby unique text.
- **Atomicity**: Multi-file patches use two-phase commit with shadow files. If any operation fails, all changes are rolled back — no partial writes.
- **Structured errors**: When context matching fails, error responses include the top-3 closest matches with similarity scores and line numbers, enabling LLMs to self-correct.
- **Advisory validation**: After patching, files are validated against language-specific formatters. Issues appear as warnings without blocking the patch.

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

- **Limits**: 2MB total output for reads, 512KB per file.
- **Security**: No path traversal (`../`), no symlinks, no absolute paths.

## Architecture

```
┌─────────────┐     ┌──────────┐     ┌──────────┐
│  LLM Client │────▶│ MCP Server│────▶│ Filesystem│
│  (Qwen,     │     │ batch__   │     │ (reads,   │
│  Claude,    │◀────│ exec      │◀────│ writes)   │
│  Gemini)    │     └──────────┘     └──────────┘
└─────────────┘
```

**CI/CD Pipeline** (triggered on push to `main`):

```
test (fmt + clippy + 125 tests)
  ↓
version-bump (auto-increment patch)
  ↓
build (5 platforms: macOS arm64/x64, Linux x64/arm64, Windows x64)
  ↓
release (GitHub Releases with binaries + SHA256)
  ↓
publish (npm registry)
```

If tests fail, nothing is released.

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

### Test Coverage (125 tests)

| Suite | Coverage |
|---|---|
| `tests/integration_test.rs` | Core patch operations, edge cases (Unicode, empty files, long lines, concurrent shadow collision, multi-op atomicity, CRLF) |
| `tests/server_test.rs` | MCP server, `batch__exec` (globs, line ranges, symbol extraction, patch operations, error handling) |
| `tests/validator_test.rs` | All 7 language-specific advisory validators |
| `src/parser.rs` (unit) | V4A patch parsing, auto-wrap missing markers, multi-file, hints, Read/Map specs |
| `src/applier.rs` (unit) | Path validation, fuzzy matching, validators, diff generation, match info |
| `src/reader.rs` (unit) | Line ranges, symbol extraction (Rust/Python/TS/Go), glob expansion |

## License

MIT
