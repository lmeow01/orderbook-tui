//! WebSocket connection setup.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type Tx = SplitSink<Ws, Message>;
pub type Rx = SplitStream<Ws>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects with TCP_NODELAY and limits large enough for full-book
/// snapshots (Coinbase's runs to several MB), then sends `subscribe`.
pub async fn connect(url: &str, subscribe: &[String]) -> Result<(Tx, Rx)> {
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(128 << 20))
        .max_frame_size(Some(64 << 20));
    let (ws, _) = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(url, Some(cfg), true),
    )
    .await
    .context("connect timed out")?
    .with_context(|| format!("connecting to {url}"))?;
    let (mut tx, rx) = ws.split();
    for m in subscribe {
        tx.send(Message::text(m.as_str()))
            .await
            .context("sending subscribe")?;
    }
    Ok((tx, rx))
}
