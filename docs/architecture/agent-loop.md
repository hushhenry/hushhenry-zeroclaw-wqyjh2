# Agent Execution Loop

The Agent Loop is the central execution cycle of ZeroClaw. It manages the interaction between the user, the LLM, and the available tools.

## 1. The Core Cycle (`run_tool_call_loop`)
The agent operates in an iterative loop (capped at `MAX_TOOL_ITERATIONS = 10` by default). Each iteration consists of:

1.  **Context Construction**: The system prompt, session history (including compaction summary), and recent user/backlog messages are assembled into a conversation history.
2.  **LLM Request**: The history and tool specifications are sent to the configured LLM provider.
3.  **Response Parsing**: The LLM's response is checked for **Tool Calls** (either XML-style `<tool_call>` tags or provider-native JSON).
4.  **Tool Execution**: If tool calls are found, they are executed.
5.  **History Enrichment**: Tool results are appended to the history, and the loop continues until the LLM produces a final text-only response or hits the iteration cap.

## 2. Tool Use Protocol
ZeroClaw uses a strict, tag-based protocol for tool invocation to ensure compatibility across diverse models:
- **Invocation**: `<tool_call>{"name": "...", "arguments": {...}}</tool_call>`
- **Results**: `<tool_result name="...">...</tool_result>`
- **Redaction**: Tool outputs are automatically scrubbed for credentials (API keys, passwords) before being sent back to the LLM.

## 3. Memory & Context Management
- **Trim History**: Once history exceeds the limit, older messages are discarded (hard cap).
- **Auto-Compaction**: Before trimming, the agent can summarize older messages into a `compaction_summary` (OpenCode-style) to preserve key facts and decisions without ballooning the token count.
- **Context Loading**: On each turn, relevant entries from the global long-term memory are searched and optionally injected as `[Memory context]`.

## 4. Observer & Monitoring
Every stage of the loop (LLM start/finish, tool start/finish) is recorded by an `Observer` for real-time health monitoring, audit logging, and performance analysis.
