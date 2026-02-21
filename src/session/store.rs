use anyhow::{bail, Context, Result};
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

    pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
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

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub session_key: String,
    pub status: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

#[derive(Debug, Clone)]
pub struct SessionRouteMetadata {
    pub agent_id: Option<String>,
    pub channel: String,
    pub account_id: Option<String>,
    pub chat_type: String,
    pub chat_id: String,
    pub route_id: Option<String>,
    pub sender_id: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionChatCandidate {
    pub chat_id: String,
    pub channel: String,
    pub account_id: Option<String>,
    pub chat_type: String,
    pub last_seen: String,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

const SESSION_SCHEMA_VERSION: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    TimedOut,
}

impl ExecRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn from_str(status: &str) -> Option<Self> {
        match status {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentSession {
    pub subagent_session_id: String,
    pub spec_id: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub meta_json: Option<String>,
}

/// Multi-agent profile: model defaults + policies (tools/skills/context).
/// Stored in sessions.db for per-session agent switching.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub agent_id: String,
    pub name: String,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub spec_id: String,
    pub name: String,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ExecRun {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub command: String,
    pub pty: bool,
    pub timeout_secs: i64,
    pub max_output_bytes: i64,
    pub watch_json: Option<String>,
    pub exit_code: Option<i64>,
    pub output_bytes: i64,
    pub truncated: bool,
    pub error_message: Option<String>,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ExecRunItem {
    pub seq: i64,
    pub run_id: String,
    pub item_type: String,
    pub payload: String,
    pub meta_json: Option<String>,
    pub created_at: String,
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
        Self::run_migrations(&conn)?;

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

    fn run_migrations(conn: &Connection) -> Result<()> {
        let mut version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("Failed to query sessions schema version")?;

        if version < 1 {
            conn.pragma_update(None, "user_version", 1_i64)
                .context("Failed to set sessions schema version to 1")?;
            version = 1;
        }

        if version < 2 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_meta (
                    session_id TEXT PRIMARY KEY,
                    agent_id TEXT,
                    channel TEXT NOT NULL,
                    account_id TEXT,
                    chat_type TEXT NOT NULL,
                    chat_id TEXT NOT NULL,
                    route_id TEXT,
                    sender_id TEXT NOT NULL,
                    title TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL,
                    FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS idx_session_meta_chat_route
                    ON session_meta(chat_id, channel, account_id, chat_type);
                 CREATE INDEX IF NOT EXISTS idx_session_meta_last_seen
                    ON session_meta(last_seen_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_session_meta_title_nocase
                    ON session_meta(title COLLATE NOCASE);",
            )
            .context("Failed to apply sessions schema migration v2")?;
            conn.pragma_update(None, "user_version", 2_i64)
                .context("Failed to set sessions schema version to 2")?;
            version = 2;
        }

        if version < 3 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS subagent_specs (
                    spec_id TEXT PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    config_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS subagent_sessions (
                    subagent_session_id TEXT PRIMARY KEY,
                    spec_id TEXT,
                    status TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    meta_json TEXT,
                    FOREIGN KEY(spec_id) REFERENCES subagent_specs(spec_id) ON DELETE SET NULL
                 );
                 CREATE TABLE IF NOT EXISTS subagent_runs (
                    run_id TEXT PRIMARY KEY,
                    subagent_session_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    input_json TEXT,
                    output_json TEXT,
                    error_message TEXT,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    updated_at TEXT NOT NULL,
                    FOREIGN KEY(subagent_session_id) REFERENCES subagent_sessions(subagent_session_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS idx_subagent_sessions_status
                    ON subagent_sessions(status, updated_at DESC);
                 CREATE INDEX IF NOT EXISTS idx_subagent_runs_status_queued
                    ON subagent_runs(status, queued_at ASC);
                 CREATE INDEX IF NOT EXISTS idx_subagent_runs_session
                    ON subagent_runs(subagent_session_id, queued_at ASC);",
            )
            .context("Failed to apply sessions schema migration v3")?;
            conn.pragma_update(None, "user_version", 3_i64)
                .context("Failed to set sessions schema version to 3")?;
            version = 3;
        }

        if version < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS exec_runs (
                    run_id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    command TEXT NOT NULL,
                    pty INTEGER NOT NULL DEFAULT 0,
                    timeout_secs INTEGER NOT NULL,
                    max_output_bytes INTEGER NOT NULL,
                    watch_json TEXT,
                    exit_code INTEGER,
                    output_bytes INTEGER NOT NULL DEFAULT 0,
                    truncated INTEGER NOT NULL DEFAULT 0,
                    error_message TEXT,
                    queued_at TEXT NOT NULL,
                    started_at TEXT,
                    finished_at TEXT,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS exec_run_items (
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL,
                    item_type TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    meta_json TEXT,
                    created_at TEXT NOT NULL,
                    FOREIGN KEY(run_id) REFERENCES exec_runs(run_id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS idx_exec_runs_status_queued
                    ON exec_runs(status, queued_at ASC);
                 CREATE INDEX IF NOT EXISTS idx_exec_runs_session
                    ON exec_runs(session_id, queued_at ASC);
                 CREATE INDEX IF NOT EXISTS idx_exec_run_items_run_seq
                    ON exec_run_items(run_id, seq ASC);",
            )
            .context("Failed to apply sessions schema migration v4")?;
            conn.pragma_update(None, "user_version", 4_i64)
                .context("Failed to set sessions schema version to 4")?;
            version = 4;
        }

        if version < 5 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS agent_specs (
                    agent_id TEXT PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    config_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_agent_specs_name
                    ON agent_specs(name);",
            )
            .context("Failed to apply sessions schema migration v5")?;
            conn.pragma_update(None, "user_version", 5_i64)
                .context("Failed to set sessions schema version to 5")?;
            version = 5;
        }

        if version < 6 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS announce_idempotency (
                    idempotency_key TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL
                 );",
            )
            .context("Failed to apply sessions schema migration v6")?;
            conn.pragma_update(None, "user_version", 6_i64)
                .context("Failed to set sessions schema version to 6")?;
            version = 6;
        }

        if version < 7 {
            conn.execute_batch("DROP TABLE IF EXISTS subagent_runs;")
                .context("Failed to apply sessions schema migration v7")?;
            conn.pragma_update(None, "user_version", 7_i64)
                .context("Failed to set sessions schema version to 7")?;
            version = 7;
        }

        if version != SESSION_SCHEMA_VERSION {
            bail!(
                "Unsupported sessions schema version {}, expected {}",
                version,
                SESSION_SCHEMA_VERSION
            );
        }

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

    pub fn upsert_route_metadata(
        &self,
        session_id: &SessionId,
        metadata: &SessionRouteMetadata,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "INSERT INTO session_meta (
                session_id, agent_id, channel, account_id, chat_type, chat_id, route_id,
                sender_id, title, created_at, updated_at, last_seen_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                channel = excluded.channel,
                account_id = excluded.account_id,
                chat_type = excluded.chat_type,
                chat_id = excluded.chat_id,
                route_id = excluded.route_id,
                sender_id = excluded.sender_id,
                title = excluded.title,
                updated_at = excluded.updated_at,
                last_seen_at = excluded.last_seen_at",
            params![
                session_id.as_str(),
                metadata.agent_id.as_deref(),
                metadata.channel.as_str(),
                metadata.account_id.as_deref(),
                metadata.chat_type.as_str(),
                metadata.chat_id.as_str(),
                metadata.route_id.as_deref(),
                metadata.sender_id.as_str(),
                metadata.title.as_deref(),
                now,
            ],
        )
        .context("Failed to upsert session route metadata")?;
        Ok(())
    }

    pub fn load_route_metadata(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionRouteMetadata>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id, channel, account_id, chat_type, chat_id, route_id, sender_id, title
             FROM session_meta
             WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| {
                Ok(SessionRouteMetadata {
                    agent_id: row.get(0)?,
                    channel: row.get(1)?,
                    account_id: row.get(2)?,
                    chat_type: row.get(3)?,
                    chat_id: row.get(4)?,
                    route_id: row.get(5)?,
                    sender_id: row.get(6)?,
                    title: row.get(7)?,
                })
            },
        )
        .optional()
        .context("Failed to query session route metadata")
    }

    /// Appends a message and returns the inserted row id for compaction boundary tracking.
    pub fn append_message(
        &self,
        session_id: &SessionId,
        role: &str,
        content: &str,
        meta_json: Option<&str>,
    ) -> Result<i64> {
        let Some(role) = SessionMessageRole::from_str(role) else {
            tracing::warn!(
                session_id = %session_id.as_str(),
                role,
                "Skipping session message with unsupported role"
            );
            return Ok(0);
        };

        let conn = self.conn.lock();
        let now = Self::now();

        conn.execute(
            "INSERT INTO session_messages (session_id, role, content, created_at, meta_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id.as_str(), role.as_str(), content, now, meta_json],
        )
        .context("Failed to append session message")?;

        let row_id = conn.last_insert_rowid();

        conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
            params![now, session_id.as_str()],
        )
        .context("Failed to update session updated_at")?;

        Ok(row_id)
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

    pub fn list_sessions(
        &self,
        session_key: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SessionSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT s.session_id, s.session_key, s.status, s.title, s.created_at, s.updated_at,
                        COUNT(m.id) AS message_count
                 FROM sessions s
                 LEFT JOIN session_messages m ON m.session_id = s.session_id
                 WHERE (?1 IS NULL OR s.session_key = ?1)
                 GROUP BY s.session_id, s.session_key, s.status, s.title, s.created_at, s.updated_at
                 ORDER BY s.updated_at DESC
                 LIMIT ?2",
            )
            .context("Failed to prepare list_sessions query")?;

        let rows = stmt
            .query_map(params![session_key, i64::from(limit)], |row| {
                Ok(SessionSummary {
                    session_id: row.get(0)?,
                    session_key: row.get(1)?,
                    status: row.get(2)?,
                    title: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    message_count: row.get(6)?,
                })
            })
            .context("Failed to query sessions list")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode sessions list")
    }

