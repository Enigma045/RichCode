use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use std::collections::VecDeque;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::{accept_async, tungstenite::Message};

/// Cap on how many outgoing messages we'll hold onto while there's no live,
/// authenticated extension connection. Bounded so a long outage can't grow
/// this without limit; oldest is dropped first (with a warning) if it fills.
const MAX_PENDING: usize = 50;


use crate::protocol::{ClientMessage, ServerMessage};

pub struct WebSocketServer {
    port: u16,
    token: String,
    tx_to_ui: mpsc::Sender<ClientMessage>,
    rx_from_ui: mpsc::Receiver<ServerMessage>,
}

impl WebSocketServer {
    pub fn new(port: u16, token: String, tx_to_ui: mpsc::Sender<ClientMessage>, rx_from_ui: mpsc::Receiver<ServerMessage>) -> Self {
        Self {
            port,
            token,
            tx_to_ui,
            rx_from_ui,
        }
    }

    pub async fn run(mut self, tx_bind_result: tokio::sync::oneshot::Sender<Result<(), String>>) {
        let addr = format!("127.0.0.1:{}", self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                let msg = format!(
                    "Failed to bind WebSocket server to {} ({}). Is another instance already running?",
                    addr, e
                );
                error!("{}", msg);
                // Previously this error only went to a logger that was never
                // initialized, so it vanished. Now it's both logged to
                // bridge.log AND surfaced to the TUI so the user actually
                // sees why nothing ever connects.
                let _ = tx_bind_result.send(Err(msg));
                return;
            }
        };

        info!("WebSocket server listening on {}", addr);
        let _ = tx_bind_result.send(Ok(()));

        let mut active_tx: Option<mpsc::Sender<ServerMessage>> = None;

        // Messages waiting for a live connection. Previously a message typed
        // during any gap (extension disconnected, or - see handle_connection -
        // connected but not yet past its Hello handshake) was gone for good.
        // Now it sits here until something can actually take it.
        let mut pending: VecDeque<ServerMessage> = VecDeque::new();

        // Periodic nudge to retry flushing `pending` even when no new UI
        // message or new connection triggers a flush attempt directly (e.g.
        // a channel that was briefly full).
        let mut retry_tick = tokio::time::interval(std::time::Duration::from_millis(500));

