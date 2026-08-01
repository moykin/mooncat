//! Throwaway diagnostic: dump raw frames from one Binance stream.
//!
//! ```bash
//! cargo run -p binance --example raw_probe -- wss://fstream.binance.com/ws btcusdt@aggTrade
//! ```

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "wss://fstream.binance.com/ws".into());
    let stream = args.next().unwrap_or_else(|| "btcusdt@aggTrade".into());

    let (socket, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut tx, mut rx) = socket.split();

    let params = stream.split(',').map(|s| format!("\"{}\"", s.trim())).collect::<Vec<_>>().join(",");
    let frame = format!(r#"{{"method":"SUBSCRIBE","params":[{params}],"id":1}}"#);
    println!("→ {frame}");
    tx.send(Message::Text(frame.into())).await?;

    // Ask the server what it thinks we are subscribed to — the only way to tell an
    // unrecognised stream name (silently accepted) from a genuinely quiet stream.
    tx.send(Message::Text(r#"{"method":"LIST_SUBSCRIPTIONS","id":2}"#.into())).await?;

    let mut seen = 0;
    while let Some(Ok(msg)) = rx.next().await {
        if let Message::Text(text) = msg {
            println!("← {text}");
            seen += 1;
            if seen >= 6 {
                break;
            }
        }
    }
    Ok(())
}
