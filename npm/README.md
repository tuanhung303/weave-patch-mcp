# mcp-weave-patch

MCP server for structured file patching using V4A diffs. Create, update, and delete files in one atomic operation.

## Quick Start

Add to your MCP configuration:

```json
{
  "weave": {
    "command": "npx",
    "args": ["-y", "mcp-weave-patch"]
  }
}
```

That's it. The binary is automatically downloaded on first run.

## Why Use This?

- **One call replaces 5+ separate Edit/Write/Create calls** — saves tokens and round-trips
- **Multi-file changes land atomically** — no broken intermediate states
- **Context-based matching** survives code drift; line numbers don't
- **Standard diff format** — reviewers understand changes instantly

## Build from Source

```bash
cargo build --release
```
