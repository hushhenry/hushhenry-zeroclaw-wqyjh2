use anyhow::{Context, Result};
use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::fmt;
use std::fs;
use std::path::Path;

use super::SessionKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionMessageRole {
    User,
    Assistant,
    Tool,
}

impl SessionMessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    pub fn from_str(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub meta_json: Option<String>,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

impl SessionStore {
    pub fn new(workspace_dir: &Path) -> Result<Self> {
        let db_dir = workspace_dir.join("memory");
        fs::create_dir_all(&db_dir).with_context(|| {
            format!(
                "Failed to create session db directory: {}",
                db_dir.display()
            )
        })?;

        let db_path = db_dir.join("sessions.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open sessions DB: {}", db_path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA mmap_size = 8388608;
             PRAGMA cache_size = -2000;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;",
        )
        .context("Failed to configure sessions DB pragmas")?;

        Self::init_schema(&conn)?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        Ok(store)
    }

    fn now() -> String {
        Utc::now().to_rfc3339()
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_index (
                session_key TEXT PRIMARY KEY,
                active_session_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                meta_json TEXT
             );

             CREATE TABLE IF NOT EXISTS session_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                meta_json TEXT
             );

             CREATE TABLE IF NOT EXISTS session_state (
                session_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (session_id, key)
             );

             CREATE INDEX IF NOT EXISTS idx_sessions_session_key ON sessions(session_key);
             CREATE INDEX IF NOT EXISTS idx_session_messages_session_id ON session_messages(session_id);
             CREATE INDEX IF NOT EXISTS idx_session_state_session_id ON session_state(session_id);",
        )
        .context("Failed to initialize sessions DB schema")?;
        Ok(())
    }

    pub fn get_or_create_active(&self, session_key: &SessionKey) -> Result<SessionId> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .context("Failed to start session transaction")?;

        if let Some(existing) = tx
            .query_row(
                "SELECT active_session_id FROM session_index WHERE session_key = ?1",
                params![session_key.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Failed to query active session")?
        {
            tx.commit()
                .context("Failed to commit read-only session transaction")?;
            return Ok(SessionId(existing));
        }

        let session_id = SessionId::new();
        let now = Self::now();

        tx.execute(
            "INSERT INTO sessions (session_id, session_key, status, title, created_at, updated_at, meta_json)
             VALUES (?1, ?2, 'active', NULL, ?3, ?3, NULL)",
            params![session_id.as_str(), session_key.as_str(), now],
        )
        .context("Failed to insert new session")?;

        tx.execute(
            "INSERT INTO session_index (session_key, active_session_id, updated_at)
             VALUES (?1, ?2, ?3)",
            params![session_key.as_str(), session_id.as_str(), now],
        )
        .context("Failed to insert session index")?;

        tx.commit()
            .context("Failed to commit get_or_create_active transaction")?;
        Ok(session_id)
    }

    pub fn create_new(&self, session_key: &SessionKey) -> Result<SessionId> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .context("Failed to start session transaction")?;
        let now = Self::now();

        let previous_active = tx
            .query_row(
                "SELECT active_session_id FROM session_index WHERE session_key = ?1",
                params![session_key.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Failed to query previous active session")?;

        if let Some(previous_id) = previous_active {
            tx.execute(
                "UPDATE sessions SET status = 'inactive', updated_at = ?1 WHERE session_id = ?2",
                params![now, previous_id],
            )
            .context("Failed to mark previous session inactive")?;
        }

        let session_id = SessionId::new();

        tx.execute(
            "INSERT INTO sessions (session_id, session_key, status, title, created_at, updated_at, meta_json)
             VALUES (?1, ?2, 'active', NULL, ?3, ?3, NULL)",
            params![session_id.as_str(), session_key.as_str(), now],
        )
        .context("Failed to insert new active session")?;

        tx.execute(
            "INSERT INTO session_index (session_key, active_session_id, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_key) DO UPDATE
             SET active_session_id = excluded.active_session_id,
                 updated_at = excluded.updated_at",
            params![session_key.as_str(), session_id.as_str(), now],
        )
        .context("Failed to update session index")?;

        tx.commit()
            .context("Failed to commit create_new transaction")?;
        Ok(session_id)
    }

    pub fn append_message(
        &self,
        session_id: &SessionId,
        role: &str,
        content: &str,
        meta_json: Option<&str>,
    ) -> Result<()> {
        let Some(role) = SessionMessageRole::from_str(role) else {
            tracing::warn!(
                session_id = %session_id.as_str(),
                role,
                "Skipping session message with unsupported role"
            );
            return Ok(());
        };

        let conn = self.conn.lock();
        let now = Self::now();

        conn.execute(
            "INSERT INTO session_messages (session_id, role, content, created_at, meta_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.as_str(), role.as_str(), content, now, meta_json],
        )
        .context("Failed to append session message")?;

        conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
            params![now, session_id.as_str()],
        )
        .context("Failed to update session updated_at")?;

        Ok(())
    }

    pub fn load_recent_messages(
        &self,
        session_id: &SessionId,
        limit: u32,
    ) -> Result<Vec<SessionMessage>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, created_at, meta_json
                 FROM session_messages
                 WHERE session_id = ?1
                   AND role IN ('user', 'assistant', 'tool')
                 ORDER BY id DESC
                 LIMIT ?2",
            )
            .context("Failed to prepare load_recent_messages query")?;

        let rows = stmt
            .query_map(params![session_id.as_str(), i64::from(limit)], |row| {
                Ok(SessionMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                    meta_json: row.get(4)?,
                })
            })
            .context("Failed to query recent session messages")?;

        let mut messages: Vec<SessionMessage> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode session messages")?;

        messages.reverse();
        Ok(messages)
    }

    pub fn load_messages_after_id(
        &self,
        session_id: &SessionId,
        after_message_id: Option<i64>,
    ) -> Result<Vec<SessionMessage>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, created_at, meta_json
                 FROM session_messages
                 WHERE session_id = ?1
                   AND role IN ('user', 'assistant', 'tool')
                   AND (?2 IS NULL OR id > ?2)
                 ORDER BY id ASC",
            )
            .context("Failed to prepare load_messages_after_id query")?;

        let rows = stmt
            .query_map(params![session_id.as_str(), after_message_id], |row| {
                Ok(SessionMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                    meta_json: row.get(4)?,
                })
            })
            .context("Failed to query session messages after boundary")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode boundary-filtered session messages")
    }

    pub fn get_state_key(&self, session_id: &SessionId, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let value = conn
            .query_row(
                "SELECT value_json FROM session_state WHERE session_id = ?1 AND key = ?2",
                params![session_id.as_str(), key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Failed to query session state")?;
        Ok(value)
    }

    pub fn set_state_key(&self, session_id: &SessionId, key: &str, value_json: &str) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "INSERT INTO session_state (session_id, key, value_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, key) DO UPDATE
             SET value_json = excluded.value_json,
                 updated_at = excluded.updated_at",
            params![session_id.as_str(), key, value_json, now],
        )
        .context("Failed to set session state")?;
        Ok(())
    }

    pub fn get_state(&self, session_id: &SessionId, key: &str) -> Result<Option<String>> {
        self.get_state_key(session_id, key)
    }

    pub fn set_state(&self, session_id: &SessionId, key: &str, value_json: &str) -> Result<()> {
        self.set_state_key(session_id, key, value_json)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use crate::session::SessionKey;
    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    #[test]
    fn session_store_create_append_and_load_recent() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let session_key = SessionKey::new("group:telegram:chat-123");
        let session_id = store.get_or_create_active(&session_key).unwrap();

        store
            .append_message(&session_id, "user", "hello", Some(r#"{"source":"test"}"#))
            .unwrap();
        store
            .append_message(&session_id, "assistant", "hi there", None)
            .unwrap();

        let messages = store.load_recent_messages(&session_id, 10).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].id > 0);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "hi there");

        let newer_session = store.create_new(&session_key).unwrap();
        assert_ne!(newer_session.as_str(), session_id.as_str());

        let active = store.get_or_create_active(&session_key).unwrap();
        assert_eq!(active.as_str(), newer_session.as_str());
    }

    #[test]
    fn session_store_skips_unsupported_roles() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let session_key = SessionKey::new("group:telegram:chat-123");
        let session_id = store.get_or_create_active(&session_key).unwrap();

        store
            .append_message(&session_id, "system", "internal marker", None)
            .unwrap();

        let messages = store.load_recent_messages(&session_id, 10).unwrap();
        assert!(messages.is_empty());

        let db_path = workspace.path().join("memory").join("sessions.db");
        let conn = Connection::open(db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1",
                params![session_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn session_store_state_and_after_boundary_loading() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_key = SessionKey::new("group:telegram:chat-compact");
        let session_id = store.get_or_create_active(&session_key).unwrap();

        store
            .append_message(&session_id, "user", "old-user", None)
            .unwrap();
        store
            .append_message(&session_id, "assistant", "old-assistant", None)
            .unwrap();
        store
            .append_message(&session_id, "user", "new-user", None)
            .unwrap();

        let all_messages = store.load_messages_after_id(&session_id, None).unwrap();
        assert_eq!(all_messages.len(), 3);
        let boundary = all_messages[1].id;

        let after_boundary = store
            .load_messages_after_id(&session_id, Some(boundary))
            .unwrap();
        assert_eq!(after_boundary.len(), 1);
        assert_eq!(after_boundary[0].content, "new-user");

        store
            .set_state_key(&session_id, "compaction_summary", "\"summary-v1\"")
            .unwrap();
        let state = store
            .get_state_key(&session_id, "compaction_summary")
            .unwrap()
            .unwrap();
        assert_eq!(state, "\"summary-v1\"");
    }
}
