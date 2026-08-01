//! Channels and envelopes: what wraps every message on the wire.
//!
//! # Why messages travel in channels
//!
//! One socket carries book deltas at ten thousand a second and order executions at a handful
//! a minute, and the second must never wait behind the first. Multiplexing by channel is what
//! makes that expressible: the writer can look at a channel number and decide priority without
//! decoding anything, and the two sides can queue, drop and resume each stream by its own
//! rules. Several TCP connections would be the obvious alternative and are worse — separate
//! congestion windows, and no ordering guarantee between them across a reconnect.
//!
//! # Why an unknown channel is kept rather than rejected
//!
//! A newer peer will one day send channel 12. Treating that as a protocol error would make
//! every channel addition a flag day. Instead the number survives decoding, the message is
//! skipped, and a counter records it — the same rule MoonProto uses for unknown command bytes
//! (`src/protocol/mod.rs:63-65`, reverse-engineering report 02 §1).
//!
//! # Version skew inside a channel
//!
//! `ver` is the schema version of the individual message, not of the protocol. A receiver
//! silently skips anything newer than it understands and keeps the connection: that is what
//! lets the core and the terminal be upgraded on different days, which is the whole point of
//! the forward-compatibility work in task 1.1.

use crate::{ClientMsg, ServerMsg};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Identifies a terminal's request. Monotonic within a session; the core deduplicates on it
/// so that a retried command is not executed twice (task 1.7).
pub type ReqId = u64;

/// A symbol's short form within one `core_epoch`, handed out in the reply to a subscription.
///
/// Four bytes instead of the twenty-odd of `"binance:linear_perp:BTCUSDT"`, repeated on every
/// delta of every book. The epoch is what makes reuse safe: ids from a previous core lifetime
/// are refused rather than silently pointing at a different instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(pub u32);

/// The schema version this build understands. Anything higher is skipped, not guessed at.
pub const KNOWN_VER: u16 = 0;

/// A channel number. Unknown values are preserved so that a peer from the future is ignored
/// rather than disconnected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Channel(pub u8);

impl Channel {
    pub const CONTROL: Self = Self(0);
    pub const ACCOUNT: Self = Self(1);
    pub const COMMAND: Self = Self(2);
    pub const BOOK: Self = Self(3);
    pub const TAPE: Self = Self(4);
    pub const CANDLES: Self = Self(5);
    pub const REFERENCE: Self = Self(6);
    pub const SETTINGS: Self = Self(7);
    pub const STRATEGY: Self = Self(8);
    pub const REPORT: Self = Self(9);
    pub const ALERTS: Self = Self(10);
    /// Reserved for cross-venue arbitrage. Deliberately unused today: the number is claimed
    /// now so that adding the feature later is not a renumbering.
    pub const ARB: Self = Self(11);

    /// Highest channel this build knows about.
    pub const HIGHEST_KNOWN: u8 = 11;

    pub const fn is_known(self) -> bool {
        self.0 <= Self::HIGHEST_KNOWN
    }

    /// Writer priority class, 0 being the most urgent.
    ///
    /// Starvation is not a risk here and that is a property of the traffic, not of the
    /// scheduler: P0 is handshake and heartbeat, P1 is orders and their acknowledgements, and
    /// both are bounded by how fast a human or an exchange can act. The unbounded stream —
    /// book deltas — sits at P2 and below, where being starved is the correct outcome.
    pub const fn class(self) -> u8 {
        match self {
            Self::CONTROL => 0,
            Self::ACCOUNT | Self::COMMAND => 1,
            Self::BOOK => 2,
            Self::TAPE | Self::CANDLES | Self::ARB => 3,
            _ => 4,
        }
    }

    /// Whether a gap in this channel can be replayed after a reconnect.
    ///
    /// The book cannot: replaying deltas to rebuild it costs more than sending one snapshot,
    /// and gets it wrong if any delta was dropped. Control has nothing to replay — a session
    /// either resumed or it did not. Reports carry their own cursor, so replay would double
    /// up rows the terminal already committed.
    pub const fn resumable(self) -> bool {
        !matches!(self, Self::CONTROL | Self::BOOK | Self::ARB | Self::REPORT)
    }

