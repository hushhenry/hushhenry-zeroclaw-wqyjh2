# Sub-Agents

Sub-Agents allow the primary ZeroClaw agent to delegate complex or long-running tasks to specialized background agents.

## 1. Sub-Agent Runtime
The `SubagentRuntime` manages the lifecycle of delegated tasks. It consists of:
- **Task Queue**: Runs are enqueued and persisted in `sessions.db`.
- **Background Worker**: A dedicated loop that claims queued runs and executes them.
- **Persistence**: Runs are restart-safe. If the agent crashes, "running" tasks are recovered to "queued" on restart.

## 2. Delegation Model
When the primary agent spawns a sub-agent:
1.  **Creation**: A new `SubagentSession` is created.
2.  **Prompting**: The sub-agent is given a specific task prompt and optional input data.
3.  **Specs**: Sub-agents can use different "Specs" (predefined model, provider, and system prompt configurations) to match the complexity of the task (e.g., a "Reviewer" spec vs. a "Coder" spec).

## 3. Concurrency & Isolation
- **Serial per Session**: Within a single `SubagentSession`, runs are executed sequentially to maintain logical order.
- **Parallel across Sessions**: Different sub-agent sessions (or runs from different users) execute in parallel.
- **Resource Management**: Each sub-agent turn runs in its own isolated `agent_turn` context with its own iteration limits.

## 4. Communication & Results
- **Polling**: The parent agent can use `subagent_poll` to check the status or retrieve the final output.
- **Steer-Backlog**: Sub-agents can be configured to notify the parent agent via the session backlog upon completion.
- **Tooling**: A suite of sub-agent tools (`subagent_spawn`, `subagent_send`, `subagent_poll`, `subagent_stop`) allows for fine-grained control over delegation.
