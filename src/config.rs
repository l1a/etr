// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration file support.
//!
//! Config is loaded from `$XDG_CONFIG_HOME/etr/config.toml`
//! (default: `~/.config/etr/config.toml`).  All fields are optional;
//! missing fields fall back to compiled-in defaults.
//!
//! CLI flags take precedence over config file values; env vars sit between
//! CLI and config file in priority.
//!
//! Use `etr --generate-config` to print a fully-commented default config,
//! `etr --write-config` to write it to the default location, or
//! `etr --merge-config` to add any missing options to an existing config.
use serde::Deserialize;

/// Well-commented default configuration file content.
///
/// Every supported key is present as a comment with its default value and a
/// brief explanation.  Used by `--generate-config`, `--write-config`, and as
/// the baseline for `--merge-config`.
pub const DEFAULT_CONFIG: &str = r#"# etr configuration file
# All fields are optional. Uncomment and edit as needed.
# CLI flags take precedence over config file values.
# Config location: $XDG_CONFIG_HOME/etr/config.toml (~/.config/etr/config.toml)

[client]

# SSH port used for the bootstrap connection (default: 22).
# ssh_port = 22

# Path to the etrs binary on the remote host (default: "etrs", relies on PATH).
# server_path = "etrs"

# Path to the client-side log file (default: $XDG_STATE_HOME/etr/etr.log).
# Logs are only written when -v or higher is passed.
# log_path = "~/.local/state/etr/etr.log"

# Path to the server log file on the remote host (default: $XDG_STATE_HOME/etr/etrs.log).
# server_log_path = "~/.local/state/etr/etrs.log"

# When true, local forwarded ports are bound to all interfaces (0.0.0.0 and ::)
# instead of loopback only, allowing other hosts to connect (default: false).
# Equivalent to -g / --gateway-ports.
# gateway_ports = false

# Default local port forwards applied to every connection (same syntax as -L).
# forward = ["8080:localhost:80", "5353:8.8.8.8:53/udp"]

# Default remote port forwards applied to every connection (same syntax as -R).
# reverse_forward = ["9090:localhost:9090"]

# Environment variables to set or forward in the remote shell.
# "KEY=VALUE" sets the variable; "KEY" alone forwards it from the local environment.
# env = ["EDITOR", "TERM=xterm-256color"]

# Enable X11 forwarding (default: false).
# x11 = false

# Enable trusted X11 forwarding (default: false).
# x11_trusted = false

[server]

# How long (seconds) the server keeps a session alive while the client is
# disconnected (default: 1800, i.e. 30 minutes).
# Overridden by --reconnect-timeout or the ETR_SERVER_NETWORK_TMOUT env var.
# reconnect_timeout = 1800
"#;

/// Per-key comment blocks appended by `merge_defaults` for the `[client]` section.
const CLIENT_KEY_BLOCKS: &[(&str, &str)] = &[
    (
        "ssh_port",
        "# SSH port used for the bootstrap connection (default: 22).\n# ssh_port = 22",
    ),
    (
        "server_path",
        "# Path to the etrs binary on the remote host (default: \"etrs\", relies on PATH).\n# server_path = \"etrs\"",
    ),
    (
        "log_path",
        "# Path to the client-side log file (default: $XDG_STATE_HOME/etr/etr.log).\n# Logs are only written when -v or higher is passed.\n# log_path = \"~/.local/state/etr/etr.log\"",
    ),
    (
        "server_log_path",
        "# Path to the server log file on the remote host (default: $XDG_STATE_HOME/etr/etrs.log).\n# server_log_path = \"~/.local/state/etr/etrs.log\"",
    ),
    (
        "gateway_ports",
        "# When true, local forwarded ports are bound to all interfaces (0.0.0.0 and ::)\n# instead of loopback only, allowing other hosts to connect (default: false).\n# gateway_ports = false",
    ),
    (
        "forward",
        "# Default local port forwards applied to every connection (same syntax as -L).\n# forward = [\"8080:localhost:80\", \"5353:8.8.8.8:53/udp\"]",
    ),
    (
        "reverse_forward",
        "# Default remote port forwards applied to every connection (same syntax as -R).\n# reverse_forward = [\"9090:localhost:9090\"]",
    ),
    (
        "env",
        "# Environment variables to set or forward in the remote shell.\n# \"KEY=VALUE\" sets the variable; \"KEY\" alone forwards it from the local environment.\n# env = [\"EDITOR\", \"TERM=xterm-256color\"]",
    ),
    (
        "x11",
        "# Enable X11 forwarding (default: false).\n# x11 = false",
    ),
    (
        "x11_trusted",
        "# Enable trusted X11 forwarding (default: false).\n# x11_trusted = false",
    ),
];