    /// Human-readable name, for logs and metric labels.
    pub const fn name(self) -> &'static str {
        match self {
            Self::CONTROL => "control",
            Self::ACCOUNT => "account",
            Self::COMMAND => "command",
            Self::BOOK => "book",
            Self::TAPE => "tape",
            Self::CANDLES => "candles",
            Self::REFERENCE => "reference",
            Self::SETTINGS => "settings",
            Self::STRATEGY => "strategy",
            Self::REPORT => "report",
            Self::ALERTS => "alerts",
            Self::ARB => "arb",
            _ => "unknown",
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_known() {
            f.write_str(self.name())
        } else {
            write!(f, "unknown({})", self.0)
        }
    }
}

/// A message whose tag this build does not recognise, captured whole.
///
/// Keeping the payload rather than discarding it is what makes the skip diagnosable: the log
/// can name what was ignored, and a relay could forward it untouched. See
/// `tests/spike_forward_compat.rs` for why the idiomatic `#[serde(other)]` cannot do this.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnknownMsg {
    pub t: String,
    pub d: rmpv::Value,
}

/// Terminal → core.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientEnvelope {
    #[serde(rename = "c")]
    pub channel: Channel,
    #[serde(rename = "r")]
    pub req: ReqId,
    /// Schema version of `msg`. Defaulted so that a peer omitting it is understood as v0.
    #[serde(rename = "v", default)]
    pub ver: u16,
    #[serde(rename = "m")]
    pub msg: ClientMsg,
}

/// Core → terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerEnvelope {
    #[serde(rename = "c")]
    pub channel: Channel,
    /// Position within the channel, so a gap is detectable and a resume knows where to start.
    /// Zero on Control, which has nothing to resume.
    #[serde(rename = "s")]
    pub seq: u64,
    #[serde(rename = "v", default)]
    pub ver: u16,
    #[serde(rename = "m")]
    pub msg: ServerMsg,
}

/// The receiving shape of an envelope: either understood, or captured intact.
///
/// `untagged` tries the typed form first and falls back. It costs about 19 % of the decode
/// (measured in `tests/spike_forward_compat.rs`, 0.71 µs on a book delta) and buys a protocol
/// whose message types carry no `Unknown` arm for every `match` in the codebase to handle.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Incoming<T> {
    Known(T),
    Unknown(UnknownEnvelope),
}

/// An envelope whose header parsed but whose message did not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnknownEnvelope {
    #[serde(rename = "c")]
    pub channel: Channel,
    #[serde(rename = "v", default)]
    pub ver: u16,
    #[serde(rename = "m")]
    pub msg: UnknownMsg,
}

/// Why a well-formed envelope was not acted upon.
///
/// Every variant means the same thing operationally — ignore it, keep the connection — but
/// they are counted apart because they say different things about the deployment. Unknown
/// channels and variants mean the peer is newer; a future version within a known variant
/// means a schema was bumped without one side being updated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skipped {
    UnknownChannel(Channel),
    FutureVersion { channel: Channel, ver: u16 },
    UnknownVariant { channel: Channel, tag: String },
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownChannel(c) => write!(f, "unknown channel {}", c.0),
            Self::FutureVersion { channel, ver } => {
                write!(f, "message on {channel} is schema v{ver}, this build knows v{KNOWN_VER}")
            }
            Self::UnknownVariant { channel, tag } => {
                write!(f, "unknown message `{tag}` on {channel}")
            }
        }
    }
}

/// What a peer chose not to act on. Exported so an operator can see version skew building up
/// before it turns into a support question.
#[derive(Debug, Default)]
pub struct SkipCounters {
    pub skipped_future_version: AtomicU64,
    pub skipped_unknown_channel: AtomicU64,
    pub skipped_unknown_variant: AtomicU64,
}

