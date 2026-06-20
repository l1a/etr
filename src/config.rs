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
//! # Example config file
//!
//! ```toml
//! [client]
//! # Default SSH port
//! ssh_port = 22
//!
//! # Path to etrs on remote hosts
//! server_path = "etrs"
//!
//! [server]
//! # How long (seconds) etrs keeps a session alive while the client is gone.
//! # Override with --reconnect-timeout or ETR_SERVER_NETWORK_TMOUT env var.
//! reconnect_timeout = 1800
//! ```
use serde::Deserialize;

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
}
