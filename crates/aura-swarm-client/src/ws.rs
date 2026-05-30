//! WebSocket client for attaching to a run's event stream.
//!
//! After a run is created with [`crate::SwarmClient::create_run`], the caller
//! attaches to the swarm-facing `WS /v1/agents/:agent_id/stream/:run_id` socket
//! returned as `event_stream_url`. This module opens that socket, lets the
//! caller send [`InboundMessage`] frames (e.g. `UserMessage` / `Cancel`), and
//! surfaces incoming [`OutboundMessage`] frames on an async channel.
//!
//! Build the fully-qualified `ws(s)://` URL with
//! [`crate::SwarmClient::run_ws_url`].

use aura_swarm_protocol::{InboundMessage, OutboundMessage};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::error::SwarmClientError;

type RunSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Handle for sending [`InboundMessage`] frames to a run's event stream.
///
/// Cloning is cheap; all clones write to the same underlying socket. Dropping
/// every clone (or the socket closing) ends the outgoing writer task.
#[derive(Debug, Clone)]
pub struct RunStreamSender {
    tx: mpsc::Sender<String>,
}

impl RunStreamSender {
    /// Send a user message for processing (`InboundMessage::UserMessage`).
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or the stream is closed.
    pub async fn send_user_message(
        &self,
        content: impl Into<String>,
    ) -> Result<(), SwarmClientError> {
        self.send(&InboundMessage::UserMessage {
            content: content.into(),
        })
        .await
    }

    /// Cancel the current turn (`InboundMessage::Cancel`).
    ///
    /// # Errors
    ///
    /// Returns an error if the stream is closed.
    pub async fn cancel(&self) -> Result<(), SwarmClientError> {
        self.send(&InboundMessage::Cancel).await
    }

    /// Send an arbitrary [`InboundMessage`] (e.g. a tool callback response).
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be serialized or the stream is closed.
    pub async fn send(&self, message: &InboundMessage) -> Result<(), SwarmClientError> {
        let json =
            serde_json::to_string(message).map_err(|e| SwarmClientError::Parse(e.to_string()))?;
        self.tx
            .send(json)
            .await
            .map_err(|_| SwarmClientError::WebSocket("run stream is closed".to_string()))
    }
}

/// Connect to a run's event stream and start proxying frames.
///
/// `url` must be a `ws://` or `wss://` URL for
/// `/v1/agents/:agent_id/stream/:run_id` (build it with
/// [`crate::SwarmClient::run_ws_url`]). `token` is the bearer JWT.
///
/// Returns a [`RunStreamSender`] for outgoing frames and a receiver that yields
/// each decoded [`OutboundMessage`]. The receiver closes when the agent closes
/// the socket or the connection errors.
///
/// # Errors
///
/// Returns [`SwarmClientError::WebSocket`] if the URL is invalid, the bearer
/// token is not a valid header value, or the connection cannot be established.
pub async fn connect_run_stream(
    url: &str,
    token: &str,
) -> Result<(RunStreamSender, mpsc::Receiver<OutboundMessage>), SwarmClientError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| SwarmClientError::WebSocket(format!("invalid stream URL: {e}")))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| SwarmClientError::WebSocket("invalid bearer token".to_string()))?,
    );

    let (ws_stream, _) = connect_async(request)
        .await
        .map_err(|e| SwarmClientError::WebSocket(e.to_string()))?;

    let (write, read) = ws_stream.split();

    let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(32);
    let (event_tx, event_rx) = mpsc::channel::<OutboundMessage>(32);

    tokio::spawn(run_writer(write, outgoing_rx));
    tokio::spawn(run_reader(read, event_tx));

    Ok((RunStreamSender { tx: outgoing_tx }, event_rx))
}

/// Task that forwards serialized outgoing frames to the socket.
async fn run_writer(mut write: SplitSink<RunSocket, Message>, mut rx: mpsc::Receiver<String>) {
    while let Some(text) = rx.recv().await {
        if write.send(Message::Text(text)).await.is_err() {
            break;
        }
    }
}

/// Task that decodes incoming frames into [`OutboundMessage`] events.
async fn run_reader(mut read: SplitStream<RunSocket>, tx: mpsc::Sender<OutboundMessage>) {
    while let Some(result) = read.next().await {
        match result {
            Ok(Message::Text(text)) => match serde_json::from_str::<OutboundMessage>(&text) {
                Ok(msg) => {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, text = %text, "Failed to parse run stream message");
                }
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "Run stream read error");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sender_serializes_user_message() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let sender = RunStreamSender { tx };
        sender.send_user_message("hello").await.unwrap();
        let json = rx.recv().await.unwrap();
        assert!(json.contains("\"type\":\"user_message\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[tokio::test]
    async fn sender_serializes_cancel() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let sender = RunStreamSender { tx };
        sender.cancel().await.unwrap();
        let json = rx.recv().await.unwrap();
        assert!(json.contains("\"type\":\"cancel\""));
    }

    #[tokio::test]
    async fn sender_errors_when_stream_closed() {
        let (tx, rx) = mpsc::channel::<String>(4);
        drop(rx);
        let sender = RunStreamSender { tx };
        let err = sender.send_user_message("hi").await.unwrap_err();
        assert!(matches!(err, SwarmClientError::WebSocket(_)));
    }
}
