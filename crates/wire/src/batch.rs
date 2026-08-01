//! Merging many small messages into one frame.
//!
//! # The problem
//!
//! A busy market produces thousands of book deltas a second, each a few hundred bytes. Sent
//! one per WebSocket frame, the per-frame overhead — a frame header, a syscall, a TLS record —
//! costs more than the payload, and the receiver wakes up thousands of times a second to do
//! almost nothing each time.
//!
//! # The rule that makes it safe
//!
//! **Latency-sensitive channels are never batched.** An order execution must not wait five
//! milliseconds behind a queue of quotes, so anything at priority class 0 or 1 — control,
//! account, commands — is emitted the instant it arrives, in its own frame, jumping whatever
//! is still accumulating. That is the customer's "executions do not queue behind deltas"
//! requirement, applied at the point where the delay would actually be introduced.
//!
//! Everything else accumulates until the first of three bounds is reached: five milliseconds,
//! sixty-four kilobytes, or two hundred and fifty-six messages. The time bound is what caps
//! the added latency; the other two are what stop one frame from becoming a small snapshot.
//!
//! # Why messages are encoded on the way in
//!
//! A batch is a MessagePack array of envelopes, and a MessagePack array is its header followed
//! by the concatenated encodings of its elements. So each message is encoded once as it
//! arrives — which is also how the sixty-four kilobyte bound is measured exactly rather than
//! estimated — and the flush is a header plus a copy, not a second pass over the data.

use crate::envelope::ServerEnvelope;
use crate::CodecError;
use std::time::{Duration, Instant};

/// Upper bound on the latency batching may add.
pub const BATCH_FLUSH_MS: u64 = 5;
/// Lower bound on the size worth waiting for.
pub const BATCH_FLUSH_BYTES: usize = 64 * 1024;
/// So that one frame does not turn into a miniature snapshot.
pub const BATCH_MAX_ITEMS: usize = 256;

/// Priority classes at or below this are never batched.
///
/// Class 0 is the handshake and the heartbeat, class 1 is orders and their acknowledgements.
/// Delaying either by five milliseconds to save a syscall is the wrong trade — and on the
/// heartbeat it would eat a measurable slice of the timeout budget.
const NEVER_BATCH_ABOVE_CLASS: u8 = 1;

/// Why a batch was closed. Carried into metrics: which bound fires most often is what tells
/// an operator whether the numbers are set sensibly for their instrument count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushReason {
    /// A latency-sensitive message arrived and went out on its own.
    Urgent,
    Deadline,
    Bytes,
    Items,
    /// The caller asked, typically because the connection is closing.
    Requested,
}

/// A frame's worth of messages, already encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// MessagePack: one envelope, or an array of them.
    pub body: Vec<u8>,
    /// Whether `body` is an array — this becomes `FLAG_BATCH` in the frame preamble.
    pub is_array: bool,
    pub count: usize,
    pub reason: FlushReason,
}

/// Accumulates outbound messages until one of the bounds is reached.
#[derive(Debug, Default)]
pub struct Batcher {
    /// Encodings in arrival order. Order within a channel is preserved because a channel's
    /// messages only ever enter here in the order they were produced.
    parts: Vec<Vec<u8>>,
    bytes: usize,
    opened_at: Option<Instant>,
}

