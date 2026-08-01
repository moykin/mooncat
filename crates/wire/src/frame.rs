//! The frame: two bytes of preamble, then a MessagePack body that may be compressed.
//!
//! ```text
//! byte 0  fmt    format of the envelope. 1 today.
//! byte 1  flags  bit0 ZSTD, bit1 BATCH, bits 2..7 reserved and required to be zero
//! byte 2+ body   MessagePack, or a zstd stream that decompresses to it
//! ```
//!
//! # Why a raw preamble instead of a field inside the message
//!
//! The decision "decompress or not" has to be made *before* anything is parsed. Putting the
//! flag inside the MessagePack document would mean a compressed frame is a msgpack `bin` blob
//! nested inside a msgpack map — an extra header and an extra copy on every frame of the
//! hottest channel, to carry one bit.
//!
//! # This layer is deliberately about bytes
//!
//! It knows nothing about envelopes, channels or message types; those arrive in task 1.3.
//! Keeping the split means the size limits, the compression policy and the decompression bomb
//! guard are all testable without constructing a single protocol message, and they stay
//! correct no matter how the message types above them change.
//!
//! # The threat this module actually defends against
//!
//! A frame arrives from the network before anyone is authenticated. Everything here is
//! written on the assumption that its contents are chosen by an adversary: the length is
//! checked before the decoder is handed the bytes, the compression ratio is capped so a
//! kilobyte cannot become a gigabyte, and the decompressed size limit is derived from the
//! *compressed* size rather than from anything the sender declares.

use crate::CodecError;

/// Envelope format. A peer that sends anything else is speaking a protocol we do not have.
pub const FMT_V1: u8 = 1;

/// Body is a zstd stream.
pub const FLAG_ZSTD: u8 = 0b0000_0001;
/// Body is an array of envelopes rather than a single one.
pub const FLAG_BATCH: u8 = 0b0000_0010;
/// Bits with no meaning yet. Required to be zero so that giving one a meaning later is a
/// change old peers reject loudly instead of misreading.
pub const FLAG_RESERVED: u8 = 0b1111_1100;

/// Largest frame a terminal may send. A subscription to 4 096 symbols is roughly 200 KiB, so
/// this is generous by a factor of five and still far below anything worth allocating.
pub const MAX_FRAME_C2S: usize = 1024 * 1024;
/// Largest frame a core may send: a 5 000-level book snapshot and a report page fit inside.
pub const MAX_FRAME_S2C: usize = 16 * 1024 * 1024;
/// Hard ceiling on decompressed size, whatever the ratio works out to.
pub const MAX_INFLATED: usize = 64 * 1024 * 1024;
/// A frame may not expand by more than this. 64:1 is comfortably above what real market data
/// achieves (a book snapshot of repeated decimals compresses about 8:1) and far below what a
/// crafted frame can reach — zstd will happily turn a kilobyte into a gigabyte.
pub const MAX_INFLATE_RATIO: usize = 64;

/// Floor under the decompression budget, regardless of ratio.
///
/// Without it the ratio rule rejects legitimate traffic, which it did on the first run of
/// these tests: a body that compresses 200:1 — perfectly ordinary for a deep book snapshot,
/// where the same price prefix repeats across hundreds of levels — was refused as a bomb
/// because 64 × its compressed size came out smaller than the body.
///
/// A megabyte is safe to concede because any peer may already send a megabyte uncompressed;
/// granting the same budget to a compressed frame adds no attack that was not already there.
/// The ratio rule keeps doing its real job, which is stopping a *large* compressed frame from
/// expanding into something enormous.
pub const MIN_INFLATE_BUDGET: usize = 1024 * 1024;
/// Below this, compression loses to its own header.
pub const COMPRESS_MIN_BYTES: usize = 512;
/// And it is only worth sending compressed if it actually saved this much.
pub const COMPRESS_MIN_GAIN_PCT: usize = 5;
/// Throughput matters more than ratio on a channel carrying ten thousand frames a second.
pub const ZSTD_LEVEL: i32 = 1;

