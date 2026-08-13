use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal as RatatuiTerminal,
};
use std::{collections::HashSet, io, time::Duration};
use tokio::sync::mpsc;

use crate::{
    richbot_filter::{ClaudeResponse, RichBotFilter},
    claude_session::ClaudeSession,
    commands::{parse_command, CommandAction},
    protocol::{ClientMessage, ServerMessage},
};

pub struct TerminalApp {
    tx_to_ws: mpsc::Sender<ServerMessage>,
    rx_from_ws: mpsc::Receiver<ClientMessage>,
    session: ClaudeSession,
    input: String,
    system_message: Option<String>,
    scroll_offset: u16,
    auto_scroll: bool,
    bind_failed: bool,
    sandbox_dir: std::path::PathBuf,
    attach_keyword: String,
    /// Assistant message IDs whose live attachment requests have already been handled.
    /// This prevents streaming updates from triggering the same request repeatedly.
    handled_ai_attachment_requests: HashSet<String>,
    /// Relative file paths that have already been requested during this session to prevent infinite attachment loops.
    requested_files: HashSet<String>,
    richbot_filter: RichBotFilter,
    richbot_status: String,
}

/// Breaks `s` into chunks of at most `width` characters. Used so that one
/// logical `Line` we push always corresponds to exactly one rendered row —
/// otherwise `Paragraph`'s own line-wrapping (enabled via `Wrap`) can spread
/// a single long line across multiple screen rows that our line-count-based
/// scroll math never accounted for, silently clipping the tail of long
/// messages off the bottom of the pane.
fn hard_wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    if s.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Extract one or more AI attachment requests from an assistant message.
///
/// Syntax:
///     [[ATTACH_FILE:relative/path/to/file.ext]]
///
/// Paths are interpreted relative to the configured sandbox. The actual path
/// is validated against the sandbox root before anything is sent to Chrome.
fn extract_attachment_requests(content: &str) -> Vec<String> {
    const PREFIX: &str = "[[ATTACH_FILE:";
    const SUFFIX: &str = "]]";

    let mut requests = Vec::new();
    let mut search_from = 0;

    while let Some(start_rel) = content[search_from..].find(PREFIX) {
        let start = search_from + start_rel + PREFIX.len();
        let Some(end_rel) = content[start..].find(SUFFIX) else {
            break;
        };
        let end = start + end_rel;
        let path = content[start..end].trim();

        if !path.is_empty()
            && path.len() <= 1024
            && !path.contains('\n')
            && !path.contains('\r')
        {
            if !requests.iter().any(|p| p == path) {
                requests.push(path.to_string());
            }
        }

        search_from = end + SUFFIX.len();
    }

    requests
}

