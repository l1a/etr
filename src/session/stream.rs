// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-stream state: ordered delivery, send history, and lifecycle.
use std::collections::VecDeque;

use crate::protocol::StreamType;

/// Lifecycle of a single multiplexed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLifecycle {
    /// Stream has been requested but the peer has not yet acknowledged it.
    Opening,
    /// Stream is fully established and data flows in both directions.
    Open,
    /// A close has been requested; waiting for in-flight data to drain.
    Closing,
    /// Stream is fully closed; the entry may be removed from the session map.
    Closed,
}

/// State for one logical stream within a session.
///
/// Stream 0 is always the terminal PTY.  Higher-numbered streams are
/// port-forward connections, each with an independent sequence-number
/// space so that a slow forward cannot stall the terminal.
pub struct StreamState {
    pub stream_id: u32,
    pub stream_type: StreamType,
    /// Next sequence number to assign to an outgoing [`StreamData`] packet.
    pub next_out_seq: u64,
    /// Next expected incoming sequence number (in-order delivery cursor).
    pub next_in_seq: u64,
    /// Ring-buffer of `(seq_num, payload)` for replay on reconnect.
    send_history: VecDeque<(u64, Vec<u8>)>,
    max_history: usize,
    pub lifecycle: StreamLifecycle,
}

impl StreamState {
    /// Create a new stream in the [`StreamLifecycle::Open`] state with sequence
    /// numbers starting at 1 (0 is reserved as "nothing received yet").
    pub fn new(stream_id: u32, stream_type: StreamType) -> Self {
        Self {
            stream_id,
            stream_type,
            next_out_seq: 1,
            next_in_seq: 1,
            send_history: VecDeque::new(),
            max_history: 10_000,
            lifecycle: StreamLifecycle::Open,
        }
    }

    /// Record a sent payload in the history buffer.
    pub fn record_send(&mut self, seq_num: u64, payload: Vec<u8>) {
        self.send_history.push_back((seq_num, payload));
        if self.send_history.len() > self.max_history {
            self.send_history.pop_front();
        }
    }

    /// Discard history entries the peer has acknowledged.
    pub fn acknowledge_up_to(&mut self, ack_seq: u64) {
        while let Some(&(seq, _)) = self.send_history.front() {
            if seq <= ack_seq {
                self.send_history.pop_front();
            } else {
                break;
            }
        }
    }

    /// Return all history entries with `seq_num > peer_last_received` for replay.
    pub fn replay_from(&self, peer_last_received: u64) -> Vec<(u64, Vec<u8>)> {
        self.send_history
            .iter()
            .filter(|&&(seq, _)| seq > peer_last_received)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stream() -> StreamState {
        let mut s = StreamState::new(0, StreamType::Terminal);
        s.max_history = 4;
        s
    }

    #[test]
    fn test_record_and_evict() {
        let mut s = make_stream();
        for i in 1u64..=5 {
            s.record_send(i, vec![i as u8]);
        }
        // max_history=4, so seq 1 evicted
        assert_eq!(s.send_history.len(), 4);
        assert_eq!(s.send_history.front().unwrap().0, 2);
    }

    #[test]
    fn test_acknowledge_trims_history() {
        let mut s = make_stream();
        for i in 1u64..=4 {
            s.record_send(i, vec![i as u8]);
        }
        s.acknowledge_up_to(2);
        assert_eq!(s.send_history.len(), 2);
        assert_eq!(s.send_history.front().unwrap().0, 3);
    }

    #[test]
    fn test_replay_from() {
        let mut s = make_stream();
        for i in 1u64..=4 {
            s.record_send(i, vec![i as u8]);
        }
        let replays = s.replay_from(2);
        assert_eq!(replays.len(), 2);
        assert_eq!(replays[0].0, 3);
        assert_eq!(replays[1].0, 4);
    }

    #[test]
    fn test_replay_from_zero_on_empty_history() {
        let s = make_stream();
        assert!(s.replay_from(0).is_empty());
    }

    #[test]
    fn test_replay_from_zero_returns_all() {
        let mut s = make_stream();
        s.record_send(1, b"a".to_vec());
        s.record_send(2, b"b".to_vec());
        let replays = s.replay_from(0);
        assert_eq!(replays.len(), 2);
    }

    #[test]
    fn test_acknowledge_up_to_drains_all() {
        let mut s = make_stream();
        for i in 1u64..=4 {
            s.record_send(i, vec![i as u8]);
        }
        s.acknowledge_up_to(u64::MAX);
        assert!(s.send_history.is_empty());
    }

    #[test]
    fn test_acknowledge_up_to_empty_history_noop() {
        let mut s = make_stream();
        s.acknowledge_up_to(100); // should not panic
        assert!(s.send_history.is_empty());
    }

    #[test]
    fn test_acknowledge_up_to_past_end_noop() {
        let mut s = make_stream();
        s.record_send(1, b"x".to_vec());
        s.acknowledge_up_to(0); // nothing to trim
        assert_eq!(s.send_history.len(), 1);
    }

    #[test]
    fn test_initial_seq_numbers() {
        let s = StreamState::new(0, StreamType::Terminal);
        assert_eq!(s.next_out_seq, 1);
        assert_eq!(s.next_in_seq, 1);
        assert_eq!(s.lifecycle, StreamLifecycle::Open);
    }
}
