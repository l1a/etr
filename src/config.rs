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
//! # Cipher suites in preference order.
//! # Available: ml-kem-1024, ml-kem-768, x25519-aes, x25519-chacha
//! ciphers = ["ml-kem-1024", "x25519-aes"]
//!
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
    /// Cipher suite preference list (short names: ml-kem-1024, ml-kem-768,
    /// x25519-aes, x25519-chacha).  Empty = use compiled-in defaults.
    #[serde(default)]
    pub ciphers: Vec<String>,

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