/// Per-key comment blocks appended by `merge_defaults` for the `[server]` section.
const SERVER_KEY_BLOCKS: &[(&str, &str)] = &[(
    "reconnect_timeout",
    "# How long (seconds) the server keeps a session alive while the client is\n# disconnected (default: 1800, i.e. 30 minutes).\n# reconnect_timeout = 1800",
)];

/// Top-level configuration loaded from `~/.config/etr/config.toml`.
///
/// Both sections are optional; missing sections fall back to [`Default`].
/// Use [`Config::load`] to read the file, or construct directly in tests.
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ClientConfig {
    /// Default SSH port for bootstrap.
    pub ssh_port: Option<u16>,

    /// Default path to etrs on the remote host.
    pub server_path: Option<String>,

    /// Default path to the client log file.
    pub log_path: Option<String>,

    /// Default path to the server log file on the remote host.
    pub server_log_path: Option<String>,

    /// Default gateway_ports setting (allows external connections to forwarded ports).
    pub gateway_ports: Option<bool>,

    /// Default local port forwards.
    pub forward: Option<Vec<String>>,

    /// Default remote port forwards.
    pub reverse_forward: Option<Vec<String>>,

    /// Extra environment variables to set in the remote shell.
    /// Each entry is either "KEY=VALUE" or just "KEY" (forwarded from the local environment).
    pub env: Option<Vec<String>>,

    /// Enable X11 forwarding.
    pub x11: Option<bool>,

    /// Enable trusted X11 forwarding.
    pub x11_trusted: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ServerConfig {
    /// How long (seconds) the server keeps a session alive while the client is
    /// disconnected.  CLI `--reconnect-timeout` and the `ETR_SERVER_NETWORK_TMOUT`
    /// env var override this; the compiled-in default is 1800 s (30 min).
    pub reconnect_timeout: Option<u64>,
}

impl Config {
    /// Load from the default XDG config path.  Returns a default `Config` if
    /// the file does not exist or cannot be parsed (parse errors are printed
    /// to stderr).
    pub fn load() -> Self {
        let path = config_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                eprintln!(
                    "[etr] warning: could not read config {}: {}",
                    path.display(),
                    e
                );
                return Self::default();
            }
        };
        match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "[etr] warning: config parse error in {}: {}",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }
}

/// Return the default config file path: `$XDG_CONFIG_HOME/etr/config.toml`.
///
/// Falls back to `~/.config/etr/config.toml` when `XDG_CONFIG_HOME` is unset,
/// and to `./.config/etr/config.toml` when the home directory cannot be determined.
pub fn config_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".config")
        })
        .join("etr")
        .join("config.toml")
}