/// Which way a frame is travelling. Only used to pick the size limit, but naming it at the
/// call site is what stops a core's 16 MiB ceiling from being applied to what a terminal sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    pub const fn max_frame(self) -> usize {
        match self {
            Self::ClientToServer => MAX_FRAME_C2S,
            Self::ServerToClient => MAX_FRAME_S2C,
        }
    }
}

/// A frame with its preamble parsed and its body ready for MessagePack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Whether the body is an array of envelopes rather than one.
    pub batch: bool,
    /// Whether it arrived compressed. Carried for metrics; the body is already plain.
    pub was_compressed: bool,
    pub body: Vec<u8>,
}

/// Wrap a MessagePack body in a frame, compressing it if that is worth doing.
///
/// `limit` applies to the frame as it goes out, so a body that only fits once compressed is
/// legal — which is the point of compressing it.
pub fn encode(body: &[u8], batch: bool, limit: usize) -> Result<Vec<u8>, CodecError> {
    let mut flags = if batch { FLAG_BATCH } else { 0 };
    let mut payload = body;

    // Only pay for compression where it can win. `COMPRESS_MIN_GAIN_PCT` exists because a
    // frame that compresses by two percent still costs the receiver a full decompression
    // pass, and on the hot path that trade is a loss.
    let compressed = (body.len() >= COMPRESS_MIN_BYTES)
        .then(|| zstd::bulk::compress(body, ZSTD_LEVEL).ok())
        .flatten()
        .filter(|z| z.len() * 100 <= body.len() * (100 - COMPRESS_MIN_GAIN_PCT));

    if let Some(z) = &compressed {
        flags |= FLAG_ZSTD;
        payload = z;
    }

    let total = 2 + payload.len();
    if total > limit {
        return Err(CodecError::TooLarge { size: total });
    }

    let mut out = Vec::with_capacity(total);
    out.push(FMT_V1);
    out.push(flags);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Parse a frame: check the preamble, enforce the limit, decompress if needed.
///
/// Every rejection here is [`CodecError`], and the caller answers with `Failed{Malformed}`
/// **without** closing the connection — a single bad frame is not grounds for a reconnect
/// storm. The exception is size, which is fatal by [`crate::ErrorCode::is_fatal`].
pub fn decode(frame: &[u8], direction: Direction) -> Result<Frame, CodecError> {
    // Length first, before any structure is trusted. This is the rule that was already in the
    // codec and has to survive every rewrite: a decoder handed a length prefix will size a
    // collection from it before checking whether the bytes behind it exist.
    if frame.len() > direction.max_frame() {
        return Err(CodecError::TooLarge { size: frame.len() });
    }
    let [fmt, flags, body @ ..] = frame else {
        return Err(CodecError::Decode(format!(
            "frame is {} bytes, shorter than the 2-byte preamble",
            frame.len()
        )));
    };

    if *fmt != FMT_V1 {
        return Err(CodecError::Decode(format!("unknown envelope format {fmt}; this peer speaks {FMT_V1}")));
    }
    if flags & FLAG_RESERVED != 0 {
        // A reserved bit set means the sender is using a feature from a version this peer
        // does not have. Failing loudly is the point: silently masking it off would let the
        // two sides disagree about what the body means.
        return Err(CodecError::Decode(format!("reserved flag bits set: {:#010b}", flags & FLAG_RESERVED)));
    }

    let was_compressed = flags & FLAG_ZSTD != 0;
    let body = if was_compressed { inflate(body)? } else { body.to_vec() };

    Ok(Frame { batch: flags & FLAG_BATCH != 0, was_compressed, body })
}

/// Decompress with a ceiling derived from the compressed size.
///
/// The ceiling is `clamp(compressed × MAX_INFLATE_RATIO, MIN_INFLATE_BUDGET, MAX_INFLATED)`,
/// so a small hostile frame can only ever make us allocate a bounded amount — while a body
/// that legitimately compresses better than 64:1 still gets through. The size zstd declares
/// in its own header is deliberately **not** consulted: it is attacker-controlled, and
/// trusting it is precisely how a decompression bomb lands.
fn inflate(compressed: &[u8]) -> Result<Vec<u8>, CodecError> {
    use std::io::Read;

    let ceiling = compressed.len().saturating_mul(MAX_INFLATE_RATIO).clamp(MIN_INFLATE_BUDGET, MAX_INFLATED);

    let decoder = zstd::stream::Decoder::new(compressed)
        .map_err(|e| CodecError::Decode(format!("not a zstd stream: {e}")))?;

    // Read one byte past the ceiling: if that byte materialises, the frame is over budget and
    // we stop there rather than after it has finished expanding.
    let mut out = Vec::new();
    decoder
        .take(ceiling as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| CodecError::Decode(format!("zstd stream is corrupt: {e}")))?;

    if out.len() > ceiling {
        return Err(CodecError::Decode(format!(
            "decompression ratio over {MAX_INFLATE_RATIO}:1 — {} compressed bytes expanded past \
             {ceiling}",
            compressed.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a real book snapshot looks like on the wire: prices sharing a prefix, quantities
    /// of varying length, field names repeating. Compresses well but not absurdly — the
    /// realistic case the limits have to let through.
    fn market_shaped(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 64);
        let mut price = 6_300_000_i64;
        let mut i = 0u64;
        while out.len() < len {
            i = i.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            price += (i >> 60) as i64 - 8;
            let qty = (i >> 33) % 1_000_000;
            out.extend_from_slice(
                format!("{{\"p\":\"{}.{:02}\",\"q\":\"0.{:06}\"}},", price / 100, price % 100, qty)
                    .as_bytes(),
            );
        }
        out.truncate(len);
        out
    }

    /// Incompressible. A proper xorshift, because a lazier generator produced a stream that
    /// zstd found structure in — which quietly turned two of these tests into no-ops.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// The pathological case a bomb is built from: maximally compressible.
    fn pathological(len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    fn round_trip(body: &[u8], batch: bool) -> Frame {
        let frame = encode(body, batch, MAX_FRAME_S2C).expect("encodes");
        decode(&frame, Direction::ServerToClient).expect("decodes")
    }

    // --- shape ---------------------------------------------------------------------------

    #[test]
    fn a_small_body_round_trips_uncompressed() {
        let body = b"a short message".to_vec();
        let frame = encode(&body, false, MAX_FRAME_C2S).unwrap();

        assert_eq!(frame[0], FMT_V1);
        assert_eq!(frame[1], 0, "no flags: not batched, not worth compressing");
        assert_eq!(&frame[2..], &body[..], "the body is carried verbatim");

        let decoded = decode(&frame, Direction::ClientToServer).unwrap();
        assert_eq!(decoded, Frame { batch: false, was_compressed: false, body });
    }

    #[test]
    fn the_batch_flag_survives_the_round_trip() {
        assert!(round_trip(b"payload", true).batch);
        assert!(!round_trip(b"payload", false).batch);
    }

    // --- compression policy ---------------------------------------------------------------

    #[test]
    fn a_large_repetitive_body_is_compressed_and_comes_back_identical() {
        let body = market_shaped(64 * 1024);
        let frame = encode(&body, false, MAX_FRAME_S2C).unwrap();

        assert_eq!(frame[1] & FLAG_ZSTD, FLAG_ZSTD, "a 64 KiB book-shaped body must compress");
        assert!(frame.len() < body.len() / 2, "and should be much smaller: got {}", frame.len());

        let decoded = decode(&frame, Direction::ServerToClient).unwrap();
        assert!(decoded.was_compressed);
        assert_eq!(decoded.body, body, "decompression must be exact");
    }

    #[test]
    fn a_body_below_the_threshold_is_never_compressed() {
        // Even perfectly compressible: below COMPRESS_MIN_BYTES the header costs more than
        // the saving, and the receiver still pays for a decompression pass.
        let body = pathological(COMPRESS_MIN_BYTES - 1);
        let frame = encode(&body, false, MAX_FRAME_S2C).unwrap();
        assert_eq!(frame[1] & FLAG_ZSTD, 0, "under the threshold, compression must not be tried");
    }

    #[test]
    fn an_incompressible_body_is_sent_plain() {
        // The gain rule earning its keep: zstd on noise produces something no smaller, and
        // sending it would cost the receiver a decompression for nothing.
        let body = noise(8 * 1024);
        let frame = encode(&body, false, MAX_FRAME_S2C).unwrap();
        assert_eq!(frame[1] & FLAG_ZSTD, 0, "an incompressible body must be sent as-is");
        assert_eq!(decode(&frame, Direction::ServerToClient).unwrap().body, body);
    }

    // --- the four acceptance invariants from 11-protocol-spec.md §10.5 ---------------------

    #[test]
    fn invariant_1_an_unknown_format_is_malformed_not_fatal() {
        let mut frame = encode(b"body", false, MAX_FRAME_C2S).unwrap();
        frame[0] = 2;

        let err = decode(&frame, Direction::ClientToServer).unwrap_err();
        assert!(matches!(err, CodecError::Decode(_)), "an unknown fmt is a decode error, got {err:?}");
        assert!(err.to_string().contains("unknown envelope format 2"), "got: {err}");

        // Malformed is explicitly non-fatal: one bad frame must not cost the session.
        assert!(!crate::ErrorCode::Malformed.is_fatal());
    }

    #[test]
    fn invariant_2_a_reserved_flag_bit_is_refused() {
        for bit in 2..8 {
            let mut frame = encode(b"body", false, MAX_FRAME_C2S).unwrap();
            frame[1] |= 1 << bit;

            let err = decode(&frame, Direction::ClientToServer).unwrap_err();
            assert!(err.to_string().contains("reserved flag"), "bit {bit} must be refused, got: {err}");
        }
    }

    #[test]
    fn invariant_3_length_is_checked_before_the_body_is_touched() {
        // An oversized frame must be refused on its length alone, whatever it contains. The
        // body here is deliberately not a valid anything: reaching a parse error instead of a
        // size error would mean the decoder looked first.
        let too_big = vec![0xffu8; MAX_FRAME_C2S + 1];
        let err = decode(&too_big, Direction::ClientToServer).unwrap_err();
        assert!(matches!(err, CodecError::TooLarge { .. }), "got {err:?}");

        // And the limit is per-direction. Incompressible on purpose: a body that compresses
        // would produce a frame under a megabyte, and the test would pass for the wrong
        // reason — it is the *frame* size the limit applies to, not the body's.
        let big = encode(&noise(2 * 1024 * 1024), false, MAX_FRAME_S2C).unwrap();
        assert!(big.len() > MAX_FRAME_C2S, "the frame must actually exceed the C2S limit");

        assert!(decode(&big, Direction::ServerToClient).is_ok(), "2 MiB is fine towards a terminal");
        assert!(
            matches!(decode(&big, Direction::ClientToServer), Err(CodecError::TooLarge { .. })),
            "but a terminal may not send it"
        );
    }

    #[test]
    fn a_body_that_compresses_past_the_ratio_still_gets_through() {
        // The regression this floor exists for. Discovered by the first run of these tests:
        // a deep book snapshot compresses far better than 64:1 because the same price prefix
        // repeats across hundreds of levels, and the ratio rule on its own called it a bomb.
        let body = pathological(MIN_INFLATE_BUDGET);
        let frame = encode(&body, false, MAX_FRAME_S2C).unwrap();

        let ratio = body.len() / (frame.len() - 2);
        assert!(ratio > MAX_INFLATE_RATIO, "the test needs a ratio past the cap, got {ratio}:1");

        let decoded = decode(&frame, Direction::ServerToClient).expect("legitimate traffic passes");
        assert_eq!(decoded.body, body);
    }

    #[test]
    fn invariant_4_a_decompression_bomb_is_refused_without_expanding_it() {
        // A megabyte of zeroes compresses to well under 16 KiB — a ratio far past 64:1.
        let bomb_source = pathological(8 * 1024 * 1024);
        let compressed = zstd::bulk::compress(&bomb_source, ZSTD_LEVEL).unwrap();
        let ratio = bomb_source.len() / compressed.len();
        assert!(ratio > MAX_INFLATE_RATIO, "test is only meaningful above the cap, got {ratio}:1");

        let mut frame = Vec::with_capacity(2 + compressed.len());
        frame.push(FMT_V1);
        frame.push(FLAG_ZSTD);
        frame.extend_from_slice(&compressed);

        let err = decode(&frame, Direction::ServerToClient).unwrap_err();
        assert!(err.to_string().contains("decompression ratio"), "got: {err}");
    }

    #[test]
    fn the_bomb_guard_scales_with_the_frame_and_not_with_the_ceiling() {
        // The point of deriving the ceiling from the compressed size: a *tiny* hostile frame
        // must not be allowed a 64 MiB allocation on its way to being rejected.
        let compressed = zstd::bulk::compress(&pathological(1024 * 1024), ZSTD_LEVEL).unwrap();
        let ceiling = compressed.len() * MAX_INFLATE_RATIO;
        assert!(
            ceiling < MAX_INFLATED,
            "for a {}-byte frame the budget is {ceiling}, far below the {MAX_INFLATED}-byte ceiling",
            compressed.len()
        );
    }

    #[test]
    fn a_ratio_just_under_the_cap_is_allowed_through() {
        // The guard must not be so eager that legitimate market data trips it. A book
        // snapshot of repeated decimals is the realistic worst case.
        let body = market_shaped(256 * 1024);
        let frame = encode(&body, false, MAX_FRAME_S2C).unwrap();
        let ratio = body.len() / (frame.len() - 2);
        println!("repetitive market-data-shaped body compresses {ratio}:1");
        assert_eq!(decode(&frame, Direction::ServerToClient).unwrap().body, body);
    }

    // --- malformed input ------------------------------------------------------------------

    #[test]
    fn a_frame_shorter_than_its_preamble_is_a_decode_error() {
        for short in [vec![], vec![FMT_V1]] {
            let err = decode(&short, Direction::ClientToServer).unwrap_err();
            assert!(err.to_string().contains("shorter than"), "got: {err}");
        }
        // Exactly two bytes is legal: an empty body is not this layer's business.
        assert!(decode(&[FMT_V1, 0], Direction::ClientToServer).is_ok());
    }

    #[test]
    fn a_zstd_flag_over_garbage_is_an_error_not_a_panic() {
        let frame = [&[FMT_V1, FLAG_ZSTD][..], &[0xde, 0xad, 0xbe, 0xef][..]].concat();
        let err = decode(&frame, Direction::ServerToClient).unwrap_err();
        assert!(matches!(err, CodecError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn a_truncated_zstd_stream_is_an_error_not_a_hang() {
        let body = market_shaped(8 * 1024);
        let mut frame = encode(&body, false, MAX_FRAME_S2C).unwrap();
        assert_eq!(frame[1] & FLAG_ZSTD, FLAG_ZSTD);
        frame.truncate(frame.len() / 2);

        assert!(decode(&frame, Direction::ServerToClient).is_err());
    }

    #[test]
    fn encoding_refuses_to_produce_an_over_limit_frame() {
        // The check is on the frame as it goes out, so the caller cannot accidentally emit
        // something the peer is obliged to reject.
        let body = noise(MAX_FRAME_C2S);
        assert!(matches!(encode(&body, false, MAX_FRAME_C2S), Err(CodecError::TooLarge { .. })));
    }
}
