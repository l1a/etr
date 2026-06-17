// SPDX-License-Identifier: GPL-3.0-or-later
//! `etr` — Eternal Terminal in Rust.
//!
//! This crate is the shared library used by both the `etr` client and `etrs`
//! server.  It is organised into five modules:
//!
//! - [`crypto`]: Cipher suite negotiation, KEM key exchange (X25519 / ML-KEM),
//!   and AEAD session encryption (AES-256-GCM / ChaCha20-Poly1305).
//! - [`protocol`]: Versioned UDP wire format — fixed [`protocol::PacketHeader`]
//!   plus protobuf [`protocol::Envelope`] with stream multiplexing.
//! - [`session`]: Per-session and per-stream state that survives reconnections.
//! - [`transport`]: Async UDP send/receive helpers.
//! - [`handshake`]: 1-RTT client and server handshake state machines.
pub mod config;
pub mod crypto;
pub mod forward;
pub mod handshake;
pub mod protocol;
pub mod session;
pub mod transport;
