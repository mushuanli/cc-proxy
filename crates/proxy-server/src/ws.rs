use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use proxy_common::WsMessage;
use tokio::time::interval;

use crate::AppState;

const PING_INTERVAL: Duration = Duration::from_secs(10);
const DEAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Check if an Origin header value is allowed to connect.
fn is_allowed_origin(origin: &str) -> bool {
    origin.starts_with("http://localhost") || origin.starts_with("http://127.0.0.1")
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Validate Origin header to prevent cross-site WebSocket hijacking
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if !is_allowed_origin(origin) {
            tracing::warn!("[ws] rejected connection from origin: {}", origin);
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // ── Send initial snapshot ──
    send_json(
        &mut sender,
        &WsMessage::McpConfigChanged {
            destination_url: state.mcp.get_destination().await,
        },
    )
    .await;

    send_json(
        &mut sender,
        &WsMessage::TeeStatusChanged {
            enabled: state.capture.is_enabled(),
        },
    )
    .await;

    // ── Main loop ──
    let mut rx = state.events.subscribe();
    let mut ping_ticker = interval(PING_INTERVAL);
    ping_ticker.tick().await;

    let mut last_pong = tokio::time::Instant::now();
    let mut last_warned: u8 = 0;

    loop {
        tokio::select! {
            _ = ping_ticker.tick() => {
                let elapsed = last_pong.elapsed().as_secs();
                if elapsed > 240 && last_warned < 3 {
                    tracing::warn!("WS no pong for {}s", elapsed);
                    last_warned = 3;
                } else if elapsed > 220 && last_warned < 2 {
                    last_warned = 2;
                } else if elapsed > 200 && last_warned < 1 {
                    last_warned = 1;
                }
                if elapsed > DEAD_TIMEOUT.as_secs() {
                    break;
                }
                if sender.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }

            msg = rx.recv() => {
                match msg {
                    Ok(ws_msg) => {
                        if let Ok(json) = serde_json::to_string(&ws_msg) {
                            if sender.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WS lagged by {} messages", n);
                        send_json(&mut sender, &WsMessage::Resync).await;
                    }
                    Err(_) => break,
                }
            }

            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Pong(_))) => {
                        last_pong = tokio::time::Instant::now();
                        last_warned = 0;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = sender.send(Message::Pong(data.to_vec())).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    let _ = sender.send(Message::Close(None)).await;
}

async fn send_json<T: serde::Serialize>(sender: &mut SplitSink<WebSocket, Message>, msg: &T) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = sender.send(Message::Text(json)).await;
    }
}
