# weave-patch-mcp

[![version](https://img.shields.io/badge/version-0.0.16-blue)](https://www.npmjs.com/package/mcp-weave-patch) [![license](https://img.shields.io/badge/license-MIT-green)](https://opensource.org/licenses/MIT) [![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#supported-platforms)

**One tool. Familiar view/write flows. Zero intermediate states.**

![Weave Patch MCP Architecture](assets/wpm_viz.jpg)

Code drifted? We still find it. Refactor touched 47 files? All-or-nothing writes. No half-applied states left behind.

---

## The Problem

Context drift makes line numbers stale. Token limits force LLM agents into partial commits. Half-finished refactors scatter across repos when agents can't apply atomic changes.

**Blind spots surface before the agent ships.**

Every multi-file edit is a gamble. One call fails, and now the agent is debugging intermediate state. Meanwhile, the context window shrinks.

## Why This Over `view` + `apply_patch`?

| | Copilot CLI `view` + `apply_patch` | weave-patch-mcp |
|---|---|---|
| **Read flow** | `view` | `view` alias or `read` |
| **Whole-file write** | Separate write tool or manual patching | `write` creates or overwrites atomically |
| **Multi-file atomicity** | No — broken intermediate states | Yes — all-or-nothing |
| **Context anchoring** | Line ranges and exact hunks | Pattern matching (survives drift) |
| **Error recovery** | Re-read and retry manually | Closest matches + structured diagnostics |
| **Scale** | Many tool calls across files | One call for many files |
| **Reviewer clarity** | Tool-specific payloads | Standard diffs + per-op summary |

**Atomic patches that survive context drift.** Scale from 1 file to 100+. Same call. Same guarantees.

### Migration from Native File Tools

This MCP tool can replace native file operations with one patch language:

| Native habit | `patch__exec` equivalent |
|---|---|
| `view path` | `view path` or `read path` |
| whole-file write | `write path` |
| `apply_patch` add file | `create path` or `write path` with `+` lines |
| `apply_patch` update file | `update path` |
| full `apply_patch` block | paste it directly; weave translates it |
| delete file | `delete path` |

`write` is the easiest replacement for a traditional whole-file write tool. `create` stays create-only if you want fail-fast behavior when a file already exists.

If you already have a native `*** Begin Patch` block, you can paste it directly into `patch__exec` without translating it first.

For best results, disable overlapping file tools in your MCP client once the agent has migrated to weave-patch:

Add to the client's deny list:
```json
["Edit(*)", "Write(*)"]
```

**Stop burning tokens on unused tool descriptions.**

## Installation

```bash
npx -y mcp-weave-patch
```

No install required. npx fetches the latest release, caches it at `~/.weave-patch/bin/`, and auto-reinstalls on version changes.

### Supported Platforms

| OS | Architecture |
|---|---|
| macOS | arm64, x64 |
| Linux | x64, arm64 |
| Windows | x64 |

## MCP Configuration

### Claude Code

```bash
claude mcp add -s user weave -- npx -y mcp-weave-patch
```

### Qwen Code

Add to `~/.qwen/settings.json` under `mcpServers`:
```json
"weave": {
  "command": "npx",
  "args": ["-y", "mcp-weave-patch"]
}
```

### Gemini CLI

Add to `~/.gemini/settings.json`:
```json
{
  "mcpServers": {
    "weave": {
      "command": "npx",
      "args": ["-y", "mcp-weave-patch"]
    }
  }
}
```

### OpenCode

Add to the config:
```json
{
  "mcpServers": {
    "weave": {
      "command": "npx",
      "args": ["-y", "mcp-weave-patch"]
    }
  }
}
```

## Tool: `patch__exec`

One tool with a required `patch` body and optional `threshold`, `dry_run`, and `response_format` params. Supports `view`, `read`, `map`, `create`, `write`, `update`, `move`, and `delete` in a single atomic call.

Use weave syntax wrapped in `=== begin` / `=== end`, or paste native `*** Begin Patch` blocks directly.

Batches execute in authored order against a staged workspace view. That means `write` then `read` sees staged content immediately, while the final filesystem commit still stays atomic.

**Agent controls**

- `dry_run=true` previews the batch without committing filesystem changes.
- `response_format=json` returns a machine-readable JSON summary with per-op results.

### 1. View or read a file

**Extract just what's needed.** Symbol extraction pulls functions, classes, and structs without reading entire files — token-efficient and surfacing relevant context.

```
=== begin
view src/main.rs
=== end
```

**Read with symbol extraction** (Rust, Python, TypeScript, JS, Go):
```
=== begin
read src/lib.rs symbols=Server,handle_request language=rust
=== end
```

**View with 1-based line range**:
```
=== begin
view config.py start=11 end=60
=== end
```

`offset=` / `limit=` still work. `start=` / `end=` are easier to map from human line numbers.

**Read multiple files** (batch read):
```
=== begin
read src/main.rs
read src/lib.rs
read src/config.rs
=== end
```

### 2. Map a directory

**Know the repo's shape at a glance.** Returns files, sizes, line counts, and function signatures — everything needed to navigate unfamiliar code.

```
=== begin
map src/ depth=2
=== end
```

Defaults: `depth=3`, `limit=6000` chars. File reads truncate at `1000` lines unless you pass `limit`, `symbols`, or `start` / `end`.

### 3. Create a file

**Fail fast when the file already exists.** Use `create` when you want new-file semantics, and `write` when you want create-or-overwrite semantics.

```
=== begin
create src/hello.rs
+pub fn hello() { println!("Hello!"); }
=== end
```

`create` accepts raw file contents or apply_patch-style `+` lines, so agents used to add-file hunks do not need to relearn the body format.

### 4. Write a file

**Whole-file replace, atomically.** This is the closest migration target for a traditional write tool.

```
=== begin
write src/hello.rs
+pub fn hello() { println!("Hello!"); }
=== end
```

### 5. Update a file

**Code drifted? We still find it.** Three-phase matching (exact → whitespace-normalized → fuzzy at 85%+) means context drift won't break the patch.

**Not "match failed" — "closest match at line 42 (87% similar)."**

Structured diagnostics show the top-3 closest matches with line numbers and similarity scores. Self-correct without re-reading the entire file.

Context lines (space-prefixed) anchor the edit. `-` removes, `+` adds.

```
=== begin
update src/lib.rs
@@ impl Server
 pub fn handle(&self, req: Request) -> Response {
-    self.old_handler(req)
+    self.new_handler(req)
 }
=== end
```

**Already have an `apply_patch` block?** Paste it directly:

```diff
*** Begin Patch
*** Update File: src/lib.rs
@@
 fn main() {
-    old();
+    new();
 }
*** End Patch
```

**Multiple hunks in one file**:
```
=== begin
update src/lib.rs
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
=== end
```

**Rename a file** (update + move):
```
=== begin
update src/old.rs
move_to src/new.rs
@@ fn foo
 fn foo() { ... }
=== end
```

**Update multiple files** (batch update):
```
=== begin
update src/api.rs
@@ fn handle
 fn handle() {
-    old();
+    new();
 }
update src/db.rs
@@ fn connect
 fn connect() {
-    let url = "old";
+    let url = "new";
 }
=== end
```

**Markdown / pipe tables (README, `.md`):** Table rows often start with `|` without a leading space. Those lines are treated as **context** so they are not skipped by the parser. For replacements, use `-|` and `+|` so the old and new rows are remove/add lines, for example:

```
=== begin
update README.md
@@
-| Old | value |
+| New | value |
=== end
```

If table rows were not recognized, a hunk could end up with only `+` lines; the matcher then has no anchor and **appends at end of file** (usually wrong).

### 6. Delete a file

**Clean removal.** One call, file gone. No orphaned references left behind.

```
=== begin
delete src/deprecated.rs
=== end
```

**Delete multiple files** (batch delete):
```
=== begin
delete src/deprecated1.rs
delete src/deprecated2.rs
delete src/deprecated3.rs
=== end
```

### Combined: all operations in one call

```
=== begin
read src/main.rs
map src/ depth=1
update src/lib.rs
@@ fn main
 fn main() {
-    old();
+    new();
 }
create src/greet.rs
+pub fn greet() { println!("hi"); }
delete src/deprecated.rs
=== end
```

Operations execute in authored order against staged state, then all writes commit atomically at the end.

## Key Concepts

- **Atomicity** — All-or-nothing writes. No half-applied refactors. Multi-file patches use two-phase commit with shadow files. If any operation fails, everything rolls back.

- **Migration-friendly syntax** — `view` mirrors native file viewers, `write` mirrors whole-file write tools, and `create` / `write` accept apply_patch-style `+` lines.

- **Fuzzy matching** — Code drifted? We still find it. Three-phase pipeline matches context even after edits.

- **Structured errors** — LLM-friendly diagnostics that show exactly where and why matching failed.

### Operation Status

Every operation returns a structured `OpStatus` enum instead of a string:

| Status | Meaning | Example |
|--------|---------|---------|
| `ok` | Operation succeeded | File created/updated/deleted |
| `skipped` | No-op, already in desired state | Delete non-existent file |
| `recoverable_error` | Context match failed, may work with different hunks | Context not found |
| `fatal_error` | File system issue blocks operation | File not found, permission denied |
| `validation_warning` | Syntax/format issue (advisory, non-blocking) | rustfmt warning |

**JSON consumers**: Status is now an enum, not a string. Old `"error"` status is split into `"recoverable_error"` or `"fatal_error"` based on error type.

- **Advisory validation** — Syntax checks after every write, without blocking the patch.

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
- **Security**: Symlinks rejected. Path traversal allowed (tool can access any path with user permissions).

## Use-Case Matrix

| Scenario | Operations | Why It Wins |
|----------|-----------|-------------|
| **Multi-file refactor** | `update` (batch) | One call, 47 files. All-or-nothing. |
| **Exploring unfamiliar code** | `map` + `read symbols=` | Token-efficient symbol extraction |
| **Fixing tests across files** | `read` + `update` + `delete` | Combined read/write in one atomic call |
| **Replacing native file tools** | `view` + `write` + `update` | Familiar flow, fewer tool switches |
| **Deleting deprecated paths** | `delete` (batch) | Clean removal, no partial states |
| **Renaming during refactor** | `update` + `move_to` | Rename and edit atomically |
| **Adding new modules** | `create` (batch) | Create multiple files without tool-switching |

## Architecture

**One tool. One parameter. Everything else is handled.**

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  LLM Client  │────▶│  MCP Server  │────▶│  Filesystem  │
│  (Claude,    │     │ patch__exec  │     │ (reads,      │
│  Qwen, etc.) │◀────│              │◀────│ writes)      │
└──────────────┘     └──────────────┘     └──────────────┘
```

**CI/CD Pipeline** (triggered on push to `main`):

**175 tests. Comprehensive test coverage. If it fails, nothing ships.**

```
test (fmt + clippy + 175 tests)
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

Binary: `target/release/weave-patch-mcp`

Run tests:

```bash
cargo test
```

### Test Coverage (175 tests)

| Suite | Coverage |
|---|---|
| `tests/integration_test.rs` | Core patch operations, edge cases (Unicode, empty files, long lines, concurrent shadow collision, multi-op atomicity, CRLF, markdown pipe rows) |
| `tests/server_test.rs` | MCP server, `patch__exec` (globs, line ranges, symbol extraction, patch operations, error handling) |
| `tests/validator_test.rs` | All 7 language-specific advisory validators |
| `src/parser.rs` (unit) | Compact syntax patch parsing, auto-wrap missing markers, multi-file, hints, Read/Map specs |
| `src/applier.rs` (unit) | Path validation, fuzzy matching, validators, diff generation, match info |
| `src/reader.rs` (unit) | Line ranges, symbol extraction (Rust/Python/TS/Go), glob expansion |

## License

MIT
