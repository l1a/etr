// SPDX-License-Identifier: GPL-3.0-or-later
//! `etr` — Eternal Terminal in Rust.
//!
//! This crate is the shared library used by both the `etr` client and `etrs`
//! server.  It is organised into four modules:
//!
//! - [`protocol`]: QUIC wire messages — protobuf [`protocol::Envelope`] with
//!   session handshake and stream multiplexing types.
//! - [`quic`]: QUIC endpoint helpers — certificate generation, server/client
//!   config, and framing for control and PTY streams.
//! - [`session`]: Per-session and per-stream state that survives reconnections.
//! - [`config`]: Config-file loading (`~/.config/etr/config.toml`).
//! - [`forward`]: `-L` forwarding spec parser.
pub mod config;
pub mod forward;
pub mod protocol;
pub mod quic;
pub mod session;