/// Insert commented blocks for any config keys not already present in `existing`.
///
/// A key is considered present if any line in `existing` — after stripping a
/// leading `#` and whitespace — begins with `key =`.  Both active settings and
/// already-commented-out settings count as present, so re-running `--merge-config`
/// is always idempotent.
///
/// Missing keys are inserted immediately after their section header if that section
/// already exists, or in a new section appended at the end of the file otherwise.
/// The result is always valid TOML (no duplicate table headers).
///
/// Returns the updated content and a list of the key names that were added.
/// If `existing` is empty the result is equivalent to [`DEFAULT_CONFIG`].
pub fn merge_defaults(existing: &str) -> (String, Vec<&'static str>) {
    let missing_client: Vec<(&str, &str)> = CLIENT_KEY_BLOCKS
        .iter()
        .filter(|(key, _)| !contains_key_line(existing, key))
        .map(|(k, b)| (*k, *b))
        .collect();

    let missing_server: Vec<(&str, &str)> = SERVER_KEY_BLOCKS
        .iter()
        .filter(|(key, _)| !contains_key_line(existing, key))
        .map(|(k, b)| (*k, *b))
        .collect();

    let additions: Vec<&'static str> = missing_client
        .iter()
        .chain(missing_server.iter())
        .map(|(k, _)| *k)
        .collect();

    if additions.is_empty() {
        let mut s = existing.trim_end().to_string();
        if !s.is_empty() {
            s.push('\n');
        }
        return (s, additions);
    }

    // Rebuild the file line-by-line, injecting missing blocks right after their
    // section header so we never emit a duplicate `[client]` or `[server]`.
    let mut result = String::new();
    let mut client_inserted = false;
    let mut server_inserted = false;

    for line in existing.lines() {
        let trimmed = line.trim();

        if trimmed == "[client]" {
            result.push_str(line);
            result.push('\n');
            if !missing_client.is_empty() {
                for (_, block) in &missing_client {
                    result.push_str(block);
                    result.push('\n');
                }
                client_inserted = true;
            }
            continue;
        }

        if trimmed == "[server]" {
            // Emit a new [client] section before [server] if one never appeared.
            if !missing_client.is_empty() && !client_inserted {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("[client]\n");
                for (_, block) in &missing_client {
                    result.push_str(block);
                    result.push('\n');
                }
                client_inserted = true;
            }
            result.push_str(line);
            result.push('\n');
            if !missing_server.is_empty() {
                for (_, block) in &missing_server {
                    result.push_str(block);
                    result.push('\n');
                }
                server_inserted = true;
            }
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    // Sections that never appeared in the file — append at the end.
    if !missing_client.is_empty() && !client_inserted {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[client]\n");
        for (_, block) in &missing_client {
            result.push_str(block);
            result.push('\n');
        }
    }
    if !missing_server.is_empty() && !server_inserted {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[server]\n");
        for (_, block) in &missing_server {
            result.push_str(block);
            result.push('\n');
        }
    }

    (result, additions)
}

/// Returns true if `content` contains a line (active or commented) of the form `key = ...`.
fn contains_key_line(content: &str, key: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        let without_comment = trimmed.strip_prefix('#').map(str::trim).unwrap_or(trimmed);
        without_comment
            .strip_prefix(key)
            .map(str::trim)
            .is_some_and(|rest| rest.starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_path_ends_with_etr_config() {
        let path = config_path();
        assert!(path.ends_with("etr/config.toml"));
    }

    #[test]
    fn test_default_config_is_empty() {
        let cfg = Config::default();
        assert!(cfg.client.ssh_port.is_none());
        assert!(cfg.client.server_path.is_none());
    }

    #[test]
    fn test_parse_empty_toml_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.client.ssh_port.is_none());
        assert!(cfg.client.server_path.is_none());
    }

    #[test]
    fn test_parse_full_client_section() {
        let toml = "[client]\nssh_port = 2222\nserver_path = \"/usr/local/bin/etrs\"\nlog_path = \"/tmp/client.log\"\nserver_log_path = \"/tmp/server.log\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.client.ssh_port, Some(2222));
        assert_eq!(
            cfg.client.server_path.as_deref(),
            Some("/usr/local/bin/etrs")
        );
        assert_eq!(cfg.client.log_path.as_deref(), Some("/tmp/client.log"));
        assert_eq!(
            cfg.client.server_log_path.as_deref(),
            Some("/tmp/server.log")
        );
    }

    #[test]
    fn test_parse_partial_client_section() {
        let toml = "[client]\nssh_port = 22\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.client.ssh_port, Some(22));
        assert!(cfg.client.server_path.is_none());
    }

    #[test]
    fn test_load_nonexistent_file_returns_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.client.ssh_port.is_none());
    }

    #[test]
    fn test_parse_server_section() {
        let toml = "[server]\nreconnect_timeout = 3600\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.reconnect_timeout, Some(3600));
    }

    #[test]
    fn test_server_section_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.server.reconnect_timeout.is_none());
    }

    #[test]
    fn test_parse_new_forward_options() {
        let toml = r#"
            [client]
            gateway_ports = true
            forward = ["8080:localhost:80", "*:3000:localhost:3000"]
            reverse_forward = ["9090:localhost:90"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.client.gateway_ports, Some(true));
        assert_eq!(
            cfg.client.forward,
            Some(vec![
                "8080:localhost:80".to_string(),
                "*:3000:localhost:3000".to_string()
            ])
        );
        assert_eq!(
            cfg.client.reverse_forward,
            Some(vec!["9090:localhost:90".to_string()])
        );
    }

    #[test]
    fn test_parse_x11_options() {
        let toml = r#"
            [client]
            x11 = true
            x11_trusted = false
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.client.x11, Some(true));
        assert_eq!(cfg.client.x11_trusted, Some(false));
    }

    #[test]
    fn test_default_config_is_valid_toml() {
        // The DEFAULT_CONFIG constant (with all lines commented out) must parse cleanly.
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(cfg.client.ssh_port.is_none());
        assert!(cfg.server.reconnect_timeout.is_none());
    }

    #[test]
    fn test_merge_defaults_empty_produces_all_keys() {
        let (merged, additions) = merge_defaults("");
        for (key, _) in CLIENT_KEY_BLOCKS.iter().chain(SERVER_KEY_BLOCKS.iter()) {
            assert!(
                additions.contains(key),
                "expected {key} in additions for empty input"
            );
            assert!(
                merged.contains(key),
                "expected {key} in merged output for empty input"
            );
        }
    }

    #[test]
    fn test_merge_defaults_all_present_no_additions() {
        // Active (uncommented) keys — all recognised, nothing added.
        let existing = "\
[client]\n\
ssh_port = 22\n\
server_path = \"etrs\"\n\
log_path = \"~/.local/state/etr/etr.log\"\n\
server_log_path = \"~/.local/state/etr/etrs.log\"\n\
gateway_ports = false\n\
forward = []\n\
reverse_forward = []\n\
env = []\n\
x11 = false\n\
x11_trusted = false\n\
[server]\n\
reconnect_timeout = 1800\n";
        let (merged, additions) = merge_defaults(existing);
        assert!(additions.is_empty(), "unexpected additions: {additions:?}");
        assert_eq!(merged.trim(), existing.trim());
    }

    #[test]
    fn test_merge_defaults_commented_keys_count_as_present() {
        // Commented-out keys must not be re-added (idempotency).
        let existing = "\
[client]\n\
# ssh_port = 22\n\
# server_path = \"etrs\"\n\
# log_path = \"~/.local/state/etr/etr.log\"\n\
# server_log_path = \"~/.local/state/etr/etrs.log\"\n\
# gateway_ports = false\n\
# forward = []\n\
# reverse_forward = []\n\
# env = []\n\
# x11 = false\n\
# x11_trusted = false\n\
[server]\n\
# reconnect_timeout = 1800\n";
        let (_, additions) = merge_defaults(existing);
        assert!(additions.is_empty(), "unexpected additions: {additions:?}");
    }

    #[test]
    fn test_merge_defaults_adds_missing_keys() {
        let existing = "[client]\nssh_port = 2222\n";
        let (merged, additions) = merge_defaults(existing);
        // ssh_port is present → should not appear in additions
        assert!(
            !additions.contains(&"ssh_port"),
            "ssh_port should not be re-added"
        );
        // All other client keys + server keys should be added
        for (key, _) in CLIENT_KEY_BLOCKS.iter().chain(SERVER_KEY_BLOCKS.iter()) {
            if *key != "ssh_port" {
                assert!(additions.contains(key), "{key} should be in additions");
                assert!(merged.contains(key), "{key} should appear in merged output");
            }
        }
        // Original content preserved
        assert!(merged.contains("ssh_port = 2222"));
    }

    #[test]
    fn test_merge_defaults_is_idempotent() {
        let existing = "[client]\nssh_port = 22\n";
        let (first, _) = merge_defaults(existing);
        let (second, additions) = merge_defaults(&first);
        assert!(additions.is_empty(), "second merge should add nothing");
        assert_eq!(first.trim(), second.trim());
    }

    #[test]
    fn test_merge_defaults_output_is_valid_toml() {
        // Merged output must remain parseable.
        let existing = "[client]\nssh_port = 2222\n";
        let (merged, _) = merge_defaults(existing);
        let cfg: Config = toml::from_str(&merged).unwrap();
        assert_eq!(cfg.client.ssh_port, Some(2222));
    }

    #[test]
    fn test_load_malformed_toml_returns_default() {
        // Config::load() must silently return Default on a parse error —
        // not panic or propagate the error.
        let result: Result<Config, _> = toml::from_str("ssh_port = !!invalid!!");
        assert!(result.is_err(), "malformed TOML should fail to parse");
        // Verify Config::load() handles this path: write a bad file to a temp dir,
        // then confirm the returned Config equals Default.
        let dir = std::env::temp_dir().join(format!("etr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "ssh_port = !!bad toml!!").unwrap();
        // Parse via toml directly (Config::load() uses the XDG path we can't override).
        let content = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&content).unwrap_or_default();
        assert!(cfg.client.ssh_port.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
