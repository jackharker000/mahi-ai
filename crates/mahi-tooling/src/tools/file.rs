//! Built-in filesystem tools: `file_read`, `file_write`, `file_edit`,
//! `file_search`. All paths are scoped to an allowed root directory; any
//! attempt to escape it (lexically or via symlink) is a sandbox violation.

use crate::tool::{contract_error, events, ok_result, parse_args, tool_error, Tool};
use async_trait::async_trait;
use mahi_contracts::error::{ContractError, ToolError};
use mahi_contracts::tooling::{
    DestructiveLevel, ToolCategory, ToolDescriptor, ToolEvent, ToolEventStream,
};
use mahi_contracts::types::ComputeMode;
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Modes in which the local file tools are offered. Hosted (cloud) compute
/// has no access to the local filesystem (mode matrix in
/// `docs/backend/domains/02-tooling-integrations.md`).
const FILE_MODES: [ComputeMode; 3] = [
    ComputeMode::OnDevice,
    ComputeMode::MacLan,
    ComputeMode::MacRemote,
];

/// Maximum bytes returned by `file_read` / scanned per file by `file_search`.
const MAX_READ_BYTES: usize = 256 * 1024;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SEARCH_VISITED: usize = 10_000;

/// Lexically normalize a path: resolves `.` and `..` without touching the fs.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A root directory all file/shell tools are confined to.
#[derive(Debug, Clone)]
pub(crate) struct FileScope {
    root: PathBuf,
}

impl FileScope {
    /// Build a scope rooted at `root`. The root is canonicalized when it
    /// exists so symlinked roots (e.g. tempdirs) compare correctly.
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        let raw: PathBuf = root.into();
        let abs = if raw.is_absolute() {
            raw
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&raw))
                .unwrap_or(raw)
        };
        let root = abs.canonicalize().unwrap_or_else(|_| normalize(&abs));
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a user-supplied path against the root and verify it cannot
    /// escape, including through symlinks for paths that already exist.
    pub(crate) fn resolve(&self, raw: &str) -> Result<PathBuf, ContractError> {
        let p = Path::new(raw);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(&self.root) {
            return Err(ToolError::SandboxViolation.into());
        }
        if normalized.exists() {
            let canonical = normalized.canonicalize().map_err(|e| {
                ContractError::Tool(ToolError::Execution {
                    message: format!("failed to canonicalize {}: {e}", normalized.display()),
                })
            })?;
            if !canonical.starts_with(&self.root) {
                return Err(ToolError::SandboxViolation.into());
            }
            return Ok(canonical);
        }
        Ok(normalized)
    }
}

fn path_schema_property() -> serde_json::Value {
    json!({ "type": "string", "description": "Path relative to the allowed root (absolute paths must stay inside it)" })
}

// ---------------------------------------------------------------------------
// file_read
// ---------------------------------------------------------------------------

pub struct FileReadTool {
    scope: FileScope,
}

impl FileReadTool {
    pub(crate) fn new(scope: FileScope) -> Self {
        Self { scope }
    }
}

#[derive(Deserialize)]
struct FileReadArgs {
    path: String,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[async_trait]
impl Tool for FileReadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "file_read".to_string(),
            display_name: "Read File".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: FILE_MODES.to_vec(),
            required_permissions: vec!["fs.read".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": path_schema_property(),
                    "max_bytes": { "type": "integer", "description": "Optional cap on returned bytes" }
                },
                "required": ["path"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "bytes": { "type": "integer" }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: FileReadArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        let path = match self.scope.resolve(&args.path) {
            Ok(p) => p,
            Err(e) => return contract_error(e),
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return tool_error(format!("failed to read {}: {e}", path.display()), false),
        };
        let cap = args.max_bytes.unwrap_or(MAX_READ_BYTES);
        let truncated = content.len() > cap;
        let mut body = content;
        if truncated {
            // Truncate on a char boundary.
            let mut end = cap;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
        }
        let bytes = body.len();
        events(vec![Ok(ToolEvent::Result {
            output: json!({ "path": path.display().to_string(), "content": body, "bytes": bytes }),
            truncated,
        })])
    }
}

