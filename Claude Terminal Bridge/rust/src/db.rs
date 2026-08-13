use rusqlite::{params, Connection, OptionalExtension, Result};
use std::path::Path;
use log::info;

use crate::message::{Message, MessageStatus};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;

        // Older versions of this DB used `id TEXT PRIMARY KEY` alone. Since
        // the extension assigns message ids like `msg_idx_0` (a per-page
        // position, not a globally unique id), that let a message from one
        // conversation silently overwrite a same-indexed message from a
        // completely different conversation. Migrate any such database to a
        // composite (conv_id, id) key before doing anything else.
        if Self::needs_pk_migration(&conn)? {
            Self::migrate_to_composite_key(&conn)?;
        }

        conn.execute(
            "CREATE TABLE IF NOT EXISTS messages (
                conv_id TEXT NOT NULL,
                id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                status TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                seq INTEGER,
                PRIMARY KEY (conv_id, id)
            )",
            [],
        )?;

        // `seq` didn't exist in older databases - add it if missing so
        // upgrades don't fail on a table that predates this column.
        Self::add_seq_column_if_missing(&conn)?;

        // Also add an index on conv_id and timestamp for faster loads
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_conv_time ON messages (conv_id, timestamp)",
            [],
        )?;
        // seq is now the actual ordering key (see get_messages_for_conversation).
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_conv_seq ON messages (conv_id, seq)",
            [],
        )?;

        info!("Database initialized");
        Ok(Self { conn })
    }

    /// True if a `messages` table already exists and its primary key is the
    /// old single-column `id`, rather than the composite `(conv_id, id)`.
    fn needs_pk_migration(conn: &Connection) -> Result<bool> {
        let table_exists: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
            [],
            |row| row.get(0),
        )?;
        if table_exists == 0 {
            return Ok(false);
        }

        let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
        let mut pk_columns: Vec<String> = Vec::new();
        let rows = stmt.query_map([], |row| {
            let name: String = row.get(1)?;
            let pk_index: i64 = row.get(5)?; // 0 = not part of PK
            Ok((name, pk_index))
        })?;
        for r in rows {
            let (name, pk_index) = r?;
            if pk_index > 0 {
                pk_columns.push(name);
            }
        }

        Ok(pk_columns == vec!["id".to_string()])
    }

    /// Rebuilds `messages` with `(conv_id, id)` as the primary key,
    /// preserving existing rows. Where the old single-column key already
    /// caused a collision and data was lost, there's nothing left to
    /// recover — this just stops it from happening going forward.
    fn migrate_to_composite_key(conn: &Connection) -> Result<()> {
        info!("Migrating messages table to composite (conv_id, id) primary key");
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE messages RENAME TO messages_old;
             CREATE TABLE messages (
                 conv_id TEXT NOT NULL,
                 id TEXT NOT NULL,
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 status TEXT NOT NULL,
                 timestamp INTEGER NOT NULL,
                 seq INTEGER,
                 PRIMARY KEY (conv_id, id)
             );
             INSERT OR IGNORE INTO messages (conv_id, id, role, content, status, timestamp)
                 SELECT conv_id, id, role, content, status, timestamp FROM messages_old;
             DROP TABLE messages_old;
             COMMIT;",
        )?;
        // seq gets backfilled by add_seq_column_if_missing, called right
        // after this in Database::new.
        Ok(())
    }

    /// Adds the `seq` column to a `messages` table created before it
    /// existed, and backfills it from the existing `timestamp` ordering so
    /// old rows still sort sanely (best effort - ties from before this fix
    /// were already ambiguous and stay that way).
    fn add_seq_column_if_missing(conn: &Connection) -> Result<()> {
        let table_exists: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
            [],
            |row| row.get(0),
        )?;
        if table_exists == 0 {
            return Ok(());
        }

        let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
        let has_seq = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == "seq");

        if !has_seq {
            info!("Adding seq column to messages table");
            conn.execute("ALTER TABLE messages ADD COLUMN seq INTEGER", [])?;
        }

        // Backfill any rows still missing seq - covers both a freshly added
        // column above and the composite-key migration path, which creates
        // the column but leaves it NULL. Skip entirely if nothing to do.
        let missing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE seq IS NULL",
            [],
            |row| row.get(0),
        )?;
        if missing == 0 {
            return Ok(());
        }

        info!("Backfilling seq column for {} existing message(s)", missing);
        conn.execute(
            "UPDATE messages SET seq = (
                SELECT COUNT(*) FROM messages m2
                WHERE m2.timestamp < messages.timestamp
                   OR (m2.timestamp = messages.timestamp AND m2.rowid <= messages.rowid)
            )",
            [],
        )?;
        Ok(())
    }

    pub fn upsert_message(&self, conv_id: &str, msg: &Message) -> Result<()> {
        let status_str = match msg.status {
            MessageStatus::Streaming => "streaming",
            MessageStatus::Complete => "complete",
        };
        
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        
        // Use INSERT OR REPLACE to update existing messages. The COALESCE
        // subqueries are scoped to (conv_id, id) — previously timestamp
        // matched on `id` alone, which let messages from different
        // conversations clobber each other.
        //
        // `seq` is the real ordering key (see get_messages_for_conversation).
        // `timestamp` alone is millisecond-resolution wall clock time, and a
        // history resync fires many upserts back-to-back fast enough to land
        // in the same millisecond - SQLite gives no ordering guarantee for
        // ties, so relying on timestamp for ORDER BY let bursts of messages
        // (e.g. every extension reconnect) come back scrambled. `seq` is
        // assigned once per message (frozen via COALESCE, same as
        // timestamp) from a strictly increasing counter, so display order
        // always matches the order messages were first seen in.
        self.conn.execute(
            "INSERT OR REPLACE INTO messages (conv_id, id, role, content, status, timestamp, seq)
             VALUES (?1, ?2, ?3, ?4, ?5,
                COALESCE((SELECT timestamp FROM messages WHERE conv_id = ?1 AND id = ?2), ?6),
                COALESCE((SELECT seq FROM messages WHERE conv_id = ?1 AND id = ?2),
                          (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages))
             )",
            params![
                conv_id,
                msg.id,
                msg.role,
                msg.content,
                status_str,
                timestamp
            ],
        )?;
        
        Ok(())
    }

    /// The conversation that most recently received a message. Used to
    /// resume into the actually-active conversation on restart, instead of
    /// an arbitrary one.
    pub fn get_last_active_conversation(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT conv_id FROM messages ORDER BY timestamp DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
    }

    /// Re-homes every message under `old_id` to `new_id`. Used when the
    /// extension discovers that a conversation it was tracking under a
    /// placeholder id (e.g. a brand-new chat with no URL id yet) has been
    /// assigned its real id, so the two halves of the conversation don't
    /// stay permanently split.
    pub fn rekey_conversation(&self, old_id: &str, new_id: &str) -> Result<()> {
        if old_id == new_id {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE OR IGNORE messages SET conv_id = ?2 WHERE conv_id = ?1",
            params![old_id, new_id],
        )?;
        // Anything still left under old_id conflicted with an existing
        // message id already under new_id; drop the leftovers rather than
        // leaving an orphaned, confusing conversation behind.
        self.conn
            .execute("DELETE FROM messages WHERE conv_id = ?1", params![old_id])?;
        info!("Rekeyed conversation {} -> {}", old_id, new_id);
        Ok(())
    }

    /// Distinct conversation ids currently stored, most-recently-active
    /// first. Used by `/select <prefix>` to resolve a prefix to a full id
    /// now that conversations live only in the DB, not an in-memory map.
    pub fn get_conversation_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT conv_id FROM messages GROUP BY conv_id ORDER BY MAX(timestamp) DESC",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut ids = Vec::new();
        for r in rows {
            ids.push(r?);
        }
        Ok(ids)
    }

    pub fn get_messages_for_conversation(&self, conv_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, status FROM messages WHERE conv_id = ?1 ORDER BY seq ASC"
        )?;
        
        let msg_iter = stmt.query_map(params![conv_id], |row| {
            let id: String = row.get(0)?;
            let role: String = row.get(1)?;
            let content: String = row.get(2)?;
            let status_str: String = row.get(3)?;
            
            let status = if status_str == "streaming" {
                MessageStatus::Streaming
            } else {
                MessageStatus::Complete
            };
            
            Ok(Message {
                id,
                role,
                content,
                status,
            })
        })?;
        
        let mut messages: Vec<Message> = Vec::new();
        for msg in msg_iter {
            if let Ok(m) = msg {
                messages.push(m);
            }
        }
        
        Ok(messages)
    }

    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute("DELETE FROM messages", [])?;
        info!("All messages deleted from database");
        Ok(())
    }
}