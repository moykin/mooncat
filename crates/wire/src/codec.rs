//! Frame encoding.
//!
//! MessagePack, because these messages are mostly numbers and the terminal decodes a lot of
//! them per second. Frames are length-checked *before* decoding: an attacker-supplied length
//! prefix must never become an allocation.

use crate::{ClientMsg, ServerMsg};
use serde::{de::DeserializeOwned, Serialize};

/// Largest frame either side will accept.
///
/// A full instrument list is the biggest legitimate message and sits well under this. The
/// cap exists so a hostile or corrupt frame cannot make us allocate unboundedly.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame is {size} bytes, over the {MAX_FRAME_BYTES} limit")]
    TooLarge { size: usize },
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("decode failed: {0}")]
    Decode(String),
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let bytes = rmp_serde::to_vec_named(msg).map_err(|e| CodecError::Encode(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge { size: bytes.len() });
    }
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    // Checked first: `rmp_serde` would otherwise size collections from the frame's own
    // headers before it ever sees whether the data backs them up.
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge { size: bytes.len() });
    }
    rmp_serde::from_slice(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

pub fn decode_client(bytes: &[u8]) -> Result<ClientMsg, CodecError> {
    decode(bytes)
}

pub fn decode_server(bytes: &[u8]) -> Result<ServerMsg, CodecError> {
    decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    use domain::{
        ConnectionEvent, Event, ExchangeId, MarketEvent, MarketKind, PublicTrade, Side, Symbol, Timestamp,
    };
    use rust_decimal_macros::dec;

    #[test]
    fn client_messages_survive_a_round_trip() {
        let msg = ClientMsg::Hello { protocol: PROTOCOL_VERSION, token: "s3cret".into() };
        assert_eq!(decode_client(&encode(&msg).unwrap()).unwrap(), msg);

        let subs = vec![exchange::Subscription::Book(Symbol::new(
            ExchangeId::Binance,
            MarketKind::LinearPerp,
            "BTCUSDT",
        ))];
        let msg = ClientMsg::Subscribe(subs);
        assert_eq!(decode_client(&encode(&msg).unwrap()).unwrap(), msg);
    }

    #[test]
    fn a_trade_event_survives_with_its_decimals_exact() {
        // The reason money is Decimal end to end: a float round trip would not be exact.
        let trade = PublicTrade {
            symbol: Symbol::new(ExchangeId::Binance, MarketKind::Spot, "BTCUSDT"),
            price: dec!(63096.01000000),
            qty: dec!(0.00000123),
            taker_side: Side::Sell,
            ts: Timestamp::from_millis(1_700_000_000_000),
            id: 4_027_555_089,
        };
        let msg = ServerMsg::Event(Box::new(Event::market(
            Timestamp::from_millis(1),
            MarketEvent::Trade(trade.clone()),
        )));

        let round_tripped = decode_server(&encode(&msg).unwrap()).unwrap();
        match round_tripped {
            ServerMsg::Event(e) => match e.payload {
                domain::Payload::Market(MarketEvent::Trade(t)) => {
                    assert_eq!(t.price, dec!(63096.01));
                    assert_eq!(t.qty, dec!(0.00000123));
                    assert_eq!(t.id, trade.id);
                    assert_eq!(t.symbol.key(), "binance:spot:BTCUSDT");
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn connection_events_survive_with_their_reason() {
        let msg = ServerMsg::Event(Box::new(Event::connection(
            Timestamp::from_millis(9),
            ConnectionEvent::Resyncing { reason: "rebuilding book".into() },
        )));
        assert_eq!(decode_server(&encode(&msg).unwrap()).unwrap(), msg);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_parsed() {
        let huge = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(decode_client(&huge), Err(CodecError::TooLarge { .. })));
    }

    #[test]
    fn garbage_is_a_decode_error_not_a_panic() {
        assert!(matches!(decode_client(&[0xc1, 0xff, 0x00]), Err(CodecError::Decode(_))));
        assert!(matches!(decode_client(&[]), Err(CodecError::Decode(_))));
    }

    #[test]
    fn a_server_frame_does_not_decode_as_a_client_frame() {
        // Cross-decoding would let a reflected frame be mistaken for a command.
        let bytes = encode(&ServerMsg::Pong(7)).unwrap();
        assert!(decode_client(&bytes).is_err());
    }
}
