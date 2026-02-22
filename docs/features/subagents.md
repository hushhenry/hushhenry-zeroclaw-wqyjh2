# Sub-Agents

Sub-Agents allow the primary ZeroClaw agent to delegate to specialized background agents via session-based async flow.

## 1. Sub-Agent Runtime
The `SubagentRuntime` provides shared access to the session store for subagent sessions. It does not run a background worker; subagent sessions are created and consumed by the async channel/session loop.

## 2. Delegation Model
When the primary agent spawns a sub-agent:
1. **Creation**: A new `SubagentSession` is created via the `subagent` tool with `action: spawn` (optional `agent_id`, default `main`; optional `parent_session_id` injected by runtime for outbound).
2. **Specs**: Sub-agents can use different agent ids when creating a session.
3. **Session-based**: Execution is driven by the async session model (no oneshot "run" queue).

## 3. Concurrency & Isolation
- **Sessions**: Each sub-agent session is isolated; multiple sessions can exist in parallel.
- **Resource Management**: When subagent sessions are processed (e.g. by a channel or gateway), each turn runs in its own context with iteration limits.

## 4. Communication & Results
- **Tooling**: The `subagent` tool supports three actions: **spawn** (create session; returns `session_id`), **send** (send `input` to `internal:{session_id}`), **stop** (send `/stop` to `internal:{session_id}` and mark session stopped).
- **Steer-Backlog**: Sub-agent sessions can carry `parent_session_id` in meta (outbound = `internal:{parent_session_id}`) for announce/backlog integration.
