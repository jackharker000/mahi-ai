//! MCP server configuration.
//!
//! On disk Mahi reads a Claude-Desktop-compatible `mcp_servers.json`:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "filesystem": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/work"],
//!       "env": { "LOG_LEVEL": "info" }
//!     }
//!   }
//! }
//! ```
//!
//! Each entry becomes one [`McpServerConfig`]; the map key is the server name
//! (which namespaces its tools as `mcp__<name>__<tool>`).

use crate::mcp::McpError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One configured MCP server: a child process Mahi spawns and speaks the MCP
/// stdio transport to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Logical name; namespaces the server's tools as `mcp__<name>__<tool>`.
    pub name: String,
    /// Executable to run (e.g. `npx`, `uvx`, or an absolute path).
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process, as ordered pairs.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl McpServerConfig {
    /// A minimal config (no args, inherited environment only).
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Builder: set the argument vector.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }
}

/// The on-disk file shape (`{ "mcpServers": { "<name>": { ... } } }`).
#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerEntry>,
}

/// One server's on-disk entry (the map value; the key supplies the name).
#[derive(Debug, Deserialize)]
struct ServerEntry {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Parse `mcp_servers.json` contents into a deterministic (name-sorted) list
/// of [`McpServerConfig`].
pub fn parse_mcp_config(contents: &str) -> Result<Vec<McpServerConfig>, McpError> {
    let file: McpConfigFile = serde_json::from_str(contents)
        .map_err(|e| McpError::Config(format!("failed to parse mcp_servers.json: {e}")))?;
    Ok(file
        .mcp_servers
        .into_iter()
        .map(|(name, entry)| McpServerConfig {
            name,
            command: entry.command,
            args: entry.args,
            // BTreeMap iteration is key-sorted -> stable env order.
            env: entry.env.into_iter().collect(),
        })
        .collect())
}

/// Load and parse an `mcp_servers.json` file. A missing file is **not** an
/// error: it yields an empty list, so MCP is simply off until configured.
pub fn load_mcp_config(path: &Path) -> Result<Vec<McpServerConfig>, McpError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_mcp_config(&contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(McpError::Config(format!(
            "failed to read {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_desktop_shape() {
        let json = r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/work"],
                    "env": { "LOG_LEVEL": "info", "API_KEY": "secret" }
                },
                "git": { "command": "uvx", "args": ["mcp-server-git"] }
            }
        }"#;
        let configs = parse_mcp_config(json).expect("valid config");
        assert_eq!(configs.len(), 2);

        // Deterministic, name-sorted: filesystem before git.
        assert_eq!(configs[0].name, "filesystem");
        assert_eq!(configs[0].command, "npx");
        assert_eq!(
            configs[0].args,
            ["-y", "@modelcontextprotocol/server-filesystem", "/work"]
        );
        // env is key-sorted: API_KEY before LOG_LEVEL.
        assert_eq!(
            configs[0].env,
            vec![
                ("API_KEY".to_string(), "secret".to_string()),
                ("LOG_LEVEL".to_string(), "info".to_string()),
            ]
        );

        assert_eq!(configs[1].name, "git");
        assert!(configs[1].env.is_empty());
    }

    #[test]
    fn args_and_env_default_to_empty() {
        let json = r#"{ "mcpServers": { "bare": { "command": "server" } } }"#;
        let configs = parse_mcp_config(json).unwrap();
        assert_eq!(configs.len(), 1);
        assert!(configs[0].args.is_empty());
        assert!(configs[0].env.is_empty());
    }

    #[test]
    fn empty_or_missing_servers_yield_empty_list() {
        assert!(parse_mcp_config("{}").unwrap().is_empty());
        assert!(parse_mcp_config(r#"{ "mcpServers": {} }"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_json_is_a_config_error() {
        let err = parse_mcp_config("{ not json").unwrap_err();
        assert!(matches!(err, McpError::Config(_)));
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let path = std::env::temp_dir().join("mahi-no-such-mcp-config-xyz.json");
        let configs = load_mcp_config(&path).expect("missing file is ok");
        assert!(configs.is_empty());
    }

    #[test]
    fn load_reads_a_real_file() {
        let dir = std::env::temp_dir().join(format!("mahi-mcp-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp_servers.json");
        std::fs::write(
            &path,
            r#"{ "mcpServers": { "echo": { "command": "cat" } } }"#,
        )
        .unwrap();
        let configs = load_mcp_config(&path).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "echo");
        assert_eq!(configs[0].command, "cat");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn builder_helpers_work() {
        let cfg = McpServerConfig::new("git", "uvx").with_args(["mcp-server-git"]);
        assert_eq!(cfg.name, "git");
        assert_eq!(cfg.args, ["mcp-server-git"]);
        assert!(cfg.env.is_empty());
    }
}
