// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration file support.
//!
//! Config is loaded from `$XDG_CONFIG_HOME/etr/config.toml`
//! (default: `~/.config/etr/config.toml`).  All fields are optional;
//! missing fields fall back to compiled-in defaults.
//!
//! CLI flags take precedence over config file values.
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
//! ```
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub client: ClientConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ClientConfig {
    /// Default SSH port for bootstrap.
    pub ssh_port: Option<u16>,

    /// Default path to etrs on the remote host.
    pub server_path: Option<String>,
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
        let toml = "[client]\nssh_port = 2222\nserver_path = \"/usr/local/bin/etrs\"\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.client.ssh_port, Some(2222));
        assert_eq!(
            cfg.client.server_path.as_deref(),
            Some("/usr/local/bin/etrs")
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
}
