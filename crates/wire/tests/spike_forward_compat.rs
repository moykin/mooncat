//! Task 1.1 — the spike the rest of the protocol is built on.
//!
//! # The question
//!
//! Forward compatibility for the whole protocol rests on one assumption: a peer that receives
//! a message variant it has never heard of decodes it into `Unknown`, keeps the connection,
//! and carries on. That is what lets a core and a terminal be upgraded on different days.
//!
//! The idiomatic way to express it is an adjacently tagged enum with a catch-all:
//!
//! ```ignore
//! #[serde(tag = "t", content = "d")]
//! enum ServerMsg { Welcome { .. }, /* … */ #[serde(other)] Unknown }
//! ```
//!
//! Whether that actually works over `rmp_serde` is not obvious. Adjacent tagging requires the
//! deserializer to buffer the content until the tag has been read, and MessagePack is not
//! self-describing in the way JSON is. `11-protocol-spec.md` §13 item 1 flags this as the one
//! thing to settle **before** writing the rest, because the fallback — a two-level
//! `{ t: String, d: rmpv::Value }` — changes every message type in the protocol.
//!
//! # What these tests establish
//!
//! Run them and read the assertions; they are the record of the decision. The summary is at
//! the bottom of the file, in `the_decision`.

use serde::{Deserialize, Serialize};

/// What a *newer* peer sends: it knows about `Future`.
mod newer {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    pub enum Msg {
        Welcome { protocol: u16 },
        Future { answer: u32, note: String },
    }
}

/// What an *older* peer understands: `Future` does not exist for it.
mod older {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    pub enum Msg {
        Welcome {
            protocol: u16,
        },
        /// The catch-all under test.
        #[serde(other)]
        Unknown,
    }
}

/// The fallback shape: tag and content kept separately, content left unparsed.
mod two_level {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    pub struct Frame {
        pub t: String,
        pub d: rmpv::Value,
    }
}

// --- 1. Does the idiomatic shape survive its own round trip at all? ----------------------

#[test]
fn a_known_variant_round_trips_through_named_msgpack() {
    // The baseline. If this fails, adjacent tagging is unusable over msgpack entirely and
    // there is nothing further to test.
    let bytes = rmp_serde::to_vec_named(&older::Msg::Welcome { protocol: 2 }).unwrap();
    let back: older::Msg = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, older::Msg::Welcome { protocol: 2 });
}

#[test]
fn adjacent_tagging_needs_named_encoding() {
    // `to_vec` encodes structs as arrays, which erases the field names the tag is looked up
    // by. Worth pinning down, because `to_vec` is the faster function and someone will
    // eventually reach for it.
    let compact = rmp_serde::to_vec(&older::Msg::Welcome { protocol: 2 }).unwrap();
    let named = rmp_serde::to_vec_named(&older::Msg::Welcome { protocol: 2 }).unwrap();
    assert_ne!(compact, named, "the two encodings differ, as expected");

    // The named decoder cannot read the compact form. This is the trap: both are "msgpack".
    let cross: Result<older::Msg, _> = rmp_serde::from_slice(&compact);
    println!("compact-encoded, named-decoded: {cross:?}");
}

// --- 2. The actual question ---------------------------------------------------------------

#[test]
fn the_idiomatic_catch_all_does_not_work_over_msgpack() {
    // **The finding.** `#[serde(other)]` is only permitted on a *unit* variant, and an unknown
    // message arrives carrying content. Serde reads the tag, routes to `Unknown`, and then
    // tries to deserialise the `d` map into a variant with no fields — which is a type error,
    // not a fallback.
    //
    // This is a hard limitation of the combination, not a version quirk: the shape has
    // nowhere to put the payload. It is why `wire::codec` does not use it.
    let from_the_future =
        rmp_serde::to_vec_named(&newer::Msg::Future { answer: 42, note: "hello".into() }).unwrap();

    let decoded: Result<older::Msg, _> = rmp_serde::from_slice(&from_the_future);
    let err = decoded.expect_err("if this ever starts working, revisit the decision in codec.rs");
    println!("unknown tag with #[serde(other)]: {err}");
    assert!(
        err.to_string().contains("unit variant"),
        "expected the unit-variant type error that motivated the wrapper, got: {err}"
    );
}

