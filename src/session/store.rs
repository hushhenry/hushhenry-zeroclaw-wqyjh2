use anyhow::{Context, Result};
use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::fmt;
use std::fs;
use std::path::Path;

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
    pub inbound_key: String,
    pub status: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

pub struct SessionStore {
    conn: Mutex<Connection>,
}

const SESSION_SCHEMA_VERSION: i64 = 12;

/// Subagent session: backed by sessions table (session_id, agent_id, status, outbound_key, ...).
#[derive(Debug, Clone)]
pub struct SubagentSession {
    pub subagent_session_id: String,
    pub agent_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub meta_json: Option<String>,
    pub outbound_key: Option<String>,
}

/// Agent (workspace or profile): agent_id, name, config. Stored in agents table.
/// Workspace agents use agent_id = name = directory name; main is not stored.
#[derive(Debug, Clone)]
pub struct Agent {
    pub agent_id: String,
    pub name: String,
    pub config_json: String,
    pub created_at: String,
    pub updated_at: String,
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
        let current_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("Failed to read user_version")?;
        if current_version == 11 {
            conn.execute(
                "INSERT OR IGNORE INTO agents (agent_id, name, config_json, created_at, updated_at)
                 SELECT agent_id, agent_id, config_json, created_at, updated_at FROM workspace_agents",
                [],
            )
            .context("Failed to migrate workspace_agents into agents")?;
            conn.execute("DROP TABLE IF EXISTS workspace_agents", [])
                .context("Failed to drop workspace_agents")?;
        }

        conn.execute_batch(
            "             CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                inbound_key TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT 'main',
                status TEXT NOT NULL,
                title TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                meta_json TEXT,
                outbound_key TEXT
             );

