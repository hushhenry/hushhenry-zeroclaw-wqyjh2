# Session Management & Context Stability

ZeroClaw's session-first architecture is designed to handle high-concurrency, multi-channel interactions while maintaining strict context isolation and controlling operational costs through advanced compaction.

## 1. Session-First Architecture

In ZeroClaw, every interaction is bound to a **Session**. This differs from traditional stateless agent loops by introducing a persistent identifier for every conversation thread.

### Session Resolution Logic
The `SessionResolver` (found in `src/session/resolver.rs`) is responsible for normalizing diverse channel metadata into a stable `SessionKey`.
- **Precedence Rules**: 
    1. **Thread/Topic**: If the platform supports threads (e.g., Discord threads, Telegram topics), the `thread_id` takes top priority.
    2. **Group/Room**: If no thread exists, the message is mapped to the `group_id`.
    3. **Direct Message**: Private messages use the user's unique ID as the session key.
- **Normalization**: `ChatType` is a normalized enum (`Direct | Group | Thread | Unknown`) used to ensure consistent behavior across different adapters (Feishu, Slack, etc.).

### Session Persistence
ZeroClaw utilizes a dedicated SQLite database (`workspace/memory/sessions.db`) to store session data.
- **Table Schema**:
    - `session_index`: Metadata about known sessions.
    - `sessions`: Active session states (active vs. inactive).
    - `session_messages`: The actual history of `user`, `assistant`, and `tool` interactions.
    - `session_state`: Operational state including compaction boundaries.
- **Performance**: A persistent SQLite connection is maintained to prevent the overhead of repeated file handles, similar to the core memory implementation.

## 2. Context Enrichment & Mutual Exclusion

To prevent "hallucination" caused by mixed contexts, ZeroClaw implements a strict separation of concerns when session mode is active (`config.session.enabled = true`):
- **Memory vs. Session**: If a session is active, the agent primarily relies on the **Session History**. The global memory RAG (Recall) is disabled for that turn unless explicitly requested, preventing cross-conversation context leakage.
- **System Events**: Internal system logs and transient notifications are *not* persisted in the session history, ensuring the agent's context window is occupied only by relevant conversation data.

## 3. OpenCode-Style Compaction

Managing long-form conversations requires balancing history depth with token costs and context limits. ZeroClaw adopts a "Summary + Tail" strategy inspired by OpenCode.

### The Compaction Flow
1. **Trigger**: When the number of messages in a session exceeds `history_limit` (default: 40), a compaction event is triggered.
2. **Summarization**: Older messages are passed to a summarization prompt. The resulting text is stored in `session_state.compaction_summary`.
3. **Boundary Marking**: The ID of the last summarized message is saved as `compaction_after_message_id`.
4. **Context Construction**: When building the prompt for the next turn, the system assembles fragments in a specific order to optimize for **Prefix Caching**:
    - **System Prompt**: Constant.
    - **Compaction Summary**: Stable (only updates when a new compaction occurs).
    - **Tail Messages**: Only messages *after* the boundary.
    - **Current Input**: The latest user message.

## 4. Configuration Reference

```toml
[session]
enabled = true          # Toggle session-first mode
history_limit = 40      # Threshold for triggering compaction
# Note: Ensure the model used for summarization is capable of handling the current history.
```
