pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_MAP_DEPTH: usize = 3;
pub const DEFAULT_MAP_OUTPUT_LIMIT: usize = 6000;
pub const DEFAULT_READ_LINE_LIMIT: usize = 1000;

pub const PATCH_PARAM_DESCRIPTION: &str = "Compact patch text containing view/read, map, create, write, update, move, and delete operations. Use weave syntax wrapped in === begin / === end, or paste native apply_patch-style *** Begin Patch blocks.";
pub const THRESHOLD_PARAM_DESCRIPTION: &str = "Optional fuzzy matching threshold (0.0-1.0). Higher values (e.g., 0.97) require stricter matching. Default: 0.97.";
pub const DRY_RUN_PARAM_DESCRIPTION: &str =
    "When true, preview the batch against staged state without committing filesystem changes.";
pub const RESPONSE_FORMAT_PARAM_DESCRIPTION: &str = "Response format. Use 'text' for the human-readable summary (default) or 'json' for a machine-readable JSON payload in the tool text response.";

pub const PATCH_EXEC_DESCRIPTION: &str = include_str!("patch_exec_description.txt");

pub fn readme_defaults_line() -> String {
    format!("Defaults: `depth={DEFAULT_MAP_DEPTH}`, `limit={DEFAULT_MAP_OUTPUT_LIMIT}` chars.")
}

pub fn server_instructions() -> String {
    format!(
        "Structured file patching MCP server. One tool: patch__exec.\n\n\
         Migration-friendly format: view/read, map, create, write, update, move, delete in === begin / === end, or native apply_patch blocks.\n\n\
         View/read: view|read <path> [symbols=a,b] [language=rust] [offset=0] [limit=100] [start=1] [end=20]\n\n\
         Create/write: create|write <path> followed by raw text or apply_patch-style + lines. Native *** Begin Patch blocks are accepted directly.\n\n\
         Agent controls: dry_run previews the batch without committing. response_format=json returns a machine-readable JSON payload.\n\n\
         Defaults: map depth={DEFAULT_MAP_DEPTH}, map limit={DEFAULT_MAP_OUTPUT_LIMIT} chars, read truncation={DEFAULT_READ_LINE_LIMIT} lines.\n\n\
         Security: no symlinks. Relative paths (including ../), absolute paths, and ~ home expansion are allowed.\n\n\
         Validators: rustfmt, gofmt, py_compile, json.tool, bash -n, node --check, terraform fmt (advisory)."
    )
}