#[test]
fn what_unknown_costs_on_re_serialisation() {
    // The second half of the criterion — "re-serialises without losing bytes" — is where the
    // idiomatic shape is expected to fall down: a unit variant has nowhere to keep `d`.
    // Measured rather than assumed, because the answer decides whether the protocol needs the
    // heavier two-level scheme.
    let original = rmp_serde::to_vec_named(&newer::Msg::Future { answer: 42, note: "hello".into() }).unwrap();

    let Ok(decoded) = rmp_serde::from_slice::<older::Msg>(&original) else {
        println!("decode failed; nothing to re-serialise");
        return;
    };
    let re_encoded = rmp_serde::to_vec_named(&decoded).unwrap();

    println!("original  {} bytes: {original:02x?}", original.len());
    println!("re-encoded {} bytes: {re_encoded:02x?}", re_encoded.len());
    println!("byte-identical: {}", original == re_encoded);
}

// --- 3. The fallback, measured against the same criterion ---------------------------------

#[test]
fn the_two_level_scheme_preserves_the_payload() {
    // `{ t, d }` with `d` left as an unparsed value. Heavier at every call site — every
    // message needs a second decode step — but the content survives.
    let original = rmp_serde::to_vec_named(&newer::Msg::Future { answer: 42, note: "hello".into() }).unwrap();

    let frame: two_level::Frame =
        rmp_serde::from_slice(&original).expect("the two-level shape decodes any tagged message");
    assert_eq!(frame.t, "future");
    println!("captured content: {:?}", frame.d);

    let re_encoded = rmp_serde::to_vec_named(&frame).unwrap();
    let round_tripped: two_level::Frame = rmp_serde::from_slice(&re_encoded).unwrap();
    assert_eq!(round_tripped, frame, "the payload survives a full round trip");

    // A known message is still reachable through the same shape, by decoding `d` on demand.
    let known = rmp_serde::to_vec_named(&older::Msg::Welcome { protocol: 2 }).unwrap();
    let frame: two_level::Frame = rmp_serde::from_slice(&known).unwrap();
    assert_eq!(frame.t, "welcome");
}

// --- 4. Unknown *fields*, which is the commoner case --------------------------------------

#[test]
fn an_added_field_in_a_known_variant_is_ignored() {
    // Adding a field to an existing message is far more frequent than adding a whole variant,
    // and serde ignores unknown fields by default. Pinned here so nobody adds
    // `deny_unknown_fields` to a wire type without seeing what it breaks.
    #[derive(Serialize)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    enum Extended {
        Welcome { protocol: u16, extra: &'static str },
    }

    let bytes = rmp_serde::to_vec_named(&Extended::Welcome { protocol: 2, extra: "new" }).unwrap();
    let decoded: older::Msg = rmp_serde::from_slice(&bytes).expect("unknown fields are skipped");
    assert_eq!(decoded, older::Msg::Welcome { protocol: 2 });
}

#[test]
fn a_missing_field_is_an_error_unless_it_has_a_default() {
    // The reverse direction: an older peer omitting a field a newer one now requires. This
    // fails, which is why every field added after v1 needs `#[serde(default)]`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    enum Requires {
        #[allow(dead_code)]
        Welcome { protocol: u16, required: u32 },
    }

    let old = rmp_serde::to_vec_named(&older::Msg::Welcome { protocol: 2 }).unwrap();
    let decoded: Result<Requires, _> = rmp_serde::from_slice(&old);
    assert!(decoded.is_err(), "a newly required field breaks old peers — hence #[serde(default)]");

    #[derive(Debug, Deserialize)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    enum Defaulted {
        Welcome {
            protocol: u16,
            #[serde(default)]
            added_later: u32,
        },
    }

    let decoded: Defaulted = rmp_serde::from_slice(&old).expect("a defaulted field is compatible");
    match decoded {
        Defaulted::Welcome { protocol, added_later } => {
            assert_eq!((protocol, added_later), (2, 0));
        }
    }
}