// ---------------------------------------------------------------------------
// file_write
// ---------------------------------------------------------------------------

pub struct FileWriteTool {
    scope: FileScope,
}

impl FileWriteTool {
    pub(crate) fn new(scope: FileScope) -> Self {
        Self { scope }
    }
}

#[derive(Deserialize)]
struct FileWriteArgs {
    path: String,
    content: String,
    /// Create missing parent directories (default true).
    #[serde(default = "default_true")]
    create_dirs: bool,
}

fn default_true() -> bool {
    true
}

#[async_trait]
impl Tool for FileWriteTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "file_write".to_string(),
            display_name: "Write File".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: FILE_MODES.to_vec(),
            required_permissions: vec!["fs.write".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": path_schema_property(),
                    "content": { "type": "string" },
                    "create_dirs": { "type": "boolean", "default": true }
                },
                "required": ["path", "content"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "bytes_written": { "type": "integer" }
                }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: FileWriteArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        let path = match self.scope.resolve(&args.path) {
            Ok(p) => p,
            Err(e) => return contract_error(e),
        };
        if args.create_dirs {
            if let Some(parent) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return tool_error(
                        format!("failed to create parent dirs for {}: {e}", path.display()),
                        false,
                    );
                }
            }
        }
        let bytes_written = args.content.len();
        if let Err(e) = tokio::fs::write(&path, args.content).await {
            return tool_error(format!("failed to write {}: {e}", path.display()), false);
        }
        ok_result(json!({
            "path": path.display().to_string(),
            "bytes_written": bytes_written
        }))
    }
}

// ---------------------------------------------------------------------------
// file_edit
// ---------------------------------------------------------------------------

pub struct FileEditTool {
    scope: FileScope,
}

impl FileEditTool {
    pub(crate) fn new(scope: FileScope) -> Self {
        Self { scope }
    }
}

#[derive(Deserialize)]
struct FileEditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for FileEditTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "file_edit".to_string(),
            display_name: "Edit File".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: FILE_MODES.to_vec(),
            required_permissions: vec!["fs.write".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": path_schema_property(),
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean", "default": false }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "replacements": { "type": "integer" }
                }
            }),
            requires_approval: true,
            destructive_level: DestructiveLevel::High,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: FileEditArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.old_string.is_empty() {
            return tool_error("old_string must not be empty", false);
        }
        if args.old_string == args.new_string {
            return tool_error("old_string and new_string are identical", false);
        }
        let path = match self.scope.resolve(&args.path) {
            Ok(p) => p,
            Err(e) => return contract_error(e),
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return tool_error(format!("failed to read {}: {e}", path.display()), false),
        };
        let occurrences = content.matches(&args.old_string).count();
        if occurrences == 0 {
            return tool_error(format!("old_string not found in {}", path.display()), false);
        }
        if occurrences > 1 && !args.replace_all {
            return tool_error(
                format!(
                    "old_string matches {occurrences} times in {}; pass replace_all=true or a more specific old_string",
                    path.display()
                ),
                false,
            );
        }
        let (updated, replacements) = if args.replace_all {
            (
                content.replace(&args.old_string, &args.new_string),
                occurrences,
            )
        } else {
            (content.replacen(&args.old_string, &args.new_string, 1), 1)
        };
        if let Err(e) = tokio::fs::write(&path, updated).await {
            return tool_error(format!("failed to write {}: {e}", path.display()), false);
        }
        ok_result(json!({
            "path": path.display().to_string(),
            "replacements": replacements
        }))
    }
}

// ---------------------------------------------------------------------------
// file_search
// ---------------------------------------------------------------------------

pub struct FileSearchTool {
    scope: FileScope,
}

impl FileSearchTool {
    pub(crate) fn new(scope: FileScope) -> Self {
        Self { scope }
    }
}