    pub fn session_exists(&self, session_id: &SessionId) -> Result<bool> {
        let conn = self.conn.lock();
        let exists = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
                params![session_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .context("Failed to query session existence")?;
        Ok(exists == 1)
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

    /// Session state key for the active multi-agent profile (agent_specs.agent_id).
    pub const ACTIVE_AGENT_ID_KEY: &'static str = "active_agent_id";
    /// Session state key for model override (e.g. "openrouter/anthropic/claude-sonnet-4").
    pub const MODEL_OVERRIDE_KEY: &'static str = "model_override";

    pub fn list_agent_specs(&self, limit: u32) -> Result<Vec<AgentSpec>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, name, config_json, created_at, updated_at
                 FROM agent_specs
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )
            .context("Failed to prepare list_agent_specs query")?;
        let rows = stmt
            .query_map(params![i64::from(limit)], |row| {
                Ok(AgentSpec {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .context("Failed to query agent specs list")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode agent specs list")
    }

    pub fn get_agent_spec_by_id(&self, agent_id: &str) -> Result<Option<AgentSpec>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id, name, config_json, created_at, updated_at
             FROM agent_specs
             WHERE agent_id = ?1",
            params![agent_id],
            |row| {
                Ok(AgentSpec {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query agent spec by id")
    }

    pub fn get_agent_spec_by_name(&self, name: &str) -> Result<Option<AgentSpec>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id, name, config_json, created_at, updated_at
             FROM agent_specs
             WHERE name = ?1",
            params![name],
            |row| {
                Ok(AgentSpec {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query agent spec by name")
    }

    pub fn upsert_agent_spec(&self, name: &str, config_json: &str) -> Result<AgentSpec> {
        let conn = self.conn.lock();
        let now = Self::now();
        let agent_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO agent_specs (agent_id, name, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name) DO UPDATE SET
                config_json = excluded.config_json,
                updated_at = excluded.updated_at",
            params![agent_id, name, config_json, now],
        )
        .context("Failed to upsert agent spec")?;

        let resolved_id = conn
            .query_row(
                "SELECT agent_id FROM agent_specs WHERE name = ?1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .context("Failed to read back agent_id after upsert")?;

        drop(conn);
        self.get_agent_spec_by_id(&resolved_id)?
            .ok_or_else(|| anyhow::anyhow!("Agent spec missing after upsert for name '{name}'"))
    }

    pub fn find_chat_candidates_by_title(
        &self,
        title_substring: &str,
        limit: u32,
    ) -> Result<Vec<SessionChatCandidate>> {
        let title_substring = title_substring.trim();
        if title_substring.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT chat_id, channel, account_id, chat_type, MAX(last_seen_at) AS last_seen
                 FROM session_meta
                 WHERE title IS NOT NULL
                   AND title <> ''
                   AND title LIKE '%' || ?1 || '%' COLLATE NOCASE
                 GROUP BY chat_id, channel, account_id, chat_type
                 ORDER BY last_seen DESC
                 LIMIT ?2",
            )
            .context("Failed to prepare find_chat_candidates_by_title query")?;

        let rows = stmt
            .query_map(params![title_substring, i64::from(limit)], |row| {
                Ok(SessionChatCandidate {
                    chat_id: row.get(0)?,
                    channel: row.get(1)?,
                    account_id: row.get(2)?,
                    chat_type: row.get(3)?,
                    last_seen: row.get(4)?,
                })
            })
            .context("Failed to query session chat candidates by title")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode title-based session chat candidates")
    }

    pub fn create_subagent_session(
        &self,
        spec_id: Option<&str>,
        meta_json: Option<&str>,
    ) -> Result<SubagentSession> {
        let conn = self.conn.lock();
        let now = Self::now();
        let subagent_session_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO subagent_sessions (
                subagent_session_id, spec_id, status, created_at, updated_at, meta_json
             ) VALUES (?1, ?2, 'active', ?3, ?3, ?4)",
            params![subagent_session_id, spec_id, now, meta_json],
        )
        .context("Failed to create subagent session")?;

        Ok(SubagentSession {
            subagent_session_id,
            spec_id: spec_id.map(ToOwned::to_owned),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            meta_json: meta_json.map(ToOwned::to_owned),
        })
    }

    pub fn upsert_subagent_spec(&self, name: &str, config_json: &str) -> Result<SubagentSpec> {
        let conn = self.conn.lock();
        let now = Self::now();
        let spec_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO subagent_specs (spec_id, name, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name) DO UPDATE SET
                config_json = excluded.config_json,
                updated_at = excluded.updated_at",
            params![spec_id, name, config_json, now],
        )
        .context("Failed to upsert subagent spec")?;

        conn.query_row(
            "SELECT spec_id, name, config_json, created_at, updated_at
             FROM subagent_specs
             WHERE name = ?1",
            params![name],
            |row| {
                Ok(SubagentSpec {
                    spec_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to load upserted subagent spec")?
        .ok_or_else(|| anyhow::anyhow!("Upserted subagent spec missing for name '{name}'"))
    }

    pub fn get_subagent_spec_by_name(&self, name: &str) -> Result<Option<SubagentSpec>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT spec_id, name, config_json, created_at, updated_at
             FROM subagent_specs
             WHERE name = ?1",
            params![name],
            |row| {
                Ok(SubagentSpec {
                    spec_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query subagent spec by name")
    }

    pub fn list_subagent_specs(&self, limit: u32) -> Result<Vec<SubagentSpec>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT spec_id, name, config_json, created_at, updated_at
                 FROM subagent_specs
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )
            .context("Failed to prepare list_subagent_specs query")?;
        let rows = stmt
            .query_map(params![i64::from(limit)], |row| {
                Ok(SubagentSpec {
                    spec_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .context("Failed to query subagent specs list")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode subagent specs list")
    }

    /// M5: Claim an idempotency key for announce. Returns true if key was inserted (first use), false if already present (duplicate).
    pub fn try_claim_announce_idempotency(&self, idempotency_key: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "INSERT OR IGNORE INTO announce_idempotency (idempotency_key, created_at) VALUES (?1, ?2)",
            params![idempotency_key, now],
        )
        .context("Failed to insert announce idempotency key")?;
        let changes = conn
            .query_row("SELECT changes()", [], |row| row.get::<_, i64>(0))
            .context("Failed to read changes after announce idempotency insert")?;
        Ok(changes == 1)
    }

    pub fn get_subagent_session(
        &self,
        subagent_session_id: &str,
    ) -> Result<Option<SubagentSession>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT subagent_session_id, spec_id, status, created_at, updated_at, meta_json
             FROM subagent_sessions
             WHERE subagent_session_id = ?1",
            params![subagent_session_id],
            |row| {
                Ok(SubagentSession {
                    subagent_session_id: row.get(0)?,
                    spec_id: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    meta_json: row.get(5)?,
                })
            },
        )
        .optional()
        .context("Failed to query subagent session")
    }

    pub fn list_subagent_sessions(&self, limit: u32) -> Result<Vec<SubagentSession>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT subagent_session_id, spec_id, status, created_at, updated_at, meta_json
                 FROM subagent_sessions
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )
            .context("Failed to prepare list_subagent_sessions query")?;
        let rows = stmt
            .query_map(params![i64::from(limit)], |row| {
                Ok(SubagentSession {
                    subagent_session_id: row.get(0)?,
                    spec_id: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    meta_json: row.get(5)?,
                })
            })
            .context("Failed to query subagent sessions list")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode subagent sessions list")
    }

    /// Mark a subagent session as stopped so the async loop can cease processing it.
    pub fn mark_subagent_session_stopped(&self, subagent_session_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "UPDATE subagent_sessions SET status = 'stopped', updated_at = ?1 WHERE subagent_session_id = ?2",
            params![now, subagent_session_id],
        )
        .context("Failed to mark subagent session stopped")?;
        Ok(())
    }

    pub fn get_subagent_spec_by_id(&self, spec_id: &str) -> Result<Option<SubagentSpec>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT spec_id, name, config_json, created_at, updated_at
             FROM subagent_specs
             WHERE spec_id = ?1",
            params![spec_id],
            |row| {
                Ok(SubagentSpec {
                    spec_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query subagent spec by id")
    }

    pub fn enqueue_exec_run(
        &self,
        session_id: &str,
        command: &str,
        pty: bool,
        timeout_secs: i64,
        max_output_bytes: i64,
        watch_json: Option<&str>,
    ) -> Result<ExecRun> {
        let conn = self.conn.lock();
        let now = Self::now();
        let run_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO exec_runs (
                run_id, session_id, status, command, pty, timeout_secs, max_output_bytes,
                watch_json, exit_code, output_bytes, truncated, error_message,
                queued_at, started_at, finished_at, updated_at
             ) VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, NULL, 0, 0, NULL, ?8, NULL, NULL, ?8)",
            params![
                run_id,
                session_id,
                command,
                if pty { 1_i64 } else { 0_i64 },
                timeout_secs,
                max_output_bytes,
                watch_json,
                now
            ],
        )
        .context("Failed to enqueue exec run")?;

        Ok(ExecRun {
            run_id,
            session_id: session_id.to_string(),
            status: ExecRunStatus::Queued.as_str().to_string(),
            command: command.to_string(),
            pty,
            timeout_secs,
            max_output_bytes,
            watch_json: watch_json.map(ToOwned::to_owned),
            exit_code: None,
            output_bytes: 0,
            truncated: false,
            error_message: None,
            queued_at: now.clone(),
            started_at: None,
            finished_at: None,
            updated_at: now,
        })
    }

    pub fn claim_next_queued_exec_run(&self) -> Result<Option<ExecRun>> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .context("Failed to start claim_next_queued_exec_run transaction")?;
        let next = tx
            .query_row(
                "SELECT run_id, session_id
                 FROM exec_runs
                 WHERE status = 'queued'
                   AND NOT EXISTS (
                        SELECT 1
                        FROM exec_runs AS running
                        WHERE running.session_id = exec_runs.session_id
                          AND running.status = 'running'
                   )
                 ORDER BY queued_at ASC
                 LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .context("Failed to query next queued exec run")?;

        let Some((run_id, session_id)) = next else {
            tx.commit()
                .context("Failed to commit empty queued exec-run transaction")?;
            return Ok(None);
        };

        let now = Self::now();
        tx.execute(
            "UPDATE exec_runs
             SET status = 'running', started_at = ?1, updated_at = ?1
             WHERE run_id = ?2
               AND status = 'queued'
               AND NOT EXISTS (
                    SELECT 1
                    FROM exec_runs AS running
                    WHERE running.session_id = ?3
                      AND running.status = 'running'
               )",
            params![now, run_id, session_id],
        )
        .context("Failed to mark exec run as running")?;

        tx.commit()
            .context("Failed to commit exec-run claim transaction")?;
        drop(conn);
        self.get_exec_run(run_id.as_str())
    }

    pub fn recover_running_exec_runs_to_queued(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "UPDATE exec_runs
             SET status = 'queued',
                 started_at = NULL,
                 updated_at = ?1
             WHERE status = 'running'",
            params![now],
        )
        .context("Failed to recover running exec runs")?;
        let changes = conn
            .query_row("SELECT changes()", [], |row| row.get::<_, i64>(0))
            .context("Failed to query recovered exec run changes")?;
        Ok(changes.max(0) as usize)
    }

    pub fn mark_exec_run_succeeded(
        &self,
        run_id: &str,
        exit_code: Option<i64>,
        output_bytes: i64,
        truncated: bool,
    ) -> Result<()> {
        self.mark_exec_run_final(
            run_id,
            ExecRunStatus::Succeeded,
            exit_code,
            output_bytes,
            truncated,
            None,
            &["running"],
            true,
        )
    }

    pub fn mark_exec_run_failed(
        &self,
        run_id: &str,
        exit_code: Option<i64>,
        output_bytes: i64,
        truncated: bool,
        error_message: &str,
    ) -> Result<()> {
        self.mark_exec_run_final(
            run_id,
            ExecRunStatus::Failed,
            exit_code,
            output_bytes,
            truncated,
            Some(error_message),
            &["running"],
            true,
        )
    }

    pub fn mark_exec_run_timed_out(
        &self,
        run_id: &str,
        output_bytes: i64,
        truncated: bool,
    ) -> Result<()> {
        self.mark_exec_run_final(
            run_id,
            ExecRunStatus::TimedOut,
            None,
            output_bytes,
            truncated,
            Some("exec run timed out"),
            &["running"],
            true,
        )
    }

    pub fn mark_exec_run_canceled(&self, run_id: &str) -> Result<()> {
        self.mark_exec_run_final(
            run_id,
            ExecRunStatus::Canceled,
            None,
            0,
            false,
            Some("exec run canceled"),
            &["queued", "running"],
            false,
        )
    }

    pub fn append_exec_run_item(
        &self,
        run_id: &str,
        item_type: &str,
        payload: &str,
        meta_json: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "INSERT INTO exec_run_items (run_id, item_type, payload, meta_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, item_type, payload, meta_json, now],
        )
        .context("Failed to append exec run item")?;

        Ok(conn.last_insert_rowid())
    }

    pub fn load_exec_run_items_since(
        &self,
        run_id: &str,
        since_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ExecRunItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT seq, run_id, item_type, payload, meta_json, created_at
                 FROM exec_run_items
                 WHERE run_id = ?1
                   AND (?2 IS NULL OR seq > ?2)
                 ORDER BY seq ASC
                 LIMIT ?3",
            )
            .context("Failed to prepare load_exec_run_items_since query")?;

        let rows = stmt
            .query_map(params![run_id, since_seq, i64::from(limit)], |row| {
                Ok(ExecRunItem {
                    seq: row.get(0)?,
                    run_id: row.get(1)?,
                    item_type: row.get(2)?,
                    payload: row.get(3)?,
                    meta_json: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .context("Failed to query exec run items")?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode exec run items")
    }

    pub fn get_exec_run(&self, run_id: &str) -> Result<Option<ExecRun>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT run_id, session_id, status, command, pty, timeout_secs, max_output_bytes, watch_json,
                    exit_code, output_bytes, truncated, error_message,
                    queued_at, started_at, finished_at, updated_at
             FROM exec_runs
             WHERE run_id = ?1",
            params![run_id],
            |row| {
                Ok(ExecRun {
                    run_id: row.get(0)?,
                    session_id: row.get(1)?,
                    status: row.get(2)?,
                    command: row.get(3)?,
                    pty: row.get::<_, i64>(4)? == 1,
                    timeout_secs: row.get(5)?,
                    max_output_bytes: row.get(6)?,
                    watch_json: row.get(7)?,
                    exit_code: row.get(8)?,
                    output_bytes: row.get(9)?,
                    truncated: row.get::<_, i64>(10)? == 1,
                    error_message: row.get(11)?,
                    queued_at: row.get(12)?,
                    started_at: row.get(13)?,
                    finished_at: row.get(14)?,
                    updated_at: row.get(15)?,
                })
            },
        )
        .optional()
        .context("Failed to query exec run")
    }

    fn mark_exec_run_final(
        &self,
        run_id: &str,
        status: ExecRunStatus,
        exit_code: Option<i64>,
        output_bytes: i64,
        truncated: bool,
        error_message: Option<&str>,
        allowed_current_statuses: &[&str],
        bail_when_unchanged: bool,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::now();
        let allowed_statuses = allowed_current_statuses
            .iter()
            .map(|value| format!("'{value}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "UPDATE exec_runs
                 SET status = ?1,
                     exit_code = COALESCE(?2, exit_code),
                     output_bytes = CASE
                        WHEN ?3 > output_bytes THEN ?3
                        ELSE output_bytes
                     END,
                     truncated = CASE
                        WHEN ?4 = 1 THEN 1
                        ELSE truncated
                     END,
                     error_message = ?5,
                     finished_at = ?6,
                     updated_at = ?6
                 WHERE run_id = ?7
                   AND status IN ({allowed_statuses})"
        );
        let changed = conn
            .execute(
                query.as_str(),
                params![
                    status.as_str(),
                    exit_code,
                    output_bytes,
                    if truncated { 1_i64 } else { 0_i64 },
                    error_message,
                    now,
                    run_id
                ],
            )
            .context("Failed to mark exec run final state")?;
        if changed == 0 && bail_when_unchanged {
            bail!("Exec run '{run_id}' must be in running state before finalizing");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecRunStatus, SessionId, SessionRouteMetadata, SessionStore};
    use crate::session::SessionKey;
    use rusqlite::{params, Connection};
    use std::fs;
    use std::time::Duration;
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

    #[test]
    fn session_store_lists_sessions_and_checks_existence() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_key = SessionKey::new("group:telegram:list-check");
        let session_id = store.get_or_create_active(&session_key).unwrap();
        store
            .append_message(&session_id, "user", "list-message", None)
            .unwrap();

        let sessions = store.list_sessions(Some(session_key.as_str()), 10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id.as_str());
        assert_eq!(sessions[0].message_count, 1);

        assert!(store.session_exists(&session_id).unwrap());
        assert!(!store
            .session_exists(&SessionId::from_string("missing-session-id"))
            .unwrap());
    }

    #[test]
    fn session_store_migrates_legacy_database_with_meta_table() {
        let workspace = TempDir::new().unwrap();
        let db_dir = workspace.path().join("memory");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("sessions.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE session_index (
                session_key TEXT PRIMARY KEY,
                active_session_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );
             CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                meta_json TEXT
             );
             CREATE TABLE session_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                meta_json TEXT
             );
             CREATE TABLE session_state (
                session_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (session_id, key)
             );",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 0_i64).unwrap();

        let store = SessionStore::new(workspace.path()).unwrap();
        drop(store);

        let migrated = Connection::open(db_path).unwrap();
        let version: i64 = migrated
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, super::SESSION_SCHEMA_VERSION);

        let table_exists: i64 = migrated
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 1);

        // subagent_runs table was dropped in migration v7 (run-based subagent removed).
        let subagent_runs_exists: i64 = migrated
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='subagent_runs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(subagent_runs_exists, 0);

        let exec_runs_exists: i64 = migrated
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='exec_runs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exec_runs_exists, 1);

        let agent_specs_exists: i64 = migrated
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_specs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_specs_exists, 1);

        let announce_idempotency_exists: i64 = migrated
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='announce_idempotency')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(announce_idempotency_exists, 1);
    }

    #[test]
    fn agent_spec_upsert_list_get_by_id_and_name() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let spec = store
            .upsert_agent_spec(
                "coder",
                r#"{"defaults":{"provider":"openrouter","model":"anthropic/claude-sonnet-4","temperature":0.2}}"#,
            )
            .unwrap();
        assert_eq!(spec.name, "coder");
        assert!(!spec.agent_id.is_empty());

        let by_id = store.get_agent_spec_by_id(&spec.agent_id).unwrap().unwrap();
        assert_eq!(by_id.name, "coder");
        let by_name = store.get_agent_spec_by_name("coder").unwrap().unwrap();
        assert_eq!(by_name.agent_id, spec.agent_id);

        let updated = store
            .upsert_agent_spec("coder", r#"{"defaults":{"model":"gpt-4"}}"#)
            .unwrap();
        assert_eq!(updated.agent_id, spec.agent_id);
        assert_eq!(updated.config_json, r#"{"defaults":{"model":"gpt-4"}}"#);

        let list = store.list_agent_specs(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "coder");
    }

    #[test]
    fn get_agent_spec_by_id_returns_none_for_unknown_id() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let spec = store.get_agent_spec_by_id("unknown-agent-id").unwrap();
        assert!(spec.is_none());
    }

    #[test]
    fn list_agent_specs_returns_empty_when_no_specs() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let list = store.list_agent_specs(10).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn session_state_keys_active_agent_id_and_model_override_persist() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session_key = SessionKey::new("group:test:state-keys");
        let session_id = store.get_or_create_active(&session_key).unwrap();

        store
            .set_state_key(
                &session_id,
                SessionStore::ACTIVE_AGENT_ID_KEY,
                "\"agent-abc\"",
            )
            .unwrap();
        store
            .set_state_key(
                &session_id,
                SessionStore::MODEL_OVERRIDE_KEY,
                "\"openrouter/anthropic/claude-sonnet-4\"",
            )
            .unwrap();

        let active = store
            .get_state_key(&session_id, SessionStore::ACTIVE_AGENT_ID_KEY)
            .unwrap();
        assert_eq!(active.as_deref(), Some("\"agent-abc\""));

        let model = store
            .get_state_key(&session_id, SessionStore::MODEL_OVERRIDE_KEY)
            .unwrap();
        assert_eq!(
            model.as_deref(),
            Some("\"openrouter/anthropic/claude-sonnet-4\"")
        );
    }

