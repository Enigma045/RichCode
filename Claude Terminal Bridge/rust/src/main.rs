mod claude_session;
mod commands;
mod config;
mod db;
mod message;
mod protocol;
mod richbot_filter;
mod sandbox;
mod terminal;
mod websocket;

use log::error;
use tokio::sync::mpsc;
use config::Config;
use protocol::{ClientMessage, ServerMessage};

/// Printing to stderr corrupts the Ratatui alternate-screen TUI, so instead of
/// disabling logging entirely (which was silently swallowing every error!/warn!
/// call from websocket.rs and terminal.rs — including bind failures, handshake
/// failures, and raw-mode init failures), we route all log output to a file.
fn init_file_logger() -> std::io::Result<()> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("bridge.log")?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Err(e) = init_file_logger() {
        // If we can't even open the log file, say so on stderr BEFORE we
        // enter the TUI's alternate screen, since after that point nothing
        // printed to stderr will be visible to the user.
        eprintln!("Failed to initialize file logger (bridge.log): {}", e);
    }

    let config = Config::default();

    let (tx_to_ws, rx_from_ui) = mpsc::channel::<ServerMessage>(100);
    let (tx_to_ui, rx_from_ws) = mpsc::channel::<ClientMessage>(100);

    let token = config.session_token.clone();

    // Write the token to a file so it's easy to copy without fighting the terminal
    if let Err(e) = std::fs::write("session_token.txt", &token) {
        eprintln!("Warning: failed to write session_token.txt: {}", e);
    }

    // Start WebSocket Server. If the port is already taken (e.g. a previous
    // instance is still running), bind() fails inside run() and previously
    // that failure vanished into a no-op logger, leaving you staring at a
    // TUI that never connects with zero indication why. We surface that
    // failure back to the TUI via a dedicated channel instead.
    let (tx_bind_result, rx_bind_result) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let ws_server = websocket::WebSocketServer::new(config.port, token.clone(), tx_to_ui, rx_from_ui);
    tokio::spawn(async move {
        ws_server.run(tx_bind_result).await;
    });

    // Start TUI
    let mut app = terminal::TerminalApp::new(tx_to_ws, rx_from_ws, config.sandbox_dir.clone(), config.attach_keyword.clone());
    if let Err(e) = app.run(&token, rx_bind_result).await {
        error!("Terminal error: {}", e);
        // enable_raw_mode()/EnterAlternateScreen failures land here. Without
        // this, app.run() would return Err, main() would exit immediately,
        // and — since logging used to be disabled — you'd get a console
        // window that opens and closes with literally no output.
        eprintln!("Terminal failed to start: {}. See bridge.log for details.", e);
        std::process::exit(1);
    }

    Ok(())
}