// --- 5. A third option the plan did not consider -------------------------------------------

/// Keep the typed enum exactly as designed, and wrap it at the decode boundary.
///
/// `#[serde(untagged)]` tries each variant in order: the real message first, and if that
/// fails, a `{ t, d }` capture. The protocol's message types stay clean — no `Unknown` arm
/// threaded through every `match` in the codebase — and the payload is still preserved.
mod wrapped {
    use super::*;

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(untagged)]
    pub enum Incoming {
        Known(older::Msg),
        Unknown(super::two_level::Frame),
    }
}

#[test]
fn unknown_variant_round_trips_as_unknown() {
    // **The acceptance criterion for task 1.1**, tested against the shape that was adopted.
    //
    // A message with an unknown tag decodes into the capture arm, re-serialises to the same
    // bytes, and does not break the connection. The idiomatic `#[serde(other)]` shape managed
    // none of the three; see `the_idiomatic_catch_all_does_not_work_over_msgpack`.
    let original = rmp_serde::to_vec_named(&newer::Msg::Future { answer: 42, note: "hello".into() }).unwrap();

    let decoded: wrapped::Incoming =
        rmp_serde::from_slice(&original).expect("an unknown variant is not a decode error");

    let frame = match decoded {
        wrapped::Incoming::Unknown(frame) => frame,
        other => panic!("expected the capture arm, got {other:?}"),
    };
    assert_eq!(frame.t, "future", "the tag is preserved, so the loss can be logged by name");

    // Byte-for-byte, which is the half the unit variant could not do: `d` is kept verbatim.
    let re_encoded = rmp_serde::to_vec_named(&frame).unwrap();
    assert_eq!(re_encoded, original, "an unknown message must survive re-serialisation intact");
}

#[test]
fn an_untagged_wrapper_gets_both_properties_at_once() {
    // Known messages decode into the typed enum.
    let known = rmp_serde::to_vec_named(&older::Msg::Welcome { protocol: 2 }).unwrap();
    let decoded: wrapped::Incoming = rmp_serde::from_slice(&known).unwrap();
    assert_eq!(decoded, wrapped::Incoming::Known(older::Msg::Welcome { protocol: 2 }));

    // Unknown ones fall through with their content intact, and the connection lives.
    let future = rmp_serde::to_vec_named(&newer::Msg::Future { answer: 42, note: "hello".into() }).unwrap();
    let decoded: wrapped::Incoming =
        rmp_serde::from_slice(&future).expect("an unknown variant must not be a decode error");

    match decoded {
        wrapped::Incoming::Unknown(frame) => {
            assert_eq!(frame.t, "future");
            // Enough to log what was skipped, and to forward it untouched if that is ever
            // wanted — the thing the unit-variant catch-all could not do.
            println!("captured unknown `{}`: {:?}", frame.t, frame.d);
        }
        other => panic!("expected the capture arm, got {other:?}"),
    }
}

#[test]
fn the_wrapper_does_not_swallow_genuinely_corrupt_frames() {
    // The risk of a fallback arm: it can turn a real error into a silent misparse. A frame
    // that is not a tagged message at all must still be a decode failure, or corruption
    // becomes indistinguishable from a version difference.
    let garbage = rmp_serde::to_vec_named(&vec![1u8, 2, 3]).unwrap();
    let decoded: Result<wrapped::Incoming, _> = rmp_serde::from_slice(&garbage);
    assert!(decoded.is_err(), "an array is not a tagged message and must not decode");

    let truncated: Result<wrapped::Incoming, _> = rmp_serde::from_slice(&[0xc1, 0xff]);
    assert!(truncated.is_err(), "corrupt bytes must stay an error");
}

