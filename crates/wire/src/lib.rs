//! The protocol between a core and a terminal.
//!
//! The core runs on a VPS next to the exchange and holds the API keys. The terminal runs on
//! a laptop and holds nothing. Everything crossing that gap is defined here.
//!
//! MessagePack over WebSocket, not a bespoke UDP protocol: the terminal renders at frame
//! rate, not at microsecond latency, and TCP's ordering and reassembly are worth more here
//! than the microseconds a custom datagram layer would save. Latency belongs where the
//! orders are sent — inside the core, which is already in the same datacentre as the venue.
//!
//! Two invariants the type system alone will not give us, so they are enforced and tested:
//! a session that has not authenticated receives no private data, and a token comparison
//! never leaks its answer through timing.

use serde::{Deserialize, Serialize};

pub mod admission;
pub mod audit;
pub mod auth;
pub mod batch;
pub mod codec;
pub mod command;
pub mod envelope;
pub mod event;
pub mod frame;
pub mod heartbeat;
pub mod idempotency;
pub mod keystore;
pub mod pin;
pub mod resume;
pub mod session;
pub mod subscription;

pub use admission::{authorize, Admit, Gatekeeper, SessionId};
pub use audit::{AuditRecord, Outcome};
pub use auth::{DeviceId, DeviceKey, DeviceRegistry, Transcript};
pub use batch::{Batch, Batcher, FlushReason};
pub use codec::{decode_client, decode_server, encode, CodecError, MAX_FRAME_BYTES};
pub use command::{Command, Role, Scope};
pub use envelope::{
    accept_client, accept_server, Channel, ClientEnvelope, Incoming, ReqId, ServerEnvelope, SkipCounters,
    Skipped, SymbolId, KNOWN_VER,
};
pub use event::{AckCode, AckDetail, AckStatus, ServerEvent};
pub use frame::{Direction, Frame};
pub use heartbeat::{Heartbeat, PongOutcome};
pub use idempotency::{Admission, ReqRing, SEEN_REQ_CAPACITY};
pub use keystore::{EncryptedFile, Keystore, KeystoreError};
pub use pin::{PinnedVerifier, SpkiPin};
pub use resume::{Impossible, ResumeLog, Resumed, RESUME_WINDOW};
pub use session::{Auth, Session};
pub use subscription::{Assigned, Subscriptions};

/// Bumped whenever a message changes shape. Mismatched peers are rejected at Hello rather
/// than left to misparse each other's frames later.
pub const PROTOCOL_VERSION: u16 = 2;

/// Terminal → core.
///
/// Adjacently tagged: the tag is what lets an unrecognised message be captured whole rather
/// than becoming a decode error. See `tests/spike_forward_compat.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Always the first message. Anything before it is refused.
    Hello {
        protocol: u16,
        token: String,
    },
    Subscribe(Vec<exchange::Subscription>),
    Unsubscribe(Vec<exchange::Subscription>),
    /// Round-trip probe. The core echoes the nonce back.
    Ping(u64),
}

/// Core → terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Hello accepted.
    Welcome {
        protocol: u16,
    },
    Event(Box<domain::Event>),
    Pong(u64),
    Failed {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The peer speaks a different protocol version.
    ProtocolMismatch,
    /// Wrong token.
    Unauthorized,
    /// A message arrived before Hello.
    NotAuthenticated,
    /// Undecodable frame.
    Malformed,
    /// Frame exceeded [`MAX_FRAME_BYTES`].
    TooLarge,
}

impl ErrorCode {
    /// Whether the core should drop the connection after reporting this.
    ///
    /// Anything touching authentication ends the session: leaving a rejected peer connected
    /// invites a retry loop against the token.
    pub const fn is_fatal(self) -> bool {
        matches!(self, Self::ProtocolMismatch | Self::Unauthorized | Self::NotAuthenticated | Self::TooLarge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failures_end_the_session_but_a_bad_frame_does_not() {
        assert!(ErrorCode::Unauthorized.is_fatal());
        assert!(ErrorCode::NotAuthenticated.is_fatal());
        assert!(ErrorCode::ProtocolMismatch.is_fatal());
        assert!(ErrorCode::TooLarge.is_fatal());

        // One corrupt frame should not cost a terminal its whole stream.
        assert!(!ErrorCode::Malformed.is_fatal());
    }
}