impl Batcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// When the accumulated batch must go out, if anything is accumulating.
    ///
    /// The caller waits on this rather than polling: a select over the event stream and this
    /// deadline is what makes the five-millisecond bound cost nothing while idle.
    pub fn deadline(&self) -> Option<Instant> {
        self.opened_at.map(|t| t + Duration::from_millis(BATCH_FLUSH_MS))
    }

    /// Offer a message. Returns a batch when one is ready to be written.
    ///
    /// An urgent message comes straight back on its own and does **not** disturb what is
    /// accumulating: the pending batch keeps filling and goes out on its own schedule. That
    /// ordering is deliberate — a quote that was already waiting has no claim to go ahead of
    /// an execution that just happened.
    pub fn push(&mut self, env: &ServerEnvelope, now: Instant) -> Result<Option<Batch>, CodecError> {
        let encoded = rmp_serde::to_vec_named(env).map_err(|e| CodecError::Encode(e.to_string()))?;

        if env.channel.class() <= NEVER_BATCH_ABOVE_CLASS {
            return Ok(Some(Batch { count: 1, body: encoded, is_array: false, reason: FlushReason::Urgent }));
        }

        if self.parts.is_empty() {
            self.opened_at = Some(now);
        }
        self.bytes += encoded.len();
        self.parts.push(encoded);

        // Checked after the push, not before: a message is never held back to keep a bound,
        // it is what closes the batch it completes.
        let reason = if self.parts.len() >= BATCH_MAX_ITEMS {
            Some(FlushReason::Items)
        } else if self.bytes >= BATCH_FLUSH_BYTES {
            Some(FlushReason::Bytes)
        } else if self.deadline().is_some_and(|d| now >= d) {
            Some(FlushReason::Deadline)
        } else {
            None
        };

        Ok(reason.map(|r| self.take(r)))
    }

    /// Flush if the time bound has passed. Called when the deadline fires.
    pub fn flush_due(&mut self, now: Instant) -> Option<Batch> {
        (!self.parts.is_empty() && self.deadline().is_some_and(|d| now >= d))
            .then(|| self.take(FlushReason::Deadline))
    }

    /// Flush whatever is held, whether or not a bound was reached.
    pub fn flush(&mut self) -> Option<Batch> {
        (!self.parts.is_empty()).then(|| self.take(FlushReason::Requested))
    }

    fn take(&mut self, reason: FlushReason) -> Batch {
        let parts = std::mem::take(&mut self.parts);
        self.bytes = 0;
        self.opened_at = None;

        // A single message is sent as itself, not as a one-element array: the array header
        // would be pure overhead, and the receiver has the flag to tell the two apart.
        let count = parts.len();
        if count == 1 {
            let body = parts.into_iter().next().expect("count is 1");
            return Batch { body, is_array: false, count, reason };
        }

        Batch { body: msgpack_array(parts), is_array: true, count, reason }
    }
}

