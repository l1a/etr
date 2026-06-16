// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-stream state: ordered delivery, send history, and lifecycle.
use std::collections::VecDeque;

use crate::protocol::StreamType;

/// Lifecycle of a single multiplexed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLifecycle {
    Opening,
    Open,
    Closing,
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
}