    #[test]
    fn announce_idempotency_first_claim_succeeds_second_is_duplicate() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let key = "announce:run-123";
        assert!(store.try_claim_announce_idempotency(key).unwrap());
        assert!(!store.try_claim_announce_idempotency(key).unwrap());
        assert!(!store.try_claim_announce_idempotency(key).unwrap());
        assert!(store
            .try_claim_announce_idempotency("announce:run-456")
            .unwrap());
    }

    #[test]
    fn subagent_store_upserts_specs_and_transitions_run_state() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let spec = store
            .upsert_subagent_spec(
                "reviewer",
                r#"{"provider":"openrouter","model":"anthropic/claude-sonnet-4"}"#,
            )
            .unwrap();
        assert_eq!(spec.name, "reviewer");

        let spec_updated = store
            .upsert_subagent_spec("reviewer", r#"{"provider":"openrouter","model":"gpt-5"}"#)
            .unwrap();
        assert_eq!(spec.spec_id, spec_updated.spec_id);
        assert_eq!(
            spec_updated.config_json,
            r#"{"provider":"openrouter","model":"gpt-5"}"#
        );

        let subagent_session = store
            .create_subagent_session(Some(spec.spec_id.as_str()), None)
            .unwrap();
        assert!(!subagent_session.subagent_session_id.is_empty());
        assert_eq!(
            subagent_session.spec_id.as_deref(),
            Some(spec.spec_id.as_str())
        );
        assert_eq!(subagent_session.status, "active");

        let looked_up = store
            .get_subagent_session(subagent_session.subagent_session_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(
            looked_up.subagent_session_id,
            subagent_session.subagent_session_id
        );
    }

    #[test]
    fn session_store_finds_chat_candidates_by_title_case_insensitive() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let key_a = SessionKey::new("group:telegram:chat-alpha");
        let session_a = store.get_or_create_active(&key_a).unwrap();
        store
            .upsert_route_metadata(
                &session_a,
                &SessionRouteMetadata {
                    agent_id: Some("zeroclaw-bot".into()),
                    channel: "telegram".into(),
                    account_id: Some("acc-main".into()),
                    chat_type: "group".into(),
                    chat_id: "chat-alpha".into(),
                    route_id: Some("thread-1".into()),
                    sender_id: "user-a".into(),
                    title: Some("Engineering Group".into()),
                },
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));