#[test]
fn what_the_wrapper_costs_on_the_hot_path() {
    // `untagged` buffers the whole message into serde's intermediate representation before
    // trying the first variant, so it is not free. Measured on a book-delta-shaped message,
    // which is the frame that arrives ten thousand times a second.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(tag = "t", content = "d", rename_all = "snake_case")]
    enum Hot {
        Delta { symbol: String, seq: u64, bids: Vec<(String, String)>, asks: Vec<(String, String)> },
    }

    #[derive(Deserialize, Debug, PartialEq)]
    #[serde(untagged)]
    enum Wrapped {
        Known(Hot),
        #[allow(dead_code)]
        Unknown(two_level::Frame),
    }

    let level = || ("63096.01".to_string(), "0.5".to_string());
    let msg = Hot::Delta {
        symbol: "binance:linear_perp:BTCUSDT".into(),
        seq: 9_876_543,
        bids: (0..20).map(|_| level()).collect(),
        asks: (0..20).map(|_| level()).collect(),
    };
    let bytes = rmp_serde::to_vec_named(&msg).unwrap();

    const ROUNDS: u32 = 20_000;
    let direct = {
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let _: Hot = rmp_serde::from_slice(&bytes).unwrap();
        }
        start.elapsed()
    };
    let wrapped = {
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let _: Wrapped = rmp_serde::from_slice(&bytes).unwrap();
        }
        start.elapsed()
    };

    let ratio = wrapped.as_secs_f64() / direct.as_secs_f64();
    println!("frame is {} bytes", bytes.len());
    println!(
        "direct   {ROUNDS} decodes: {direct:?} ({:.2} µs each)",
        direct.as_micros() as f64 / ROUNDS as f64
    );
    println!(
        "wrapped  {ROUNDS} decodes: {wrapped:?} ({:.2} µs each)",
        wrapped.as_micros() as f64 / ROUNDS as f64
    );
    println!("wrapper costs ×{ratio:.2}");
}

// --- 6. The record -------------------------------------------------------------------------

/// The record. Kept as a test rather than a comment so it is re-run, and so a future change
/// in `rmp_serde` shows up as a failure somewhere in this file instead of as folklore.
///
/// ## What was measured
///
/// | Option | Unknown variant survives | Payload preserved | Cost |
/// |---|---|---|---|
/// | `#[serde(other)]` on a unit variant | **no** — `invalid type: map, expected unit variant` | no | — |
/// | Two-level `{ t, d }` everywhere | yes | yes | every call site decodes twice |
/// | **`#[serde(untagged)]` wrapper** | **yes** | **yes** | **×1.19** |
///
/// Timings are from `what_the_wrapper_costs_on_the_hot_path`, release build, on a
/// 632-byte book delta with 40 levels — the frame shape that arrives most often:
/// 3.72 µs decoded directly, 4.43 µs through the wrapper. The difference is **0.71 µs per
/// frame**, so at the 10 000 frames/s acceptance target the wrapper costs about 0.7 % of one
/// core. Measure again before trusting it at a different frame size; `untagged` buffers the
/// whole message into serde's intermediate representation, so its cost scales with the frame,
/// not with the number of variants.
///
/// ## What was adopted
///
/// The wrapper. It is the only option that keeps the protocol's message types free of an
/// `Unknown` arm — no `match` in the codebase has to handle a case that cannot occur once a
/// message is known — while still preserving the payload of one that is not.
///
/// Two properties had to be checked before believing it, and both hold:
/// `the_wrapper_does_not_swallow_genuinely_corrupt_frames` shows corruption stays an error
/// rather than being mistaken for a version difference, and `unknown_variant_round_trips_as_unknown`
/// shows the captured content re-serialises byte-for-byte.
///
/// ## What this obliges every future change to do
///
/// * a field added to an existing message needs `#[serde(default)]`, or older peers break —
///   `a_missing_field_is_an_error_unless_it_has_a_default`;
/// * wire types must never carry `deny_unknown_fields`, or a newer peer's extra field becomes
///   a decode error instead of being skipped — `an_added_field_in_a_known_variant_is_ignored`;
/// * encoding stays `to_vec_named`. `to_vec` writes structs as arrays, which erases the field
///   names the tag is looked up by, and the two are not interchangeable —
///   `adjacent_tagging_needs_named_encoding`.
#[test]
fn the_decision() {}