impl SkipCounters {
    pub fn record(&self, skipped: &Skipped) {
        let counter = match skipped {
            Skipped::FutureVersion { .. } => &self.skipped_future_version,
            Skipped::UnknownChannel(_) => &self.skipped_unknown_channel,
            Skipped::UnknownVariant { .. } => &self.skipped_unknown_variant,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn future_version(&self) -> u64 {
        self.skipped_future_version.load(Ordering::Relaxed)
    }
    pub fn unknown_channel(&self) -> u64 {
        self.skipped_unknown_channel.load(Ordering::Relaxed)
    }
    pub fn unknown_variant(&self) -> u64 {
        self.skipped_unknown_variant.load(Ordering::Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.future_version() + self.unknown_channel() + self.unknown_variant()
    }
}

/// Decide whether a decoded client envelope should be acted on.
pub fn accept_client(incoming: Incoming<ClientEnvelope>) -> Result<ClientEnvelope, Skipped> {
    match incoming {
        Incoming::Known(env) => gate(env.channel, env.ver).map(|()| env),
        Incoming::Unknown(env) => Err(unknown_reason(env)),
    }
}

/// Decide whether a decoded server envelope should be acted on.
pub fn accept_server(incoming: Incoming<ServerEnvelope>) -> Result<ServerEnvelope, Skipped> {
    match incoming {
        Incoming::Known(env) => gate(env.channel, env.ver).map(|()| env),
        Incoming::Unknown(env) => Err(unknown_reason(env)),
    }
}

/// The channel is checked before the version: an unknown channel says more about the peer
/// than the schema of a message we were never going to route anywhere.
fn gate(channel: Channel, ver: u16) -> Result<(), Skipped> {
    if !channel.is_known() {
        return Err(Skipped::UnknownChannel(channel));
    }
    if ver > KNOWN_VER {
        return Err(Skipped::FutureVersion { channel, ver });
    }
    Ok(())
}

fn unknown_reason(env: UnknownEnvelope) -> Skipped {
    if !env.channel.is_known() {
        Skipped::UnknownChannel(env.channel)
    } else if env.ver > KNOWN_VER {
        Skipped::FutureVersion { channel: env.channel, ver: env.ver }
    } else {
        Skipped::UnknownVariant { channel: env.channel, tag: env.msg.t }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    fn client(channel: Channel, ver: u16) -> Vec<u8> {
        let env = ClientEnvelope {
            channel,
            req: 7,
            ver,
            msg: ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: "0123456789abcdef".into() },
        };
        rmp_serde::to_vec_named(&env).unwrap()
    }

    fn decode_client(bytes: &[u8]) -> Result<ClientEnvelope, Skipped> {
        accept_client(rmp_serde::from_slice(bytes).expect("a well-formed envelope decodes"))
    }

    // --- channel catalogue -----------------------------------------------------------------

    #[test]
    fn the_catalogue_matches_the_specification() {
        // Twelve channels, numbered without gaps. A renumbering would be a silent wire break,
        // so the numbers are asserted rather than trusted to the reader.
        let all = [
            (Channel::CONTROL, 0, "control"),
            (Channel::ACCOUNT, 1, "account"),
            (Channel::COMMAND, 2, "command"),
            (Channel::BOOK, 3, "book"),
            (Channel::TAPE, 4, "tape"),
            (Channel::CANDLES, 5, "candles"),
            (Channel::REFERENCE, 6, "reference"),
            (Channel::SETTINGS, 7, "settings"),
            (Channel::STRATEGY, 8, "strategy"),
            (Channel::REPORT, 9, "report"),
            (Channel::ALERTS, 10, "alerts"),
            (Channel::ARB, 11, "arb"),
        ];
        for (channel, number, name) in all {
            assert_eq!(channel.0, number, "{name} moved");
            assert_eq!(channel.name(), name);
            assert!(channel.is_known());
        }
        assert_eq!(all.len(), Channel::HIGHEST_KNOWN as usize + 1, "a channel is missing");
    }

    #[test]
    fn executions_outrank_book_deltas() {
        // The customer requirement, expressed as an assertion rather than a comment: an
        // execution must never queue behind a burst of quotes.
        assert!(Channel::ACCOUNT.class() < Channel::BOOK.class());
        assert!(Channel::COMMAND.class() < Channel::BOOK.class());
        assert!(Channel::CONTROL.class() < Channel::ACCOUNT.class(), "heartbeat outranks all");
        assert!(Channel::BOOK.class() < Channel::TAPE.class());
    }

    #[test]
    fn the_book_is_not_resumable_and_the_tape_is() {
        // Replaying deltas to rebuild a book costs more than one snapshot and is wrong if any
        // delta was lost. The tape is append-only, so replay is both cheap and correct.
        assert!(!Channel::BOOK.resumable());
        assert!(!Channel::CONTROL.resumable());
        assert!(!Channel::REPORT.resumable(), "reports carry their own cursor");
        assert!(Channel::TAPE.resumable());
        assert!(Channel::ACCOUNT.resumable(), "a missed execution must be recoverable");
    }

    #[test]
    fn an_unknown_channel_still_has_a_usable_class_and_name() {
        let future = Channel(200);
        assert!(!future.is_known());
        assert_eq!(future.class(), 4, "unknown traffic is scheduled last, not dropped on the floor");
        assert_eq!(future.to_string(), "unknown(200)");
    }

    // --- envelope round trip ---------------------------------------------------------------

    #[test]
    fn an_envelope_round_trips_with_its_header_intact() {
        let bytes = client(Channel::CONTROL, 0);
        let env = decode_client(&bytes).expect("a v0 control envelope is accepted");
        assert_eq!((env.channel, env.req, env.ver), (Channel::CONTROL, 7, 0));
    }

    #[test]
    fn the_header_keys_are_single_letters_on_the_wire() {
        // Every envelope carries these four keys, on every frame of every channel. Long names
        // would cost more than the payload on a book delta.
        let bytes = client(Channel::BOOK, 0);
        let raw: rmpv::Value = rmp_serde::from_slice(&bytes).unwrap();
        let keys: Vec<String> = match &raw {
            rmpv::Value::Map(entries) => {
                entries.iter().filter_map(|(k, _)| k.as_str().map(str::to_string)).collect()
            }
            other => panic!("an envelope must encode as a map, got {other:?}"),
        };
        assert_eq!(keys, vec!["c", "r", "v", "m"]);
    }

    #[test]
    fn a_missing_version_field_reads_as_v0() {
        // `#[serde(default)]` on `ver`, so a peer that omits it is understood rather than
        // refused. Task 1.1 established this as a rule for every field added after v1.
        let no_ver = rmp_serde::to_vec_named(&rmpv::Value::Map(vec![
            ("c".into(), rmpv::Value::from(0u8)),
            ("r".into(), rmpv::Value::from(1u64)),
            (
                "m".into(),
                rmpv::Value::Map(vec![("t".into(), "ping".into()), ("d".into(), rmpv::Value::from(9u64))]),
            ),
        ]))
        .unwrap();

        let env = decode_client(&no_ver).expect("an envelope without `v` is a v0 envelope");
        assert_eq!(env.ver, 0);
        assert_eq!(env.msg, ClientMsg::Ping(9));
    }

    // --- acceptance invariant 7 from 11-protocol-spec.md §10.5 -----------------------------

    #[test]
    fn a_future_version_is_skipped_and_counted_and_the_connection_lives() {
        let counters = SkipCounters::default();

        let bytes = client(Channel::COMMAND, KNOWN_VER + 1);
        let skipped = decode_client(&bytes).expect_err("a newer schema must not be acted on");
        counters.record(&skipped);

        assert_eq!(skipped, Skipped::FutureVersion { channel: Channel::COMMAND, ver: KNOWN_VER + 1 });
        assert_eq!(counters.future_version(), 1);
        assert_eq!(counters.unknown_channel(), 0, "the wrong counter must not move");
        assert_eq!(counters.unknown_variant(), 0);

        // Nothing above returned an error to the transport: the frame decoded, the session is
        // untouched, and the very next message is handled normally.
        let next = decode_client(&client(Channel::COMMAND, KNOWN_VER)).expect("still connected");
        assert_eq!(next.channel, Channel::COMMAND);
    }

    #[test]
    fn the_message_of_a_future_version_is_never_interpreted() {
        // Skipping has to happen before the payload is believed. A v99 `Hello` that was acted
        // on would authenticate a session against rules this build cannot know.
        let bytes = client(Channel::CONTROL, 99);
        assert!(decode_client(&bytes).is_err(), "a v99 Hello must not authenticate anything");
    }

    // --- unknown channels and variants -----------------------------------------------------

    #[test]
    fn an_unknown_channel_is_skipped_not_rejected() {
        let counters = SkipCounters::default();
        let skipped = decode_client(&client(Channel(12), 0)).expect_err("channel 12 is unknown");
        counters.record(&skipped);

        assert_eq!(skipped, Skipped::UnknownChannel(Channel(12)));
        assert_eq!(counters.unknown_channel(), 1);
        assert!(skipped.to_string().contains("unknown channel 12"));
    }

    #[test]
    fn an_unknown_variant_is_captured_with_its_payload() {
        // The property task 1.1 chose the untagged wrapper for: the tag survives, so the log
        // can name what was ignored instead of reporting an anonymous skip.
        let envelope = rmp_serde::to_vec_named(&rmpv::Value::Map(vec![
            ("c".into(), rmpv::Value::from(2u8)),
            ("r".into(), rmpv::Value::from(1u64)),
            ("v".into(), rmpv::Value::from(0u16)),
            (
                "m".into(),
                rmpv::Value::Map(vec![
                    ("t".into(), "place_order".into()),
                    ("d".into(), rmpv::Value::Map(vec![("qty".into(), "0.5".into())])),
                ]),
            ),
        ]))
        .unwrap();

        let counters = SkipCounters::default();
        let skipped = decode_client(&envelope).expect_err("this build has no PlaceOrder yet");
        counters.record(&skipped);

        assert_eq!(skipped, Skipped::UnknownVariant { channel: Channel::COMMAND, tag: "place_order".into() });
        assert_eq!(counters.unknown_variant(), 1);
        assert!(skipped.to_string().contains("place_order"), "the log must name it");
    }

    #[test]
    fn an_unknown_variant_on_an_unknown_channel_is_attributed_to_the_channel() {
        // Both are true at once; the channel is the more useful diagnosis, because it says
        // the peer has a whole subsystem this build does not.
        let envelope = rmp_serde::to_vec_named(&rmpv::Value::Map(vec![
            ("c".into(), rmpv::Value::from(200u8)),
            ("r".into(), rmpv::Value::from(1u64)),
            (
                "m".into(),
                rmpv::Value::Map(vec![("t".into(), "whatever".into()), ("d".into(), rmpv::Value::Nil)]),
            ),
        ]))
        .unwrap();

        assert_eq!(decode_client(&envelope).unwrap_err(), Skipped::UnknownChannel(Channel(200)));
    }

    #[test]
    fn counters_separate_the_three_reasons() {
        // They are counted apart because they mean different things: a future variant says
        // the peer is newer, a future version says a schema moved under a running deployment.
        let counters = SkipCounters::default();
        counters.record(&Skipped::UnknownChannel(Channel(12)));
        counters.record(&Skipped::FutureVersion { channel: Channel::BOOK, ver: 5 });
        counters.record(&Skipped::FutureVersion { channel: Channel::BOOK, ver: 6 });
        counters.record(&Skipped::UnknownVariant { channel: Channel::COMMAND, tag: "x".into() });

        assert_eq!(counters.unknown_channel(), 1);
        assert_eq!(counters.future_version(), 2);
        assert_eq!(counters.unknown_variant(), 1);
        assert_eq!(counters.total(), 4);
    }

    // --- direction ---------------------------------------------------------------------------

    #[test]
    fn a_server_envelope_does_not_decode_as_a_client_envelope() {
        // `seq` and `req` occupy different keys, so a reflected server frame cannot be
        // mistaken for a command. Without this, an echo becomes an instruction.
        let server = ServerEnvelope { channel: Channel::ACCOUNT, seq: 1, ver: 0, msg: ServerMsg::Pong(1) };
        let bytes = rmp_serde::to_vec_named(&server).unwrap();

        let as_client: Incoming<ClientEnvelope> =
            rmp_serde::from_slice(&bytes).expect("it is still a map with c/v/m");
        assert!(
            matches!(as_client, Incoming::Unknown(_)),
            "a server envelope must not arrive as a usable command"
        );
    }

    #[test]
    fn a_symbol_id_is_transparent_on_the_wire() {
        // Four bytes, not a nested map: this rides on every delta of every book.
        let bytes = rmp_serde::to_vec_named(&SymbolId(70_000)).unwrap();
        let raw: rmpv::Value = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(raw.as_u64(), Some(70_000));
    }
}