        let key_b = SessionKey::new("group:slack:chat-beta");
        let session_b = store.get_or_create_active(&key_b).unwrap();
        store
            .upsert_route_metadata(
                &session_b,
                &SessionRouteMetadata {
                    agent_id: None,
                    channel: "slack".into(),
                    account_id: Some("acc-main".into()),
                    chat_type: "group".into(),
                    chat_id: "chat-beta".into(),
                    route_id: None,
                    sender_id: "user-b".into(),
                    title: Some("operations group".into()),
                },
            )
            .unwrap();

        let candidates = store.find_chat_candidates_by_title("GROUP", 10).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].chat_id, "chat-beta");
        assert_eq!(candidates[0].channel, "slack");
        assert_eq!(candidates[0].account_id.as_deref(), Some("acc-main"));
        assert_eq!(candidates[0].chat_type, "group");
        assert!(!candidates[0].last_seen.is_empty());
        assert_eq!(candidates[1].chat_id, "chat-alpha");
    }

    #[test]
    fn session_store_loads_route_metadata_by_session_id() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let session = store
            .get_or_create_active(&SessionKey::new("group:slack:team-1"))
            .unwrap();
        store
            .upsert_route_metadata(
                &session,
                &SessionRouteMetadata {
                    agent_id: Some("zeroclaw-bot".into()),
                    channel: "slack".into(),
                    account_id: Some("acc-1".into()),
                    chat_type: "group".into(),
                    chat_id: "chat-1".into(),
                    route_id: Some("thread-1".into()),
                    sender_id: "user-1".into(),
                    title: Some("Ops".into()),
                },
            )
            .unwrap();

        let loaded = store.load_route_metadata(&session).unwrap().unwrap();
        assert_eq!(loaded.channel, "slack");
        assert_eq!(loaded.route_id.as_deref(), Some("thread-1"));
        assert_eq!(loaded.chat_id, "chat-1");
    }

    #[test]
    fn exec_store_enqueues_claims_streams_and_recovers() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let queued = store
            .enqueue_exec_run(
                "session-ops",
                "echo hello",
                false,
                30,
                2048,
                Some(r#"[{"regex":"hello","event":"ready"}]"#),
            )
            .unwrap();
        assert_eq!(queued.status, ExecRunStatus::Queued.as_str());

        let claimed = store.claim_next_queued_exec_run().unwrap().unwrap();
        assert_eq!(claimed.run_id, queued.run_id);
        assert_eq!(claimed.status, ExecRunStatus::Running.as_str());

        let seq = store
            .append_exec_run_item(claimed.run_id.as_str(), "stdout", "hello\n", None)
            .unwrap();
        assert!(seq > 0);
        let streamed = store
            .load_exec_run_items_since(claimed.run_id.as_str(), Some(seq - 1), 10)
            .unwrap();
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].payload, "hello\n");

        store
            .mark_exec_run_succeeded(claimed.run_id.as_str(), Some(0), 6, false)
            .unwrap();
        let completed = store
            .get_exec_run(claimed.run_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, ExecRunStatus::Succeeded.as_str());
        assert_eq!(completed.exit_code, Some(0));

        let second = store
            .enqueue_exec_run("session-ops", "sleep 1", false, 30, 1024, None)
            .unwrap();
        let second_claimed = store.claim_next_queued_exec_run().unwrap().unwrap();
        assert_eq!(second_claimed.run_id, second.run_id);
        let recovered = store.recover_running_exec_runs_to_queued().unwrap();
        assert_eq!(recovered, 1);
        let recovered_run = store.get_exec_run(second.run_id.as_str()).unwrap().unwrap();
        assert_eq!(recovered_run.status, ExecRunStatus::Queued.as_str());
        assert!(recovered_run.started_at.is_none());
    }
}
