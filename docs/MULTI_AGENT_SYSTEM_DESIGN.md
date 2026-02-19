# Zeroclaw Multi‑Agent System Design (OpenClaw‑inspired)

Status: **Design proposal** (no implementation yet)

Owner: @wqyjh + contributors

Last updated: 2026-02-19

---

## 1. Motivation

Zeroclaw currently has a **subagent runtime** that behaves like an async job queue (`subagent_runs`) rather than a **full multi‑agent system**.

We want a system where:

- A subagent is a **first‑class, fully capable agent** with its own persistent session/history and configurable defaults (model/tools/skills/memory policy).
- A subagent’s **input does not come from external chat channels**; it comes from the parent/orchestrator agent.
- A subagent’s **output does not go to external channels** by default; it returns to the orchestrator agent.
- Users can **switch models** and **switch agents** at the session level (like OpenClaw’s `/models` + `/model`), and can create agents via natural language (through tools + guided collection).

This document proposes an architecture for Zeroclaw that adopts the proven design patterns from OpenClaw.

---

## 2. Design Principles (Lessons from OpenClaw)

OpenClaw’s multi‑agent/subagent behavior is stable in practice because it separates concerns:

1) **Session = durable conversation state**
- Subagents are independent sessions (child sessions).

2) **Internal execution ≠ external delivery**
- An agent turn can run and persist to transcript while `deliver=false` prevents sending to external channels.
- A dedicated internal “message channel” is used for agent-to-agent injections.

3) **Announce flow (structured report) rather than raw forward**
- Child results are summarized/packaged into a “system/announce” style message which is injected into the parent session to drive orchestration.

4) **Queue/lane aware scheduling**
- When the parent is busy, child completion messages are queued/steered to avoid interrupting a running tool loop.

5) **Hard loop prevention**
- Idempotency keys, TTL/hop limits, self-send protection.

Zeroclaw should implement these **as system mechanics**, not prompt conventions.

---

## 3. Key Entities

### 3.1 AgentSpec (new)

A persistent description of an agent profile.

Minimum fields:

- `agent_id` (stable id)
- `name` (human-friendly, unique)
- `description`
- `defaults.model`: `{ provider, model, temperature? }`
- `defaults.system_prompt?`
- `policy.tools`: allow/deny list (tool registry filter)
- `policy.skills`: allow/deny list (skill injection filter)
- `policy.memory`: scope + autosave settings
- `workspace.overlay?`: whether to use `workspace/agents/<agent_id>/...` bootstrap overlays

**Storage options:**

- Preferred: SQLite table in `memory/sessions.db` (queryable, updateable, command-friendly)
- Secondary export: `workspace/agents/<agent_id>/agent.toml` for Git visibility

### 3.2 Session

A conversation instance. Every session has:

- `session_key` / `session_id`
- `agent_id` (which AgentSpec drives turns for this session)
- `active_agent_id` (for user-facing sessions that can switch agent)
- `provider_override` / `model_override` (session-level overrides)
- `deliver_policy` / `send_policy` (whether outbound sends are allowed)
- `owner_session_key?` (parent session that spawned this session)
- `channel` + routing metadata

**Important:** Subagents are sessions; they do not use a separate “run queue DB model” for persistence.

---

## 4. Channels & Delivery Model

### 4.1 Internal message channel

Introduce a reserved internal channel identifier:

- `INTERNAL_MESSAGE_CHANNEL = "agent"` (or `"internal"`)

Any agent-to-agent injection must set channel to this value.

### 4.2 Deliver gating (must be hard-coded)

Rule:

- External delivery is allowed **only if** `wants_deliver==true` **AND** `resolved_channel != INTERNAL_MESSAGE_CHANNEL`.

This mirrors OpenClaw’s `deliver && channel !== INTERNAL_MESSAGE_CHANNEL` behavior and prevents accidental external posts from internal injections.

### 4.3 Three classes of messages

We distinguish message *roles* from message *semantics*:

1) **Orchestration input** (drives another agent turn)
- Best practice: inject as a normal “user turn” into the target session, but mark provenance.

2) **Event/logging** (does not drive a turn)
- Store as session event/custom entry (no LLM turn).

3) **Tool protocol** (structured)
- Use actual tool call/result roles/structures; do not repurpose tool messages for natural language agent-to-agent reports.