fn clean_path(p: &std::path::Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

impl TerminalApp {
    pub fn new(tx_to_ws: mpsc::Sender<ServerMessage>, rx_from_ws: mpsc::Receiver<ClientMessage>, sandbox_dir: std::path::PathBuf, attach_keyword: String) -> Self {
        Self {
            tx_to_ws,
            rx_from_ws,
            session: ClaudeSession::new(),
            input: String::new(),
            system_message: Some("Waiting for Chrome extension...".into()),
            scroll_offset: 0,
            auto_scroll: true,
            bind_failed: false,
            sandbox_dir,
            attach_keyword,
            handled_ai_attachment_requests: HashSet::new(),
            requested_files: HashSet::new(),
            richbot_filter: RichBotFilter::from_environment(),
            richbot_status: "🟢 Active (Monitoring Claude responses for sandbox file requests)".to_string(),
        }
    }

    pub async fn run(
        &mut self,
        token: &str,
        mut rx_bind_result: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = RatatuiTerminal::new(backend)?;

        self.system_message = Some(format!("Claude Terminal Bridge v1.0\nSession token: {}\nWaiting for connection...", token));

        loop {
            // Handle incoming WS messages
            while let Ok(msg) = self.rx_from_ws.try_recv() {
                self.handle_client_message(msg).await;
            }

            // Surface whether the WebSocket server actually managed to bind
            // its port. Previously a bind failure (e.g. port 8765 already in
            // use by a prior instance) was logged nowhere and the TUI just
            // sat forever on "Waiting for connection..." with no way to tell
            // that the server never even started.
            if !self.bind_failed {
                match rx_bind_result.try_recv() {
                    Ok(Ok(())) => {
                        // Bound successfully; leave the normal "waiting for
                        // connection" message in place.
                    }
                    Ok(Err(msg)) => {
                        self.bind_failed = true;
                        self.system_message = Some(format!(
                            "SERVER FAILED TO START: {}\nCheck bridge.log. Change the port in config.rs or stop the other instance, then relaunch.",
                            msg
                        ));
                    }
                    Err(_) => {
                        // Not resolved yet (Empty) or already consumed
                        // (Closed after first Ok) — nothing to do either way.
                    }
                }
            }

            terminal.draw(|f| self.ui(f))?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.session.clear();
                            self.system_message = Some("Cleared history".into());
                        }
                        KeyCode::Enter => {
                            if !self.input.is_empty() {
                                let input = self.input.clone();
                                self.input.clear();
                                self.handle_input(input).await;
                            }
                        }
                        KeyCode::Char(c) => {
                            self.input.push(c);
                        }
                        KeyCode::Backspace => {
                            self.input.pop();
                        }
                        KeyCode::PageUp => {
                            self.scroll_offset = self.scroll_offset.saturating_sub(5);
                            self.auto_scroll = false;
                        }
                        KeyCode::PageDown => {
                            self.scroll_offset = self.scroll_offset.saturating_add(5);
                        }
                        KeyCode::Up => {
                            self.scroll_offset = self.scroll_offset.saturating_sub(1);
                            self.auto_scroll = false;
                        }
                        KeyCode::Down => {
                            self.scroll_offset = self.scroll_offset.saturating_add(1);
                        }
                        _ => {}
                    }
                }
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        Ok(())
    }

    async fn handle_client_message(&mut self, msg: ClientMessage) {
        match msg {
            ClientMessage::Hello { .. } => {
                self.session.connected = true;
                self.system_message = Some("Extension connected.".into());
            }
            ClientMessage::Disconnected => {
                self.session.connected = false;
                self.system_message = Some("Extension disconnected. Waiting for reconnect...".into());
            }
            ClientMessage::AssistantMessage { conversation_id, message_id, role, content, status, historical } => {
                use crate::message::MessageStatus;
                let msg_status = if status == "streaming" {
                    MessageStatus::Streaming
                } else {
                    MessageStatus::Complete
                };

                // Save/display the assistant message normally.
                self.session.update_message(
                    conversation_id.clone(),
                    message_id.clone(),
                    role.clone(),
                    content.clone(),
                    msg_status,
                );

                // A live Claude response can request an exact sandbox file by
                // emitting: [[ATTACH_FILE:path/to/file]]
                //
                // History/resync messages are deliberately ignored here so
                // reopening the extension cannot replay old attachment
                // requests. The message id is also used as a de-duplication
                // key because streaming responses arrive multiple times.
                if role == "assistant" && !historical && status != "streaming" {
                    // Deduplicate processing so a completed Claude message is only processed once.
                    let message_key = format!("{}:{}", conversation_id, message_id);

                    if self.handled_ai_attachment_requests.insert(message_key) {
                        // 1. Check whether Claude returned generated files JSON to write into sandbox
                        let filter = self.richbot_filter.clone();
                        let content_clone = content.clone();
                        let sandbox_clone = self.sandbox_dir.clone();

                        let response_result = tokio::task::spawn_blocking(move || {
                            filter.process_claude_response(&content_clone, &sandbox_clone)
                        }).await;

                        match response_result {
                            Ok(Ok(ClaudeResponse::FilesWritten(files))) => {
                                if files.is_empty() {
                                    self.richbot_status = "⚠️ Claude returned a file object, but no files were written.".to_string();
                                } else {
                                    self.richbot_status = format!(
                                        "✅ Wrote {} file{} to sandbox: {}",
                                        files.len(),
                                        if files.len() == 1 { "" } else { "s" },
                                        files.join(", ")
                                    );

                                    self.system_message = Some(format!(
                                        "Claude created {} file{} in {}",
                                        files.len(),
                                        if files.len() == 1 { "" } else { "s" },
                                        self.sandbox_dir.display()
                                    ));
                                }

                                // Generated files written - do not process as sandbox file attachment request
                                return;
                            }
                            Ok(Ok(ClaudeResponse::Text(_))) => {
                                // Normal text response - proceed with sandbox file attachment detection
                            }
                            Ok(Err(e)) => {
                                self.richbot_status = format!("❌ Failed to write Claude files: {}", e);
                                self.system_message = Some(format!("Claude file-write error: {}", e));
                            }
                            Err(e) => {
                                self.richbot_status = format!("❌ Task error: {}", e);
                            }
                        }

                        // 2. Existing sandbox-file attachment detection
                        let mut requests = match self.richbot_filter.extract_paths(&content, &self.sandbox_dir).await {
                            Ok(paths) => paths,
                            Err(e) => {
                                let err_msg = format!("Filter error: {}", e);
                                self.system_message = Some(err_msg.clone());
                                self.richbot_status = format!("❌ {}", err_msg);
                                Vec::new()
                            }
                        };

                        // Keep the explicit marker as a deterministic fallback
                        for path in extract_attachment_requests(&content) {
                            if !requests.iter().any(|p| p == &path) {
                                requests.push(path);
                            }
                        }

                        // Filter out files that have ALREADY been requested during this session to prevent infinite attachment loops
                        let new_requests: Vec<String> = requests
                            .into_iter()
                            .filter(|p| !self.requested_files.contains(p))
                            .collect();

                        if !new_requests.is_empty() {
                            let richbot_msg = format!("📂 Requested sandbox files: {}", new_requests.join(", "));
                            self.richbot_status = richbot_msg;

                            for relative_path in new_requests {
                                self.requested_files.insert(relative_path.clone());
                                self.request_sandbox_file(&relative_path).await;
                            }
                        }
                    }
                }

                // We will handle auto-scroll in the UI render function to avoid scrolling into the void.
            }
            ClientMessage::RekeyConversation { old_conversation_id, new_conversation_id } => {
                self.session.rekey_conversation(&old_conversation_id, &new_conversation_id);
            }
            ClientMessage::ConversationActive { conversation_id } => {
                self.session.set_active_tab_conversation(&conversation_id);
            }
            ClientMessage::Diagnostic { message } => {
                // Surfaced in the status line only - never written into
                // conversation history/DB, so it can't be mistaken for
                // (or accidentally re-sent as) a real chat message.
                self.system_message = Some(format!("[Extension diagnostic] {}", message));
            }
            _ => {}
        }
    }

    async fn request_sandbox_file(&mut self, relative_path: &str) {
        if !self.sandbox_dir.exists() {
            let _ = std::fs::create_dir_all(&self.sandbox_dir);
        }

        let raw_root = match std::fs::canonicalize(&self.sandbox_dir) {
            Ok(path) => path,
            Err(e) => {
                self.system_message = Some(format!(
                    "Requested '{}', but sandbox directory could not be resolved: {}",
                    relative_path, e
                ));
                return;
            }
        };
        let sandbox_root = clean_path(&raw_root);

        // Normalize relative path (strip leading ./ or sandbox/ prefix if provided)
        let clean_rel = relative_path
            .trim_start_matches("./")
            .trim_start_matches('\\')
            .trim_start_matches('/');

        let clean_rel = if let Some(stripped) = clean_rel.strip_prefix("sandbox/") {
            stripped
        } else {
            clean_rel
        };

        let candidate = sandbox_root.join(clean_rel);
        let requested = match std::fs::canonicalize(&candidate) {
            Ok(path) => clean_path(&path),
            Err(_) => {
                let fallback = sandbox_root.join(relative_path);
                match std::fs::canonicalize(&fallback) {
                    Ok(path) => clean_path(&path),
                    Err(e) => {
                        self.system_message = Some(format!(
                            "Sandbox file '{}' does not exist in {}: {}",
                            relative_path, self.sandbox_dir.display(), e
                        ));
                        return;
                    }
                }
            }
        };

        if !requested.starts_with(&sandbox_root) || !requested.is_file() {
            self.system_message = Some(format!(
                "Blocked attachment outside sandbox: {}",
                relative_path
            ));
            return;
        }

        let path = requested.to_string_lossy().into_owned();
        let prompt = format!(
            "The file '{}' has been attached from the local sandbox workspace.",
            clean_rel
        );

        match self.tx_to_ws.send(ServerMessage::AttachFile { path, prompt }).await {
            Ok(()) => {
                self.richbot_status = format!("📂 Attached sandbox file: {}", clean_rel);
                self.system_message = Some(format!(
                    "Sending requested sandbox file '{}' to Claude...",
                    clean_rel
                ));
            }
            Err(e) => {
                self.system_message = Some(format!(
                    "Failed to send attachment request for '{}': {}",
                    clean_rel, e
                ));
            }
        }
    }

    async fn handle_input(&mut self, input: String) {
        let cmd = if input.trim().eq_ignore_ascii_case(&self.attach_keyword) {
            Some(CommandAction::Attach(None))
        } else {
            parse_command(&input)
        };

        if let Some(cmd) = cmd {
            match cmd {
                CommandAction::Attach(file_opt) => {
                    if let Some(target_file) = file_opt {
                        self.request_sandbox_file(&target_file).await;
                    } else {
                        match crate::sandbox::create_file_manifest(&self.sandbox_dir) {
                            Ok(manifest_path) => {
                                let manifest_path = match std::fs::canonicalize(&manifest_path) {
                                    Ok(path) => clean_path(&path),
                                    Err(e) => {
                                        self.system_message = Some(format!(
                                            "Manifest created, but absolute path could not be resolved: {}",
                                            e
                                        ));
                                        return;
                                    }
                                };
                                let prompt = format!(
                                    "Please use the attached sandbox_file_names.txt as the file manifest. Read the manifest to understand which files are available in the sandbox."
                                );
                                let _ = self.tx_to_ws.send(ServerMessage::AttachFile {
                                    path: manifest_path.to_string_lossy().into_owned(),
                                    prompt,
                                }).await;
                                self.richbot_status = "📂 Attached sandbox manifest (sandbox_file_names.txt)".to_string();
                                self.system_message = Some(format!(
                                    "Created {} and sent it to Claude for automatic attachment.",
                                    manifest_path.display()
                                ));
                            }
                            Err(e) => {
                                self.system_message = Some(format!(
                                    "Failed to create sandbox manifest in {}: {}",
                                    self.sandbox_dir.display(),
                                    e
                                ));
                            }
                        }
                    }
                }
                CommandAction::Help => {
                    self.system_message = Some(format!(
                        "Commands: {} (scan sandbox + attach manifest), /richbot <prompt> (or /rb), /help, /status, /select [id-prefix], /clear, /quit",
                        self.attach_keyword
                    ));
                }
                CommandAction::Status => {
                    let status = format!(
                        "Extension Connected: {}\nActive Conversation: {:?}\nBrowser Tab Conversation: {:?}\nAuto-follow: {}",
                        self.session.connected,
                        self.session.current_conversation_id,
                        self.session.active_tab_conversation_id,
                        self.session.auto_follow
                    );
                    self.system_message = Some(status);
                }
                CommandAction::Select(query) => {
                    if query.is_empty() {
                        // No id given: resume following the most recently
                        // active conversation instead of a pinned one.
                        self.session.resume_auto_follow();
                        self.system_message = Some(format!(
                            "Following most recent conversation: {:?}",
                            self.session.current_conversation_id
                        ));
                    } else {
                        let matched = self.session.find_conversation_by_prefix(&query);
                        match matched {
                            Some(id) => {
                                self.session.select_conversation(id.clone());
                                self.system_message = Some(format!("Switched to conversation {} (auto-follow off)", id));
                            }
                            None => {
                                self.system_message = Some(format!("No conversation matching '{}'", query));
                            }
                        }
                    }
                }
                CommandAction::Clear => {
                    self.session.clear();
                    self.requested_files.clear();
                    self.handled_ai_attachment_requests.clear();
                    self.system_message = Some("Cleared".into());
                }
                CommandAction::Quit => {
                    std::process::exit(0);
                }
                CommandAction::RichBot(prompt) => {
                    if prompt.is_empty() {
                        self.system_message = Some("Usage: /richbot <your prompt> (or /rb <prompt>)".to_string());
                    } else {
                        self.richbot_status = format!("⏳ Processing prompt: '{}'...", prompt);
                        let prompt_clone = prompt.clone();
                        let response = tokio::task::spawn_blocking(move || {
                            richbot::model::set_control_with_persona(&prompt_clone, "Quick")
                        }).await.unwrap_or_else(|e| format!("Error calling RichBot: {}", e));

                        self.richbot_status = format!("💬 {}", response);
                    }
                }
                _ => {
                    self.system_message = Some(format!("Unimplemented command: {}", input));
                }
            }
            return;
        }

        // Locally echo the message immediately instead of waiting for the
        // extension to detect it in claude.ai's DOM and round-trip it back.
        // The terminal already knows exactly what was typed - making that
        // the single source of truth for the user's own turns removes an
        // entire dependency on correctly reverse-engineering claude.ai's
        // live markup (which has already been the root cause of two other
        // bugs this session).
        //
        // Target this at active_tab_conversation_id (what the extension has
        // told us the browser tab is actually on), NOT
        // current_conversation_id. The latter is reconstructed at startup
        // from whichever conversation last received a message *in a
        // previous run* and only falls back to "pending" when it's None -
        // which, after the very first use, it never is. That mismatch was
        // sending your own typed messages into a stale leftover
        // conversation while Claude's reply landed under the tab's real,
        // different id, so the two never shared a bucket.
        let conv_id = self
            .session
            .active_tab_conversation_id
            .clone()
            .unwrap_or_else(|| "pending".to_string());
        let local_id = format!("local_{}", uuid::Uuid::new_v4());

        // Make sure whatever we just echoed into is actually what's on
        // screen (subject to auto_follow, same as ConversationActive from
        // the extension - this just also treats "the conversation we're
        // about to type into" as its own vote for what's active, instead of
        // waiting on the extension's own report to catch up).
        self.session.set_active_tab_conversation(&conv_id);

        self.session.update_message(
            conv_id,
            local_id,
            "user".to_string(),
            input.clone(),
            crate::message::MessageStatus::Complete,
        );

        // Send message to WS
        let _ = self.tx_to_ws.send(ServerMessage::SendMessage { content: input }).await;
    }

    fn ui(&mut self, f: &mut ratatui::Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(
                [
                    Constraint::Length(3), // Header
                    Constraint::Min(8),    // Messages
                    Constraint::Length(3), // RichBot Status Field
                    Constraint::Length(3), // Input
                ]
                .as_ref(),
            )
            .split(f.size());

        // Header
        let status = if self.session.connected { "● Connected" } else { "○ Disconnected" };
        let header = Paragraph::new(format!("Claude Terminal Bridge - {}", status))
            .style(Style::default().fg(Color::Cyan))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(header, chunks[0]);

        // Messages
        let mut msgs = Vec::new();

        // Inner width of the messages pane, accounting for the left/right
        // border columns. Content is pre-wrapped to this width below so
        // that `total_lines` (computed from `msgs.len()`) matches the
        // number of rows actually rendered, and auto-scroll doesn't clip
        // the tail of long lines.
        let inner_width = chunks[1].width.saturating_sub(2) as usize;

        if let Some(sys) = &self.system_message {
            for line in format!("[System] {}", sys).lines() {
                for wrapped in hard_wrap(line, inner_width) {
                    msgs.push(Line::from(Span::styled(wrapped, Style::default().fg(Color::Yellow))));
                }
            }
            msgs.push(Line::from(""));
        }

        for msg in self.session.get_current_messages() {
            let (name, name_color) = match msg.role.as_str() {
                "user" => ("You", Color::Green),
                _ => ("Claude", Color::Magenta),
            };
            msgs.push(Line::from(Span::styled(name, Style::default().fg(name_color).add_modifier(Modifier::BOLD))));

            // Split by explicit newlines first, then hard-wrap each of
            // those to the pane width. Owned (not borrowed) since `msg`
            // is dropped at the end of this iteration but `msgs` outlives
            // the loop.
            for line in msg.content.lines() {
                for wrapped in hard_wrap(line, inner_width) {
                    msgs.push(Line::from(wrapped));
                }
            }
            msgs.push(Line::from(""));
        }

        let total_lines = msgs.len() as u16;
        let view_height = chunks[1].height.saturating_sub(2); // Subtract borders
        let max_scroll = total_lines.saturating_sub(view_height);
        
        // Auto-scroll logic
        if self.auto_scroll {
            self.scroll_offset = max_scroll;
        } else if self.scroll_offset >= max_scroll {
            // Re-enable auto-scroll if user scrolled all the way to the bottom
            self.auto_scroll = true;
            self.scroll_offset = max_scroll;
        }
        
        // Clamp manual scrolling
        let current_scroll = self.scroll_offset.min(max_scroll);
        
        let messages_widget = Paragraph::new(msgs)
            .block(Block::default().title("Conversation").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((current_scroll, 0));
        f.render_widget(messages_widget, chunks[1]);

        // RichBot Dedicated Field (chunks[2])
        let richbot_text = format!("🤖 {}", self.richbot_status);
        let richbot_widget = Paragraph::new(richbot_text)
            .style(Style::default().fg(Color::Cyan))
            .block(Block::default().title("RichBot Field").borders(Borders::ALL));
        f.render_widget(richbot_widget, chunks[2]);

        // Input (chunks[3])
        let input_prefix = "> ";
        let input_inner_width = chunks[3].width.saturating_sub(2) as usize;
        let cursor_col = input_prefix.len() + self.input.len();

        let input_scroll_x = cursor_col
            .saturating_sub(input_inner_width.saturating_sub(1).max(1))
            as u16;

        let input_text = format!("{}{}", input_prefix, self.input);
        let input_widget = Paragraph::new(input_text)
            .style(Style::default().fg(Color::Yellow))
            .block(Block::default().title("Input").borders(Borders::ALL))
            .scroll((0, input_scroll_x));
        f.render_widget(input_widget, chunks[3]);

        let visible_cursor_col = (cursor_col as u16).saturating_sub(input_scroll_x);
        f.set_cursor(
            chunks[3].x + 1 + visible_cursor_col,
            chunks[3].y + 1
        );
    }
}