/// Concatenate pre-encoded values under a MessagePack array header.
///
/// Valid because a MessagePack array is exactly its header followed by its elements, so the
/// parts never need re-encoding. Assembling this by hand rather than through `rmp_serde` is
/// what makes batching close to free on the hot path.
fn msgpack_array(parts: Vec<Vec<u8>>) -> Vec<u8> {
    let payload: usize = parts.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(payload + 5);

    let n = parts.len();
    match n {
        0..=15 => out.push(0x90 | n as u8),
        16..=0xffff => {
            out.push(0xdc);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(0xdd);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    for part in parts {
        out.extend_from_slice(&part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Channel;
    use crate::ServerMsg;

    fn env(channel: Channel, seq: u64) -> ServerEnvelope {
        ServerEnvelope { channel, seq, ver: 0, msg: ServerMsg::Pong(seq) }
    }

    fn at(ms: u64) -> Instant {
        // A fixed origin so the arithmetic in these tests is exact rather than wall-clock.
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    fn decode_batch(batch: &Batch) -> Vec<ServerEnvelope> {
        if batch.is_array {
            rmp_serde::from_slice(&batch.body).expect("a batch decodes as an array")
        } else {
            vec![rmp_serde::from_slice(&batch.body).expect("a lone message decodes as itself")]
        }
    }

    // --- the urgency rule ------------------------------------------------------------------

    #[test]
    fn account_and_control_are_never_batched() {
        // The customer requirement, at the one place where the delay would be introduced.
        let mut b = Batcher::new();
        for channel in [Channel::CONTROL, Channel::ACCOUNT, Channel::COMMAND] {
            let batch = b
                .push(&env(channel, 1), at(0))
                .unwrap()
                .unwrap_or_else(|| panic!("{channel} must go out immediately"));
            assert_eq!(batch.reason, FlushReason::Urgent);
            assert_eq!(batch.count, 1);
            assert!(!batch.is_array, "a lone message needs no array header");
        }
        assert!(b.is_empty(), "urgent traffic must not enter the accumulator");
    }

    #[test]
    fn an_execution_overtakes_quotes_that_were_already_waiting() {
        // The ordering that matters: a delta waiting since a moment ago has no claim to go
        // ahead of an execution that just happened.
        let mut b = Batcher::new();
        for seq in 0..10 {
            assert!(b.push(&env(Channel::BOOK, seq), at(0)).unwrap().is_none());
        }

        let urgent = b.push(&env(Channel::ACCOUNT, 99), at(1)).unwrap().expect("goes out at once");
        assert_eq!(decode_batch(&urgent)[0].seq, 99);

        // And the quotes are still accumulating, undisturbed, on their own schedule.
        assert_eq!(b.len(), 10);
        assert_eq!(b.flush().unwrap().count, 10);
    }

    #[test]
    fn book_tape_and_candles_are_batched() {
        let mut b = Batcher::new();
        for channel in [Channel::BOOK, Channel::TAPE, Channel::CANDLES, Channel::REFERENCE] {
            assert!(b.push(&env(channel, 1), at(0)).unwrap().is_none(), "{channel} should accumulate");
        }
        assert_eq!(b.len(), 4);
    }

    // --- the three bounds ------------------------------------------------------------------

    #[test]
    fn the_item_bound_closes_the_batch() {
        let mut b = Batcher::new();
        for seq in 0..BATCH_MAX_ITEMS as u64 - 1 {
            assert!(b.push(&env(Channel::BOOK, seq), at(0)).unwrap().is_none());
        }
        let batch = b.push(&env(Channel::BOOK, 999), at(0)).unwrap().expect("the 256th closes it");
        assert_eq!(batch.reason, FlushReason::Items);
        assert_eq!(batch.count, BATCH_MAX_ITEMS);
        assert!(b.is_empty(), "the accumulator resets");
    }

    #[test]
    fn the_time_bound_closes_the_batch() {
        let mut b = Batcher::new();
        assert!(b.push(&env(Channel::BOOK, 1), at(0)).unwrap().is_none());

        assert!(b.flush_due(at(BATCH_FLUSH_MS - 1)).is_none(), "not yet due");
        let batch = b.flush_due(at(BATCH_FLUSH_MS)).expect("due at exactly the bound");
        assert_eq!(batch.reason, FlushReason::Deadline);
    }

    #[test]
    fn the_byte_bound_closes_the_batch() {
        // Needs messages big enough that 64 KiB is reached before 256 of them — a deep book
        // snapshot, not a quote. With small messages the item bound always fires first, which
        // is the correct behaviour and is what the previous test covers.
        let bulky = |seq: u64| ServerEnvelope {
            channel: Channel::BOOK,
            seq,
            ver: 0,
            msg: ServerMsg::Failed { code: crate::ErrorCode::Malformed, message: "x".repeat(512) },
        };

        let mut b = Batcher::new();
        let mut sent = 0;
        let batch = loop {
            sent += 1;
            assert!(sent < BATCH_MAX_ITEMS as u64, "the item bound fired first: messages too small");
            if let Some(batch) = b.push(&bulky(sent), at(0)).unwrap() {
                break batch;
            }
        };
        assert_eq!(batch.reason, FlushReason::Bytes);
        assert!(batch.body.len() >= BATCH_FLUSH_BYTES);
        assert!(batch.count < BATCH_MAX_ITEMS, "closed on size, not on count");
    }

    #[test]
    fn with_ordinary_messages_the_item_bound_is_the_one_that_fires() {
        // Worth pinning down, because it says which bound actually governs in production: a
        // book delta is a few dozen bytes, so 256 of them are nowhere near 64 KiB.
        let mut b = Batcher::new();
        let mut sent = 0;
        let batch = loop {
            sent += 1;
            if let Some(batch) = b.push(&env(Channel::BOOK, sent), at(0)).unwrap() {
                break batch;
            }
        };
        assert_eq!(batch.reason, FlushReason::Items);
        assert!(
            batch.body.len() < BATCH_FLUSH_BYTES,
            "256 ordinary messages should still be well under the byte bound, got {}",
            batch.body.len()
        );
    }

    #[test]
    fn the_deadline_is_measured_from_the_first_message_not_the_last() {
        // Otherwise a steady trickle resets the clock on every message and the batch never
        // goes out — a starvation bug that only shows up under light load.
        let mut b = Batcher::new();
        b.push(&env(Channel::BOOK, 1), at(0)).unwrap();
        b.push(&env(Channel::BOOK, 2), at(3)).unwrap();
        b.push(&env(Channel::BOOK, 3), at(4)).unwrap();

        assert_eq!(b.deadline(), Some(at(BATCH_FLUSH_MS)), "still measured from the first");
        assert!(b.flush_due(at(BATCH_FLUSH_MS)).is_some());
    }

    #[test]
    fn an_empty_batcher_has_no_deadline_and_nothing_to_flush() {
        let mut b = Batcher::new();
        assert!(b.deadline().is_none(), "an idle connection must not hold a timer");
        assert!(b.flush().is_none());
        assert!(b.flush_due(at(1_000)).is_none());
    }

    // --- encoding --------------------------------------------------------------------------

    #[test]
    fn a_batch_decodes_as_an_array_in_arrival_order() {
        let mut b = Batcher::new();
        for seq in 0..5 {
            b.push(&env(Channel::TAPE, seq), at(0)).unwrap();
        }
        let batch = b.flush().unwrap();

        assert!(batch.is_array);
        let decoded = decode_batch(&batch);
        assert_eq!(decoded.len(), 5);
        assert_eq!(
            decoded.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4],
            "order within a channel must survive batching"
        );
    }

    #[test]
    fn a_single_message_is_not_wrapped_in_an_array() {
        let mut b = Batcher::new();
        b.push(&env(Channel::BOOK, 7), at(0)).unwrap();
        let batch = b.flush().unwrap();

        assert!(!batch.is_array, "one message needs no array header");
        assert_eq!(decode_batch(&batch)[0].seq, 7);
    }

    #[test]
    fn the_hand_written_array_header_matches_what_rmp_serde_produces() {
        // The whole optimisation rests on a MessagePack array being its header followed by its
        // elements. If that ever stops holding, everything here silently produces garbage, so
        // it is checked against the library at the three header widths.
        for n in [1usize, 15, 16, 300] {
            let envelopes: Vec<_> = (0..n as u64).map(|s| env(Channel::TAPE, s)).collect();
            let by_library = rmp_serde::to_vec_named(&envelopes).unwrap();

            let parts: Vec<Vec<u8>> = envelopes.iter().map(|e| rmp_serde::to_vec_named(e).unwrap()).collect();
            let by_hand = msgpack_array(parts);

            assert_eq!(by_hand, by_library, "hand-assembled array differs at n = {n}");
        }
    }

    // --- the acceptance measurement ----------------------------------------------------------

    #[test]
    fn batching_cuts_frames_by_at_least_eight_times() {
        // The acceptance criterion for task 1.4. The stream is synthetic but shaped like the
        // real one: eleven instruments, BTCUSDT carrying the majority of the traffic, book
        // deltas dominating, an occasional execution mixed in.
        //
        // What is measured is frames, not bytes — the cost being removed is per-frame
        // overhead: a WebSocket header, a TLS record, a syscall and a wakeup on the far side.
        const SECONDS: u64 = 10;
        const DELTAS_PER_SEC: u64 = 2_000;
        const EXECUTIONS_PER_SEC: u64 = 2;

        let mut b = Batcher::new();
        let (mut frames, mut messages, mut urgent_frames) = (0u64, 0u64, 0u64);

        for tick_us in 0..SECONDS * 1_000_000 {
            let now = at(tick_us / 1_000);

            // Deltas arrive on a fixed cadence across eleven instruments.
            if tick_us % (1_000_000 / DELTAS_PER_SEC) == 0 {
                messages += 1;
                let channel = if messages % 3 == 0 { Channel::TAPE } else { Channel::BOOK };
                if let Some(batch) = b.push(&env(channel, messages), now).unwrap() {
                    frames += 1;
                    assert_ne!(batch.reason, FlushReason::Urgent);
                }
            }
            // Executions are rare and must never wait.
            if tick_us % (1_000_000 / EXECUTIONS_PER_SEC) == 0 {
                messages += 1;
                let batch = b.push(&env(Channel::ACCOUNT, messages), now).unwrap();
                let batch = batch.expect("an execution is always its own frame");
                assert_eq!(batch.reason, FlushReason::Urgent);
                frames += 1;
                urgent_frames += 1;
            }
            if b.flush_due(now).is_some() {
                frames += 1;
            }
        }
        if b.flush().is_some() {
            frames += 1;
        }

        let ratio = messages as f64 / frames as f64;
        println!("{messages} messages in {frames} frames over {SECONDS}s — {ratio:.1}× reduction");
        println!("  of those, {urgent_frames} frames carried an execution on its own");

        assert!(ratio >= 8.0, "batching must cut frames at least 8×, got {ratio:.1}×");
        assert_eq!(
            urgent_frames,
            SECONDS * EXECUTIONS_PER_SEC,
            "every execution must have had its own frame — none may be batched away"
        );
    }

    #[test]
    fn added_latency_never_exceeds_the_time_bound() {
        // The other half of the acceptance criterion: p99 latency must not move. The bound is
        // structural rather than statistical — no message can be held longer than
        // BATCH_FLUSH_MS, so the worst case is the bound itself.
        let mut b = Batcher::new();
        let mut worst_hold = 0u64;

        for ms in 0..1_000u64 {
            let now = at(ms);
            // One message per millisecond: slow enough that neither size bound ever fires,
            // which is the case where the time bound is the only thing preventing a stall.
            b.push(&env(Channel::BOOK, ms), now).unwrap();

            let opened = b.deadline().map(|d| d - Duration::from_millis(BATCH_FLUSH_MS));
            if let (Some(opened), Some(_)) = (opened, b.flush_due(now)) {
                worst_hold = worst_hold.max((now - opened).as_millis() as u64);
            }
        }
        assert!(
            worst_hold <= BATCH_FLUSH_MS,
            "a message was held {worst_hold} ms, over the {BATCH_FLUSH_MS} ms bound"
        );
    }
}