---

## 5. Subagent Spawn & Execution

### 5.1 Spawn

API concept:

- `spawn_subagent(parent_session_key, agent_id, label?, meta?) -> child_session_key`

Child session defaults:

- `channel = INTERNAL_MESSAGE_CHANNEL`
- `deliver_policy = false`
- `owner_session_key = parent_session_key`
- `lane = subagent` (for concurrency)

### 5.2 Agent-to-agent injection

API concept:

- `inject_message(session_key, message, {channel, deliver, lane, idempotency_key, provenance})`

This *persists* a new message in the target session and schedules an agent turn.

### 5.3 Completion & announce

When a child session completes a unit of work, Zeroclaw should run an **announce flow**:

1) Read the child’s latest assistant reply (or last meaningful toolResult if assistant is empty).
2) Build a structured report message (see §6).
3) Inject it into the parent session (internal channel, deliver=false by default).
4) Parent agent decides whether to reply externally.

This is intentionally not “raw forward of child reply”.

---

## 6. Announce Format (Best Practice: **B = semantic payload in meta**, session stays clean)

We intentionally **do not** persist large “memory/recall-like” context blocks inside session history.
In a multi-agent system, child→parent handoff must avoid transcript bloat and keep turns stable.

Therefore we choose best practice **B**:

- The **persisted** message in the parent session may be short (even `finish`),
- while the **semantic payload** (task + result summary + identifiers) lives in `meta_json`.
- During the *next* parent agent turn, the runtime may render a compact, ephemeral context block
  from this meta (for the model), but **must not** write that rendered block back into session history.

### 6.1 Persisted parent-session message (minimal)

Recommended persisted content (user-role injection):

- `[@agent:<name>#<agent_id>] finish`

Keep it short. The point is to record **that** a child completed, not to store the whole report inline.

### 6.2 Semantic payload (meta_json)

Attach a structured payload to the injected message:

```json
{
  "internal": true,
  "kind": "subagent_announce",
  "trace_id": "...",
  "hop": 0,
  "idempotency_key": "announce:...",

  "source_agent_id": "agt_...",
  "source_agent_name": "coder",
  "source_session_key": "session:child...",
  "source_run_id": "run_...",

  "task": {
    "label": "<label>",
    "prompt": "<original task prompt or compact summary>",
    "tags": ["project:x", "topic:y"]
  },
  "result": {
    "status": "ok|error|timeout|unknown",
    "summary": "<200-800 chars>",
    "artifacts": [
      {"kind": "file", "path": "docs/out.md"},
      {"kind": "commit", "sha": "..."}
    ]
  },
  "stats": {
    "duration_ms": 1234,
    "tokens": null,
    "cost_usd": null
  }
}
```

Notes:
- `result.summary` is the **semantic anchor** for recall/query building; never rely on the persisted text
  (e.g. `finish`) for retrieval.
- `artifacts[]` are stable anchors (paths/commit shas) and should be preferred when available.

### 6.3 Ephemeral rendering (for the *next* turn only)

When building the next parent prompt, the runtime may render a compact block:

```
[Context: subagent_announce]
From: <name>#<id>
Task: <label>
Result: <summary>
Artifacts: <paths/shas>
[/Context]
```

Hard rule: **Never persist this rendered context block into session history.**

### 6.4 Provenance / delivery semantics

The injected message must set:

- `channel = INTERNAL_MESSAGE_CHANNEL`
- `deliver = false` (default)

External delivery is only allowed if explicitly requested and must obey deliver gating.

---

## 7. Queue, Lanes, and Busy-Parent Handling

Without scheduling, multi-agent systems become unstable (interruptions, reentrancy, loops).

### 7.1 Lanes

Introduce lanes similar to OpenClaw:

- `main`
- `subagent`
- `cron` (optional)
- `internal_announce`

Each lane has configurable concurrency.

### 7.2 Per-session serialization

Even with lane concurrency, enforce:

- **At most 1 active agent turn per session**

### 7.3 Busy parent: steer/backlog/followup/collect

When the parent session is running:

- **steer**: try to append the announce injection into the in-flight run’s queue.
- **followup**: enqueue injection to run immediately after the current turn finishes.
- **collect**: merge multiple subagent announces into a single summary injection.

