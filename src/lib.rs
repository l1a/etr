// SPDX-License-Identifier: GPL-3.0-or-later
//! `etr` — Eternal Terminal in Rust.
//!
//! This crate provides the shared library used by both the `etr` client and
//! `etrs` server. It is split into three modules:
//!
//! - [`crypto`]: AES-256-GCM session encryption and HKDF key derivation.
//! - [`protocol`]: Wire-format packet types shared between client and server.
//! - [`session`]: Persistent session state and length-prefixed TCP framing helpers.
pub mod crypto;
pub mod protocol;
pub mod session;
