# Command System

ZeroClaw supports a rich set of Slash Commands (`/`) to manage sessions, sub-agents, and configuration within a conversation.

## 1. Session Management

- **`/new`**: Starts a fresh session within the current chat context.
    - **Effect**: Clears the agent's short-term history for this group/thread. Useful when starting a new topic or debugging.
    - **Persistence**: Previous sessions are archived and can be listed or re-activated via the database.

- **`/sessions`**: Lists the current session and previous sessions associated with the current conversation key.
    - Shows session ID, status (active/inactive), and message count.

- **`/compact`**: Triggers a manual compaction event for the active session.
    - Forces the agent to summarize the current history and move the compaction boundary forward, saving context window space.

## 2. Queue Configuration

- **`/queue [mode]`**: Configures how concurrent messages are handled for the session.
    - **Supported Modes**: 
        - `steer-backlog` (Default): Concurrent messages are queued and injected as steering context on the next turn.
    - **Persistence**: Queue mode is stored in `sessions.db` per-session.

## 3. Sub-Agent Monitoring

- **`/subagents`**: Provides a quick overview of the sub-agent system status.
    - **Specs**: Lists available sub-agent specifications (e.g., model/provider profiles).
    - **Sessions**: Lists active and inactive sub-agent sessions.
    - **Runs**: Lists the status of recent sub-agent runs (queued, running, succeeded, failed).

## 4. General Commands

- **`/quit` / `/exit`**: (CLI only) Safely exits the interactive mode.

## 5. Implementation Notes
- **Channel Support**: Commands are intercepted at the channel layer (`src/channels/mod.rs`) before reaching the agent.
- **Routing**: Commands are executed "inline"—they do not trigger an LLM turn but return an immediate system response to the user.
- **Access Control**: Commands are only available on channels where `session.enabled = true`.