This avoids breaking tool iteration state.

---

## 8. Loop Prevention (Hard Requirements)

Because agent-to-agent injections may appear as user turns, we must prevent self-amplifying loops.

Hard rules:

1) **No self-send**
- `target_session_key != source_session_key`

2) **TTL/hop limit**
- increment hop on every injection; drop when `hop > MAX_HOPS`.

3) **Idempotency**
- each announce injection uses a deterministic idempotency key derived from (parent, child, run).

4) **Delivery gating**
- internal channel injections never deliver externally.

5) **Reentrancy protection**
- if a session already has an active turn, injections become followups (or steer/backlog).

---

## 9. Models & Agents: Commands and Tools

### 9.1 User commands (CLI/chat)

#### `/models`

Behavior (variant B):

- Output `provider/model` lines.
- Cap max lines (e.g. 60).
- For providers with too many models, show first K then `+ (n more)`.

#### `/model <provider>/<model>`

- Sets session-level model override.

#### `/agents`

- List available AgentSpecs.
- Show current session `active_agent_id`.

#### `/agent <agent-name|agent-id>`

- Exact match only.
- Switches session’s `active_agent_id`.

### 9.2 LLM tools (natural language)

To support “帮我切模型/切 agent/创建 agent”，provide tools:

- `models_list(provider?)`
- `session_set_model(model_ref)`
- `agents_list()`
- `session_set_agent(agent_id|name)` (the tool can support fuzzy matching; `/agent` command does not)
- `agent_create(draft)` and `agent_update(patch)`

---

## 10. Skills, Tools, Workspace, Memory in a Multi‑Agent World

### 10.1 Skills

Current state: skills are loaded globally from `workspace/skills` and injected as a list.

Target state:

- Skills remain discoverable globally, but **each agent has a skills allowlist/denylist**.
- System prompt injection enumerates only allowed skills for the active agent.

### 10.2 Tools

Target state:

- Tool registry is filtered per agent via `policy.tools`.
- High-risk tools (shell, http, sessions tools) should be denied by default for subagents unless explicitly allowed.

### 10.3 Workspace overlays

Support per-agent bootstrap overlays:

- `workspace/agents/<agent_id>/SOUL.md`
- `workspace/agents/<agent_id>/TOOLS.md`
- `workspace/agents/<agent_id>/USER.md`
- etc.

If overlay files exist, use them; otherwise fall back to root workspace files.

### 10.4 Memory

Current state: memory writes often pass `session_id=None` and use fixed keys like `user_msg`.

Target state:

- Memory operations must always carry `session_id` (or `session_key`).
- Keys must be session-scoped to avoid collisions.
- Memory policy should allow:
  - per-session recall (conversation-local)
  - per-agent recall (agent preferences)
  - global recall (user facts)

This enables multi-agent isolation while retaining shared facts where desired.

---

## 11. Suggested Implementation Phases (for later)

### Phase 1 (core multi-agent skeleton)

- Add AgentSpec registry + `/agents` `/agent` + tools.
- Add session-level model overrides + `/models` `/model`.
- Add internal channel + deliver gating.
- Convert subagent concept from `subagent_runs` to child sessions + announce injections.

### Phase 2 (stability)

- lanes + per-session serialization + busy-parent strategies.
- idempotency + TTL/hop.

### Phase 3 (ergonomics)

- agent builder (natural language creation).
- overlay workspace files.
- memory scoping policy.

---

## 12. Compatibility Notes (current Zeroclaw)

- Existing `subagent_spawn_oneshot` should become a **compatibility wrapper** or a **skill**.
- Existing `subagent_specs` can be migrated into AgentSpecs (model/system prompt fields map cleanly).

---

## Appendix A: OpenClaw References (for reviewers)

- `sessions_spawn` behavior: deliver=false child session + announce step.
- Internal channel gating: `deliver && channel !== INTERNAL_MESSAGE_CHANNEL`.
- Subagent announce flow: generate triggerMessage and inject via gateway `agent`.
- Queue/steer-backlog patterns to avoid interrupting active runs.

(See OpenClaw sources: `docs/concepts/session-tool.md`, `src/agents/subagent-announce.ts`, `src/gateway/server-methods/agent.ts`.)