             CREATE TABLE IF NOT EXISTS session_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                meta_json TEXT,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
             );

             CREATE TABLE IF NOT EXISTS session_state (
                session_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (session_id, key),
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
             );

             CREATE TABLE IF NOT EXISTS agents (
                agent_id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                config_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_sessions_inbound_key ON sessions(inbound_key);
             CREATE INDEX IF NOT EXISTS idx_sessions_inbound_updated ON sessions(inbound_key, updated_at DESC);
             CREATE INDEX IF NOT EXISTS idx_session_messages_session_id ON session_messages(session_id);
             CREATE INDEX IF NOT EXISTS idx_session_state_session_id ON session_state(session_id);
             CREATE INDEX IF NOT EXISTS idx_agents_name ON agents(name);",
        )
        .context("Failed to initialize sessions DB schema")?;

        conn.pragma_update(None, "user_version", SESSION_SCHEMA_VERSION)
            .context("Failed to set sessions schema version")?;
        Ok(())
    }

    /// Resolves the active session for an inbound key: latest session by created_at.
    /// Creates a new session with agent_id = 'main' if none exists.
    pub fn get_or_create_active(&self, inbound_key: &str) -> Result<SessionId> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .context("Failed to start session transaction")?;

        let existing = tx
            .query_row(
                "SELECT session_id FROM sessions WHERE inbound_key = ?1 ORDER BY created_at DESC LIMIT 1",
                params![inbound_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("Failed to query latest session by inbound_key")?;

        if let Some(session_id) = existing {
            tx.commit()
                .context("Failed to commit read-only session transaction")?;
            return Ok(SessionId(session_id));
        }

        let session_id = SessionId::new();
        let now = Self::now();

        tx.execute(
            "INSERT INTO sessions (session_id, inbound_key, agent_id, status, title, created_at, updated_at, meta_json)
             VALUES (?1, ?2, 'main', 'active', NULL, ?3, ?3, NULL)",
            params![session_id.as_str(), inbound_key, now],
        )
        .context("Failed to insert new session")?;

        tx.commit()
            .context("Failed to commit get_or_create_active transaction")?;
        Ok(session_id)
    }

    /// Create a new session for the inbound key. If agent_id is None, uses "main".
    pub fn create_new(&self, inbound_key: &str, agent_id: Option<&str>) -> Result<SessionId> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction()
            .context("Failed to start session transaction")?;
        let now = Self::now();
        let agent_id_val = agent_id.unwrap_or("main").trim();
        let agent_id_val = if agent_id_val.is_empty() {
            "main"
        } else {
            agent_id_val
        };

        tx.execute(
            "UPDATE sessions SET status = 'inactive' WHERE inbound_key = ?1",
            params![inbound_key],
        )
        .context("Failed to mark previous sessions inactive")?;

        let session_id = SessionId::new();
        tx.execute(
            "INSERT INTO sessions (session_id, inbound_key, agent_id, status, title, created_at, updated_at, meta_json)
             VALUES (?1, ?2, ?3, 'active', NULL, ?4, ?4, NULL)",
            params![session_id.as_str(), inbound_key, agent_id_val, now],
        )
        .context("Failed to insert new active session")?;

        tx.commit()
            .context("Failed to commit create_new transaction")?;
        Ok(session_id)
    }

    /// Returns the agent_id stored for this session (from sessions table).
    pub fn get_agent_id_for_session(&self, session_id: &SessionId) -> Result<Option<String>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id FROM sessions WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("Failed to query agent_id for session")
    }

    /// Update the session's agent_id in the sessions table (used when switching agent).
    pub fn update_session_agent_id(&self, session_id: &SessionId, agent_id: &str) -> Result<()> {
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "UPDATE sessions SET agent_id = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![agent_id.trim(), now, session_id.as_str()],
        )
        .context("Failed to update session agent_id")?;
        Ok(())
    }

    /// Effective workspace agent for a session: state active_agent_id override, else session's agent_id, else "main".
    pub fn effective_workspace_agent_id(&self, session_id: &SessionId) -> Result<String> {
        if let Some(raw) = self.get_state_key(session_id, Self::ACTIVE_AGENT_ID_KEY)? {
            let parsed: Option<String> = serde_json::from_str(&raw).ok();
            if let Some(id) = parsed.filter(|s| !s.is_empty()) {
                return Ok(id);
            }
            if !raw.is_empty() && !raw.starts_with('"') {
                return Ok(raw);
            }
        }
        let from_row = self
            .get_agent_id_for_session(session_id)?
            .unwrap_or_else(|| "main".into());
        Ok(from_row)
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
        inbound_key_filter: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SessionSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT s.session_id, s.inbound_key, s.status, s.title, s.created_at, s.updated_at,
                        COUNT(m.id) AS message_count
                 FROM sessions s
                 LEFT JOIN session_messages m ON m.session_id = s.session_id
                 WHERE (?1 IS NULL OR s.inbound_key = ?1)
                 GROUP BY s.session_id, s.inbound_key, s.status, s.title, s.created_at, s.updated_at
                 ORDER BY s.updated_at DESC
                 LIMIT ?2",
            )
            .context("Failed to prepare list_sessions query")?;

        let rows = stmt
            .query_map(params![inbound_key_filter, i64::from(limit)], |row| {
                Ok(SessionSummary {
                    session_id: row.get(0)?,
                    inbound_key: row.get(1)?,
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

    /// Returns the id of the last message in the session (max id).
    pub fn last_message_id(&self, session_id: &SessionId) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        let id = conn
            .query_row(
                "SELECT id FROM session_messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                params![session_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("Failed to query last message id")?;
        Ok(id)
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

    /// Session state key for the active agent (agents.agent_id).
    pub const ACTIVE_AGENT_ID_KEY: &'static str = "active_agent_id";
    /// Session state key for model override (e.g. "openrouter/anthropic/claude-sonnet-4").
    pub const MODEL_OVERRIDE_KEY: &'static str = "model_override";
    /// Session state key for temperature override (f64 as JSON number).
    pub const TEMPERATURE_OVERRIDE_KEY: &'static str = "temperature_override";

    pub fn list_agents(&self, limit: u32) -> Result<Vec<Agent>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT agent_id, name, config_json, created_at, updated_at
                 FROM agents
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )
            .context("Failed to prepare list_agents query")?;
        let rows = stmt
            .query_map(params![i64::from(limit)], |row| {
                Ok(Agent {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })
            .context("Failed to query agents list")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode agents list")
    }

    pub fn get_agent_by_id(&self, agent_id: &str) -> Result<Option<Agent>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id, name, config_json, created_at, updated_at
             FROM agents
             WHERE agent_id = ?1",
            params![agent_id],
            |row| {
                Ok(Agent {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query agent by id")
    }

    pub fn get_agent_by_name(&self, name: &str) -> Result<Option<Agent>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT agent_id, name, config_json, created_at, updated_at
             FROM agents
             WHERE name = ?1",
            params![name],
            |row| {
                Ok(Agent {
                    agent_id: row.get(0)?,
                    name: row.get(1)?,
                    config_json: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()
        .context("Failed to query agent by name")
    }

    pub fn upsert_agent(&self, name: &str, config_json: &str) -> Result<Agent> {
        let conn = self.conn.lock();
        let now = Self::now();
        let agent_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO agents (agent_id, name, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name) DO UPDATE SET
                config_json = excluded.config_json,
                updated_at = excluded.updated_at",
            params![agent_id, name, config_json, now],
        )
        .context("Failed to upsert agent")?;

        let resolved_id = conn
            .query_row(
                "SELECT agent_id FROM agents WHERE name = ?1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .context("Failed to read back agent_id after upsert")?;

        drop(conn);
        self.get_agent_by_id(&resolved_id)?
            .ok_or_else(|| anyhow::anyhow!("Agent missing after upsert for name '{name}'"))
    }

    /// List workspace agent ids: "main" plus agent_id from agents where agent_id = name (workspace agents).
    pub fn list_workspace_agent_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT agent_id FROM agents WHERE agent_id = name ORDER BY agent_id")
            .context("Failed to prepare list_workspace_agent_ids query")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("Failed to query agents for workspace list")?;
        let mut ids: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to decode workspace agent ids")?;
        let mut out = vec![crate::multi_agent::DEFAULT_AGENT_ID.to_string()];
        out.append(&mut ids);
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Get workspace agent by agent_id (same as get_agent_by_id; main is not stored).
    pub fn get_workspace_agent(&self, agent_id: &str) -> Result<Option<Agent>> {
        if agent_id.trim().is_empty() || agent_id == crate::multi_agent::DEFAULT_AGENT_ID {
            return Ok(None);
        }
        self.get_agent_by_id(agent_id)
    }

    /// Insert or update workspace agent in agents table (agent_id = name = directory name).
    pub fn upsert_workspace_agent(&self, agent_id: &str, config_json: &str) -> Result<()> {
        if agent_id.trim().is_empty() || agent_id == crate::multi_agent::DEFAULT_AGENT_ID {
            anyhow::bail!("Cannot upsert workspace agent 'main'");
        }
        let conn = self.conn.lock();
        let now = Self::now();
        conn.execute(
            "INSERT INTO agents (agent_id, name, config_json, created_at, updated_at)
             VALUES (?1, ?1, ?2, ?3, ?3)
             ON CONFLICT(agent_id) DO UPDATE SET
                config_json = excluded.config_json,
                updated_at = excluded.updated_at,
                name = excluded.name",
            params![agent_id, config_json, now],
        )
        .context("Failed to upsert workspace agent in agents table")?;
        Ok(())
    }

    pub fn create_subagent_session(
        &self,
        agent_id: Option<&str>,
        meta_json: Option<&str>,
        outbound_key: Option<&str>,
    ) -> Result<SubagentSession> {
        let conn = self.conn.lock();
        let now = Self::now();
        let session_id = uuid::Uuid::new_v4().to_string();
        let inbound_key = format!("internal:{session_id}");
        let agent_id_val = agent_id.unwrap_or("main");
        conn.execute(
            "INSERT INTO sessions (session_id, inbound_key, agent_id, status, title, created_at, updated_at, meta_json, outbound_key)
             VALUES (?1, ?2, ?3, 'active', NULL, ?4, ?4, ?5, ?6)",
            params![session_id, inbound_key, agent_id_val, now, meta_json, outbound_key],
        )
        .context("Failed to create subagent session in sessions table")?;

        Ok(SubagentSession {
            subagent_session_id: session_id,
            agent_id: agent_id_val.to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            meta_json: meta_json.map(ToOwned::to_owned),
            outbound_key: outbound_key.map(ToOwned::to_owned),
        })
    }

    pub fn get_subagent_session(
        &self,
        subagent_session_id: &str,
    ) -> Result<Option<SubagentSession>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT session_id, agent_id, status, created_at, updated_at, meta_json, outbound_key
             FROM sessions
             WHERE session_id = ?1",
            params![subagent_session_id],
            |row| {
                Ok(SubagentSession {
                    subagent_session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    meta_json: row.get(5)?,
                    outbound_key: row.get(6)?,
                })
            },
        )
        .optional()
        .context("Failed to query subagent session from sessions")
    }

    /// Returns outbound_key for a session (for internal messages with empty reply_target).
    pub fn get_outbound_key_for_session(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let opt: Option<Option<String>> = conn
            .query_row(
                "SELECT outbound_key FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query outbound_key from sessions")?;
        Ok(opt.flatten())
    }

    pub fn list_subagent_sessions(&self, limit: u32) -> Result<Vec<SubagentSession>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, agent_id, status, created_at, updated_at, meta_json, outbound_key
                 FROM sessions
                 WHERE inbound_key LIKE 'internal:%'
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )
            .context("Failed to prepare list_subagent_sessions query")?;
        let rows = stmt
            .query_map(params![i64::from(limit)], |row| {
                Ok(SubagentSession {
                    subagent_session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    status: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    meta_json: row.get(5)?,
                    outbound_key: row.get(6)?,
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
            "UPDATE sessions SET status = 'stopped', updated_at = ?1 WHERE session_id = ?2",
            params![now, subagent_session_id],
        )
        .context("Failed to mark subagent session stopped")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionId, SessionStore};
    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    #[test]
    fn session_store_create_append_and_load_recent() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let inbound_key = "channel:telegram:chat-123";
        let session_id = store.get_or_create_active(inbound_key).unwrap();

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

        let newer_session = store.create_new(inbound_key, None).unwrap();
        assert_ne!(newer_session.as_str(), session_id.as_str());

        let active = store.get_or_create_active(inbound_key).unwrap();
        assert_eq!(active.as_str(), newer_session.as_str());
    }

    #[test]
    fn last_message_id_returns_none_when_no_messages_and_max_id_after_append() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let inbound_key = "channel:test:chat-last-id";
        let session_id = store.get_or_create_active(inbound_key).unwrap();

        assert!(store.last_message_id(&session_id).unwrap().is_none());

        store
            .append_message(&session_id, "user", "first", None)
            .unwrap();
        let id1 = store.last_message_id(&session_id).unwrap();
        assert!(id1.is_some());
        assert!(id1.unwrap() > 0);

        store
            .append_message(&session_id, "assistant", "second", None)
            .unwrap();
        let id2 = store.last_message_id(&session_id).unwrap();
        assert!(id2.is_some());
        assert!(id2.unwrap() >= id1.unwrap());

        let other_session = store.create_new("channel:test:other", None).unwrap();
        assert!(store.last_message_id(&other_session).unwrap().is_none());
    }

    #[test]
    fn session_store_skips_unsupported_roles() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let inbound_key = "channel:telegram:chat-123";
        let session_id = store.get_or_create_active(inbound_key).unwrap();

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
        let inbound_key = "channel:telegram:chat-compact";
        let session_id = store.get_or_create_active(inbound_key).unwrap();

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
        let inbound_key = "channel:telegram:list-check";
        let session_id = store.get_or_create_active(inbound_key).unwrap();
        store
            .append_message(&session_id, "user", "list-message", None)
            .unwrap();

        let sessions = store.list_sessions(Some(inbound_key), 10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id.as_str());
        assert_eq!(sessions[0].inbound_key, inbound_key);
        assert_eq!(sessions[0].message_count, 1);

        assert!(store.session_exists(&session_id).unwrap());
        assert!(!store
            .session_exists(&SessionId::from_string("missing-session-id"))
            .unwrap());
    }

    #[test]
    fn session_store_init_schema_creates_all_tables() {
        let workspace = TempDir::new().unwrap();
        let db_path = workspace.path().join("memory").join("sessions.db");
        let _store = SessionStore::new(workspace.path()).unwrap();
        drop(_store);

        let conn = Connection::open(db_path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, super::SESSION_SCHEMA_VERSION);

        let tables = ["sessions", "session_messages", "session_state", "agents"];
        for name in tables {
            let exists: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "table {} should exist", name);
        }
    }

    #[test]
    fn agent_upsert_list_get_by_id_and_name() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let agent = store
            .upsert_agent(
                "coder",
                r#"{"defaults":{"provider":"openrouter","model":"anthropic/claude-sonnet-4","temperature":0.2}}"#,
            )
            .unwrap();
        assert_eq!(agent.name, "coder");
        assert!(!agent.agent_id.is_empty());

        let by_id = store.get_agent_by_id(&agent.agent_id).unwrap().unwrap();
        assert_eq!(by_id.name, "coder");
        let by_name = store.get_agent_by_name("coder").unwrap().unwrap();
        assert_eq!(by_name.agent_id, agent.agent_id);

        let updated = store
            .upsert_agent("coder", r#"{"defaults":{"model":"gpt-4"}}"#)
            .unwrap();
        assert_eq!(updated.agent_id, agent.agent_id);
        assert_eq!(updated.config_json, r#"{"defaults":{"model":"gpt-4"}}"#);

        let list = store.list_agents(10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "coder");
    }

    #[test]
    fn get_agent_by_id_returns_none_for_unknown_id() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let agent = store.get_agent_by_id("unknown-agent-id").unwrap();
        assert!(agent.is_none());
    }

    #[test]
    fn list_agents_returns_empty_when_no_agents() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let list = store.list_agents(10).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn session_state_keys_active_agent_id_and_model_override_persist() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();
        let inbound_key = "channel:test:state-keys";
        let session_id = store.get_or_create_active(inbound_key).unwrap();

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
    fn subagent_session_uses_sessions_table() {
        let workspace = TempDir::new().unwrap();
        let store = SessionStore::new(workspace.path()).unwrap();

        let agent = store
            .upsert_agent(
                "reviewer",
                r#"{"provider":"openrouter","model":"anthropic/claude-sonnet-4"}"#,
            )
            .unwrap();
        assert_eq!(agent.name, "reviewer");

        let subagent_session = store
            .create_subagent_session(Some(agent.agent_id.as_str()), None, None)
            .unwrap();
        assert!(!subagent_session.subagent_session_id.is_empty());
        assert_eq!(subagent_session.agent_id, agent.agent_id);
        assert_eq!(subagent_session.status, "active");

        let looked_up = store
            .get_subagent_session(subagent_session.subagent_session_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(
            looked_up.subagent_session_id,
            subagent_session.subagent_session_id
        );
        assert_eq!(looked_up.agent_id, agent.agent_id);

        store
            .mark_subagent_session_stopped(subagent_session.subagent_session_id.as_str())
            .unwrap();
        let stopped = store
            .get_subagent_session(subagent_session.subagent_session_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(stopped.status, "stopped");
    }
}