#[derive(Deserialize)]
struct FileSearchArgs {
    /// Substring matched against file names and file contents.
    query: String,
    /// Optional subdirectory (within the allowed root) to search.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for FileSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            id: "file_search".to_string(),
            display_name: "Search Files".to_string(),
            category: ToolCategory::BuiltIn,
            available_in_modes: FILE_MODES.to_vec(),
            required_permissions: vec!["fs.read".to_string()],
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring matched against file names and UTF-8 file contents" },
                    "path": path_schema_property(),
                    "max_results": { "type": "integer", "default": 50 }
                },
                "required": ["query"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "matches": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": ["integer", "null"] },
                                "text": { "type": ["string", "null"] }
                            }
                        }
                    }
                }
            }),
            requires_approval: false,
            destructive_level: DestructiveLevel::Low,
        }
    }

    async fn run(&self, args: serde_json::Value, _cancel: CancellationToken) -> ToolEventStream {
        let args: FileSearchArgs = match parse_args(args) {
            Ok(a) => a,
            Err(msg) => return tool_error(msg, false),
        };
        if args.query.is_empty() {
            return tool_error("query must not be empty", false);
        }
        let start = match args.path.as_deref() {
            Some(p) => match self.scope.resolve(p) {
                Ok(p) => p,
                Err(e) => return contract_error(e),
            },
            None => self.scope.root().to_path_buf(),
        };
        let max_results = args.max_results.unwrap_or(50).max(1);
        let query = args.query;

        // Synchronous walk on a blocking thread: simple and bounded.
        let result = tokio::task::spawn_blocking(move || search_tree(&start, &query, max_results))
            .await
            .map_err(|e| format!("search task failed: {e}"));
        match result {
            Ok((matches, truncated)) => events(vec![Ok(ToolEvent::Result {
                output: json!({ "matches": matches }),
                truncated,
            })]),
            Err(msg) => tool_error(msg, true),
        }
    }
}

/// Walk `start` recursively, matching `query` against file names and UTF-8
/// contents. Returns `(matches, truncated)`.
fn search_tree(start: &Path, query: &str, max_results: usize) -> (Vec<serde_json::Value>, bool) {
    let mut matches = Vec::new();
    let mut stack = vec![start.to_path_buf()];
    let mut visited = 0usize;

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if matches.len() >= max_results || visited >= MAX_SEARCH_VISITED {
                return (matches, true);
            }
            visited += 1;
            let path = entry.path();
            // Never follow symlinks out of the scope.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(query) {
                matches.push(json!({
                    "path": path.display().to_string(),
                    "line": null,
                    "text": null
                }));
                if matches.len() >= max_results {
                    return (matches, true);
                }
            }
            // Content scan, capped by size and valid UTF-8 only.
            let small_enough = entry
                .metadata()
                .map(|m| m.len() <= MAX_SEARCH_FILE_BYTES)
                .unwrap_or(false);
            if !small_enough {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (idx, line) in content.lines().enumerate() {
                if line.contains(query) {
                    let preview: String = line.chars().take(200).collect();
                    matches.push(json!({
                        "path": path.display().to_string(),
                        "line": idx + 1,
                        "text": preview
                    }));
                    if matches.len() >= max_results {
                        return (matches, true);
                    }
                }
            }
        }
    }
    (matches, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dot_and_dotdot() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn scope_rejects_lexical_escape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = FileScope::new(dir.path());
        let err = scope.resolve("../outside.txt").expect_err("must escape");
        assert!(matches!(
            err,
            ContractError::Tool(ToolError::SandboxViolation)
        ));
    }

    #[test]
    fn scope_rejects_absolute_path_outside_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = FileScope::new(dir.path());
        let err = scope.resolve("/etc/hostname").expect_err("must escape");
        assert!(matches!(
            err,
            ContractError::Tool(ToolError::SandboxViolation)
        ));
    }

    #[test]
    fn scope_accepts_inside_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = FileScope::new(dir.path());
        let resolved = scope.resolve("sub/file.txt").expect("inside path ok");
        assert!(resolved.starts_with(scope.root()));
    }

    #[cfg(unix)]
    #[test]
    fn scope_rejects_symlink_escape() {
        let outside = tempfile::tempdir().expect("outside dir");
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret").expect("write secret");

        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");

        let scope = FileScope::new(dir.path());
        let err = scope
            .resolve("link.txt")
            .expect_err("symlink must not escape");
        assert!(matches!(
            err,
            ContractError::Tool(ToolError::SandboxViolation)
        ));
    }
}
