// SPDX-License-Identifier: GPL-3.0-or-later
//! Session state: per-session bookkeeping that survives QUIC reconnections.
//!
//! A [`SessionState`] lives for the lifetime of a logical session (potentially
//! spanning many QUIC reconnects).  It owns all [`StreamState`] instances.
//! QUIC provides reliable in-order delivery within a connection, so
//! [`StreamState`] is used only for cross-connection replay, not gap detection.
pub mod stream;

pub use stream::{StreamLifecycle, StreamState};

use std::collections::HashMap;

use crate::protocol::StreamType;

/// Per-session state shared between connection setup and data-plane code.
pub struct SessionState {
    /// 16-byte session identifier; stable across reconnections.
    pub session_id: [u8; 16],
    /// Passkey bootstrapped via SSH; used to authenticate reconnecting clients.
    pub passkey: String,
    /// Per-stream state indexed by stream ID.
    pub streams: HashMap<u32, StreamState>,
}

impl SessionState {
    /// Create a new session with stream 0 (terminal PTY) pre-opened.
    pub fn new(session_id: [u8; 16], passkey: String) -> Self {
        let mut state = Self {
            session_id,
            passkey,
            streams: HashMap::new(),
        };
        state
            .streams
            .insert(0, StreamState::new(0, StreamType::Terminal));
        state
    }

    /// Open a new stream and return a mutable reference to it.
    pub fn open_stream(&mut self, stream_id: u32, stream_type: StreamType) -> &mut StreamState {
        self.streams
            .entry(stream_id)
            .or_insert_with(|| StreamState::new(stream_id, stream_type))
    }

    /// Return an immutable reference to the stream with the given ID, or `None`.
    pub fn stream(&self, stream_id: u32) -> Option<&StreamState> {
        self.streams.get(&stream_id)
    }

    /// Return a mutable reference to the stream with the given ID, or `None`.
    pub fn stream_mut(&mut self, stream_id: u32) -> Option<&mut StreamState> {
        self.streams.get_mut(&stream_id)
    }

    /// Transition stream `stream_id` to [`StreamLifecycle::Closed`].
    ///
    /// No-op if the stream does not exist.
    pub fn close_stream(&mut self, stream_id: u32) {
        if let Some(s) = self.streams.get_mut(&stream_id) {
            s.lifecycle = StreamLifecycle::Closed;
        }
    }

    /// Snapshot of each stream's last-received seq for inclusion in handshake messages.
    ///
    /// The value is `next_in_seq - 1`, which equals the highest seq number the
    /// peer has successfully delivered to us.  Returns nothing for a stream that
    /// has received no messages yet (`next_in_seq == 1` → `last == 0`, which
    /// tells the peer "replay everything").
    pub fn last_received_map(&self) -> HashMap<u32, u64> {
        self.streams
            .iter()
            .filter_map(|(&id, s)| {
                let last = s.next_in_seq.checked_sub(1)?;
                Some((id, last))
            })
            .collect()
    }

    /// Apply the peer's ack map: trim each stream's send history.
    pub fn apply_server_acks(&mut self, server_acks: &HashMap<u32, u64>) {
        for (&stream_id, &ack_seq) in server_acks {
            if let Some(s) = self.streams.get_mut(&stream_id) {
                s.acknowledge_up_to(ack_seq);
            }
        }
    }

    /// Collect all replay packets needed after a reconnect, keyed by stream.
    ///
    /// `peer_last_received` maps stream_id → highest seq the peer has seen.
    pub fn collect_replays(
        &self,
        peer_last_received: &HashMap<u32, u64>,
    ) -> HashMap<u32, Vec<(u64, Vec<u8>)>> {
        self.streams
            .iter()
            .filter_map(|(&id, s)| {
                let last = peer_last_received.get(&id).copied().unwrap_or(0);
                let replays = s.replay_from(last);
                if replays.is_empty() {
                    None
                } else {
                    Some((id, replays))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> SessionState {
        SessionState::new([0u8; 16], "passkey".to_string())
    }

    #[test]
    fn test_stream_zero_exists() {
        let s = make_state();
        assert!(s.stream(0).is_some());
        assert_eq!(s.stream(0).unwrap().stream_type, StreamType::Terminal);
    }

    #[test]
    fn test_open_and_close_stream() {
        let mut s = make_state();
        s.open_stream(1, StreamType::PortForward);
        assert!(s.stream(1).is_some());
        s.close_stream(1);
        assert_eq!(s.stream(1).unwrap().lifecycle, StreamLifecycle::Closed);
    }

    #[test]
    fn test_last_received_map() {
        let mut s = make_state();
        s.stream_mut(0).unwrap().next_in_seq = 6; // received up to seq 5
        let map = s.last_received_map();
        assert_eq!(map[&0], 5);
    }

    #[test]
    fn test_collect_replays() {
        let mut s = make_state();
        let st = s.stream_mut(0).unwrap();
        st.record_send(1, b"a".to_vec());
        st.record_send(2, b"b".to_vec());
        st.record_send(3, b"c".to_vec());

        let peer_acks = [(0u32, 1u64)].into();
        let replays = s.collect_replays(&peer_acks);
        let stream0 = &replays[&0];
        assert_eq!(stream0.len(), 2);
        assert_eq!(stream0[0].0, 2);
        assert_eq!(stream0[1].0, 3);
    }

    #[test]
    fn test_close_stream_nonexistent_noop() {
        let mut s = make_state();
        s.close_stream(99);
        assert!(s.stream(99).is_none());
    }

    #[test]
    fn test_apply_server_acks_unknown_stream_ignored() {
        let mut s = make_state();
        let acks = [(5u32, 100u64)].into();
        s.apply_server_acks(&acks);
        assert!(s.stream(5).is_none());
    }

    #[test]
    fn test_last_received_map_nothing_received() {
        let s = make_state();
        let map = s.last_received_map();
        assert_eq!(map[&0], 0);
    }

    #[test]
    fn test_last_received_map_after_receive() {
        let mut s = make_state();
        s.stream_mut(0).unwrap().next_in_seq = 4;
        let map = s.last_received_map();
        assert_eq!(map[&0], 3);
    }

    #[test]
    fn test_collect_replays_empty_peer_map_returns_all() {
        let mut s = make_state();
        let st = s.stream_mut(0).unwrap();
        st.record_send(1, b"x".to_vec());
        st.record_send(2, b"y".to_vec());

        let replays = s.collect_replays(&HashMap::new());
        let r = &replays[&0];
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, 1);
        assert_eq!(r[1].0, 2);
    }

    #[test]
    fn test_collect_replays_empty_history_omitted() {
        let s = make_state();
        let replays = s.collect_replays(&HashMap::new());
        assert!(replays.is_empty());
    }

    #[test]
    fn test_stream_unknown_returns_none() {
        let s = make_state();
        assert!(s.stream(42).is_none());
    }

    #[test]
    fn test_open_stream_idempotent() {
        let mut s = make_state();
        s.open_stream(2, StreamType::PortForward);
        s.open_stream(2, StreamType::PortForward);
        assert!(s.streams.contains_key(&2));
        assert_eq!(s.streams.iter().filter(|(id, _)| **id == 2).count(), 1);
    }
}
