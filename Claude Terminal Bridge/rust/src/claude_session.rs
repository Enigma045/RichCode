use crate::message::{Message, MessageStatus};
use crate::db::Database;

pub struct ClaudeSession {
    pub current_conversation_id: Option<String>,
    pub connected: bool,
    pub db: Option<Database>,
    /// While true, the session follows whichever conversation the browser
    /// tab reports itself to be on (see `active_tab_conversation_id`).
    /// Turned off by an explicit /select, and back on by
    /// resume_auto_follow().
    pub auto_follow: bool,
    /// The conversation id the Claude browser tab most recently reported
    /// itself to be on, via ConversationActive. This is the target for new
    /// local echoes of what you type, and (while auto_follow is on) also
    /// drives what the TUI displays.
    ///
    /// This used to be inferred from `current_conversation_id`, i.e.
    /// "whichever conversation most recently received a message" - but that
    /// id is reconstructed at startup from the *last run's* last-active
    /// conversation, which is almost never the conversation the tab is
    /// actually sitting on right now. Typing a message would locally-echo
    /// it into that stale id while the reply that actually came back landed
    /// under the tab's real (different) id, so the two halves of the
    /// exchange never shared a conversation - your own message and Claude's
    /// reply could each end up in a bucket the other one wasn't in,
    /// depending on which had touched the DB most recently. Tracking the
    /// tab's own report directly removes the inference entirely.
    pub active_tab_conversation_id: Option<String>,
}

impl ClaudeSession {
    pub fn new() -> Self {
        let db_path = Self::db_path();
        log::info!("Opening database at {:?}", db_path);
        let db = Database::new(&db_path).ok();
        let mut current_conversation_id = None;
        
        if let Some(ref db_instance) = db {
            current_conversation_id = match db_instance.get_last_active_conversation() {
                Ok(Some(id)) => Some(id),
                _ => None,
            };
        }
        
        Self {
            current_conversation_id,
            connected: false,
            db,
            auto_follow: true,
            active_tab_conversation_id: None,
        }
    }

    /// Resolves to `memory.db` next to the running executable, rather than
    /// a bare relative path. A relative "memory.db" resolves against the
    /// process's current working directory, which differs depending on how
    /// the app is launched (`cargo run` vs. double-clicking the .exe vs. a
    /// shortcut vs. a parent process) — that mismatch can silently point
    /// different launches at two different DB files, so messages appear to
    /// save "somewhere" but never show up on the next run.
    fn db_path() -> std::path::PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("memory.db")))
            .unwrap_or_else(|| std::path::PathBuf::from("memory.db"))
    }

    pub fn update_message(&mut self, conv_id: String, msg_id: String, role: String, content: String, status: MessageStatus) {
        let msg = Message {
            id: msg_id.clone(),
            role: role.clone(),
            content: content.clone(),
            status: status.clone(),
        };
        
        if let Some(ref db) = self.db {
            if let Err(e) = db.upsert_message(&conv_id, &msg) {
                log::error!("Failed to save message to DB: {}", e);
            }
        }

        if self.auto_follow || self.current_conversation_id.is_none() || self.current_conversation_id.as_deref() == Some("pending") {
            if conv_id != "pending" || self.current_conversation_id.is_none() {
                self.current_conversation_id = Some(conv_id);
            }
        }
    }

    /// Called when the extension reports (via ConversationActive) which
    /// conversation the Claude browser tab is currently on. This is the
    /// single source of truth for both (a) where new local echoes of your
    /// own typed messages should go, and (b) while auto_follow is on, what
    /// the TUI displays - replacing the old inference from message arrival
    /// order.
    pub fn set_active_tab_conversation(&mut self, conv_id: &str) {
        self.active_tab_conversation_id = Some(conv_id.to_string());
        if self.auto_follow {
            self.current_conversation_id = Some(conv_id.to_string());
        }
    }

    /// Re-homes an in-memory conversation (and its DB rows) from `old_id` to
    /// `new_id`. Used when the extension reports that a conversation it was
    /// tracking under a placeholder id has been assigned its real id, so a
    /// single chat doesn't end up split into two unrelated conversations.
    pub fn rekey_conversation(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id {
            return;
        }

        if let Some(ref db) = self.db {
            if let Err(e) = db.rekey_conversation(old_id, new_id) {
                log::error!("Failed to rekey conversation in DB: {}", e);
            }
        }

        if self.current_conversation_id.as_deref() == Some(old_id) {
            self.current_conversation_id = Some(new_id.to_string());
        }
        if self.active_tab_conversation_id.as_deref() == Some(old_id) {
            self.active_tab_conversation_id = Some(new_id.to_string());
        }
    }

    /// Manually pin the active conversation (used by /select). Returns
    /// false if no such conversation is known.
    pub fn select_conversation(&mut self, conv_id: String) -> bool {
        self.current_conversation_id = Some(conv_id);
        self.auto_follow = false;
        true
    }

    /// Finds a known conversation id starting with `prefix`, most-recently-
    /// active match first. Conversations now live only in the DB (there's
    /// no in-memory map to scan), so this delegates to it. Returns None if
    /// there's no database or nothing matches.
    pub fn find_conversation_by_prefix(&self, prefix: &str) -> Option<String> {
        let db = self.db.as_ref()?;
        let ids = db.get_conversation_ids().ok()?;
        ids.into_iter().find(|id| id.starts_with(prefix))
    }

    /// Resume following whatever conversation the browser tab is on. Prefers
    /// the tab's own report (active_tab_conversation_id) since that reflects
    /// reality right now; only falls back to the DB's last-active record if
    /// the extension hasn't reported anything yet this run.
    pub fn resume_auto_follow(&mut self) {
        self.auto_follow = true;
        if let Some(ref id) = self.active_tab_conversation_id {
            self.current_conversation_id = Some(id.clone());
        } else if let Some(ref db) = self.db {
            if let Ok(Some(id)) = db.get_last_active_conversation() {
                self.current_conversation_id = Some(id);
            }
        }
    }

    pub fn clear(&mut self) {
        if let Some(ref db) = self.db {
            let _ = db.clear_all();
        }
        self.current_conversation_id = None;
        self.auto_follow = true;
    }
    
    pub fn get_current_messages(&self) -> Vec<Message> {
        if let Some(ref id) = self.current_conversation_id {
            if let Some(ref db) = self.db {
                if let Ok(messages) = db.get_messages_for_conversation(id) {
                    return messages;
                }
            }
        }
        Vec::new()
    }
}