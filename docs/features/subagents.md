# Sub-Agents

Sub-Agents allow the primary ZeroClaw agent to delegate to specialized background agents via session-based async flow.

## 1. Sub-Agent Runtime
The `SubagentRuntime` provides shared access to the session store for subagent sessions. It does not run a background worker; subagent sessions are created and consumed by the async channel/session loop.

## 2. Delegation Model
When the primary agent spawns a sub-agent:
1. **Creation**: A new `SubagentSession` is created (e.g. via `subagent_send` with prompt/context).
2. **Specs**: Sub-agents can use different "Specs" (predefined model, provider, and system prompt configurations) when creating a session.
3. **Session-based**: Execution is driven by the async session model (no oneshot "run" queue).

## 3. Concurrency & Isolation
- **Sessions**: Each sub-agent session is isolated; multiple sessions can exist in parallel.
- **Resource Management**: When subagent sessions are processed (e.g. by a channel or gateway), each turn runs in its own context with iteration limits.

## 4. Communication & Results
- **Tooling**: The `subagent_send` tool creates or reuses a subagent session and returns session id and status.
- **Steer-Backlog**: Sub-agent sessions can carry `parent_session_id` in meta for future announce/backlog integration.