        loop {
            tokio::select! {
                Ok((stream, addr)) = listener.accept() => {
                    let (tx_ws, rx_ws) = mpsc::channel::<ServerMessage>(64);
                    active_tx = Some(tx_ws);

                    let tx_to_ui_clone = self.tx_to_ui.clone();
                    let token = self.token.clone();

                    tokio::spawn(async move {
                        handle_connection(stream, addr, token, tx_to_ui_clone, rx_ws).await;
                    });

                    Self::flush_pending(&mut active_tx, &mut pending);
                }

                // Messages from UI to send to Chrome extension
                Some(msg) = self.rx_from_ui.recv() => {
                    if pending.len() >= MAX_PENDING {
                        pending.pop_front();
                        warn!("Pending outgoing queue full; dropped oldest queued message");
                    }
                    pending.push_back(msg);

                    let had_connection = active_tx.is_some();
                    Self::flush_pending(&mut active_tx, &mut pending);

                    // Only bother the user with a queued notice when there's
                    // genuinely no connection to hand it to yet - if a
                    // connection exists, flush_pending almost always drains
                    // it immediately and a notice would just be noise.
                    if !pending.is_empty() && !had_connection {
                        let _ = self.tx_to_ui.send(ClientMessage::Diagnostic {
                            message: format!(
                                "Message queued ({} pending): extension is disconnected. Will send automatically once it reconnects.",
                                pending.len()
                            ),
                        }).await;
                    }
                }

                _ = retry_tick.tick() => {
                    Self::flush_pending(&mut active_tx, &mut pending);
                }
            }
        }
    }

    /// Tries to hand off as much of `pending` as possible to the current
    /// connection's channel, in order, stopping at the first failure (full
    /// or closed channel) so nothing gets skipped or reordered. A closed
    /// channel means that connection's task has already exited, so we also
    /// clear `active_tx` here rather than leaving a dead sender around.
    fn flush_pending(active_tx: &mut Option<mpsc::Sender<ServerMessage>>, pending: &mut VecDeque<ServerMessage>) {
        let Some(tx) = active_tx.as_ref() else { return };

        while let Some(msg) = pending.pop_front() {
            match tx.try_send(msg) {
                Ok(()) => continue,
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    pending.push_front(msg);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
                    pending.push_front(msg);
                    *active_tx = None;
                    break;
                }
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    expected_token: String,
    tx_to_ui: mpsc::Sender<ClientMessage>,
    mut rx_ws: mpsc::Receiver<ServerMessage>,
) {
    info!("New connection from {}", addr);
    
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed: {}", e);
            return;
        }
    };
    
    let (mut write, mut read) = ws_stream.split();
    let mut authenticated = false;

    // Messages that arrived on rx_ws before the Hello handshake finished.
    // Previously these were received off the channel and then silently
    // discarded (the `if authenticated { write... }` below had no `else`),
    // which is an easy race to lose: the run() loop can flush a queued
    // message into this connection's channel within microseconds of
    // accept(), well before the extension's Hello frame has round-tripped.
    // Now we hold onto them and send them the moment auth succeeds, in order.
    let mut queued_before_auth: Vec<ServerMessage> = Vec::new();

    loop {
        tokio::select! {
            Some(msg_res) = read.next() => {
                match msg_res {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(client_msg) => {
                                match &client_msg {
                                    ClientMessage::Hello { token, .. } => {
                                        if let Some(ref t) = token {
                                            if t.trim() == expected_token.trim() {
                                                authenticated = true;
                                                let ack = ServerMessage::HelloAck {
                                                    server: "claude-terminal".to_string(),
                                                    version: "1.0".to_string(),
                                                };
                                                if let Ok(json) = serde_json::to_string(&ack) {
                                                    let _ = write.send(Message::Text(json)).await;
                                                }
                                                info!("Extension authenticated successfully");

                                                // Previously this success path went straight to `continue`
                                                // without ever forwarding the Hello to tx_to_ui, so
                                                // terminal.rs's `session.connected = true` on
                                                // ClientMessage::Hello was dead code - the header's
                                                // "Connected" indicator never actually flipped true.
                                                let _ = tx_to_ui.send(client_msg.clone()).await;

                                                // Flush anything that arrived (and was held) before we
                                                // got here, oldest first.
                                                for queued in queued_before_auth.drain(..) {
                                                    if let Ok(json) = serde_json::to_string(&queued) {
                                                        if let Err(e) = write.send(Message::Text(json)).await {
                                                            warn!("Failed to send queued message to extension: {}", e);
                                                            break;
                                                        }
                                                    }
                                                }

                                                continue;
                                            }
                                        }
                                        warn!("Extension provided invalid token");
                                        let err = ServerMessage::Error { message: "Invalid token".into() };
                                        if let Ok(json) = serde_json::to_string(&err) {
                                            let _ = write.send(Message::Text(json)).await;
                                        }
                                        break; // Disconnect
                                    },
                                    ClientMessage::Pong => {
                                        // Handle pong
                                        continue;
                                    }
                                    _ => {}
                                }

                                if !authenticated {
                                    warn!("Message received before authentication");
                                    continue;
                                }

                                // Forward to UI
                                let _ = tx_to_ui.send(client_msg).await;
                            }
                            Err(e) => warn!("Failed to parse message from extension: {}", e),
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("Connection closed by extension");
                        break;
                    }
                    Err(e) => {
                        warn!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            
            Some(server_msg) = rx_ws.recv() => {
                if authenticated {
                    if let Ok(json) = serde_json::to_string(&server_msg) {
                        if let Err(e) = write.send(Message::Text(json)).await {
                            warn!("Failed to send message to extension: {}", e);
                            break;
                        }
                    }
                } else {
                    // Hold it - flushed above the moment auth succeeds. If
                    // this connection instead ends without ever
                    // authenticating, the block after the loop reports the
                    // loss instead of silently dropping it.
                    queued_before_auth.push(server_msg);
                }
            }
        }
    }
    
    // If this connection never made it past Hello (bad token, dropped
    // mid-handshake, etc.), anything queued in `queued_before_auth` can't be
    // handed back to run()'s own pending queue - that would need a queue
    // shared across every connection attempt, not just this one. Rather than
    // eating those messages with no trace, at least tell the user so it's a
    // visible gap instead of a silent one.
    if !authenticated && !queued_before_auth.is_empty() {
        warn!("{} outgoing message(s) lost: connection ended before authenticating", queued_before_auth.len());
        let _ = tx_to_ui.send(ClientMessage::Diagnostic {
            message: format!(
                "{} message(s) were lost: the connection closed before the extension finished authenticating. Please resend.",
                queued_before_auth.len()
            ),
        }).await;
    }

    // Covers every exit path above (invalid token, extension-initiated
    // close, socket error, or the outer select! loop exiting for any other
    // reason) so the TUI's "Connected" indicator - which was previously only
    // ever set to true on Hello and never set back to false - actually
    // reflects reality instead of continuing to claim a dead connection is
    // live.
    let _ = tx_to_ui.send(ClientMessage::Disconnected).await;

    info!("Connection from {} ended", addr);
}
