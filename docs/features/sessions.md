# Session Management

ZeroClaw implements a **session-first** architecture to ensure stable, isolated, and cost-effective interactions across different channels and users.

## Core Concepts

### 1. Session-First Routing
Unlike legacy models that treat every message as an independent event, ZeroClaw groups messages into persistent **Sessions**.
- **SessionResolver**: Automatically maps incoming channel metadata (User ID, Group ID, Thread ID) to a unique `SessionKey`.
- **Precedence**: Thread > Group > Direct. A conversation in a specific thread remains isolated from the main group chat.
- **`/new` Command**: Users can start a fresh session at any time within the same chat context, effectively resetting the agent's short-term memory for that conversation.

### 2. SQLite Session Store
All session data is persisted in a dedicated SQLite database located at `workspace/memory/sessions.db`.
- **Persistence**: Only `user`, `assistant`, and `tool` messages are stored. System events and transient notifications are excluded to keep the history clean.
- **Connection Pooling**: Uses a persistent connection model to minimize I/O overhead.

### 3. Context Enrichment & Memory Isolation
When session mode is enabled (`config.session.enabled = true`), the agent's context is built exclusively from the session history.
- **Mutual Exclusion**: Session history and global RAG (memory recall) are mutually exclusive by default to prevent context pollution and reduce token costs.
- **Stability**: This ensures that \"coding sessions\" or complex multi-step tasks have a reliable, linear history.

## OpenCode-Style Compaction

To handle long-running conversations without exceeding model context limits or ballooning costs, ZeroClaw uses a compaction strategy inspired by OpenCode:

1. **Summary Boundary**: When a session exceeds the `history_limit`, older messages are summarized into a `compaction_summary`.
2. **Tail Messages**: Only messages *after* the `compaction_after_message_id` boundary are loaded as full-text history.
3. **Prefix-Cache Friendly**: The prompt structure is strictly ordered to maximize provider-side prefix caching:
   - System Prompt (Static)
   - Compaction Summary (Stable)
   - Tail Messages (Growing)
   - Latest User Message

## Configuration

Enable session management in `config.toml`:

```toml
[session]
enabled = true
history_limit = 40 # Number of messages to keep before compacting
```
