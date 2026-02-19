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

## 2. Design Principles

This design is compatible with OpenClaw patterns, but is grounded in first principles.

A multi-agent system must solve three problems:

1) **State** — durable, multi-turn conversations
2) **Capability** — configurable differences between agents
3) **Scheduling** — safe concurrency without reentrancy, loops, or duplication

From that, the simplest effective architecture is:

- **Session** (state): persistent transcript + session config
- **AgentSpec** (capability): model + skills/tools/context policies
- **TurnJob** (scheduling): a queued unit of work that runs *one* session turn

Everything else ("subagents", "announce", "lanes") is an implementation detail built on these primitives.

### 2.1 Hard rules (mechanics, not prompt conventions)

- **Per-session serialization:** at most 1 active TurnJob per session.
- **Internal execution ≠ external delivery:** internal events default to `deliver=false`.
- **Ephemeral context:** recall/context blocks are built per turn and are **not** written into chat history. (Rationale: prevents transcript bloat, avoids turning recall into permanent “facts”, and keeps retrieval + debugging sane.)
- **Loop prevention:** idempotency + TTL/hop + no self-send.

### 2.2 Subagents are just sessions

A “subagent” is not a special runtime. It is simply:

- a **child session** (with its own transcript) whose input events come from a parent session,
- and whose output is converted into a parent-session **backlog injection** (typically via announce meta payload + a minimal injected message).

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
- `context_policy`: whether to use memory recall, budgets/filters (memory is global; no agent isolation)
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
- `lane = subagent` (optional; only if we keep lane-based concurrency)

### 5.2 Agent-to-agent injection

API concept:

- `inject_message(session_key, message, {channel, deliver, idempotency_key, provenance, meta_json})`

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

This section also defines **steer-backlog** semantics precisely.

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

### 7.3 Queue mode (per session): **steer-backlog only**

To keep Zeroclaw minimal and predictable, we support exactly **one** queue mode:

- **steer-backlog** (aka **steer+backlog**) — always on.

Semantics:

- If a session has an in-flight agent turn (often with many tool iterations) and a new inbound message arrives,
  the runtime **steers** to the new message at the next safe tool boundary (so the user does not wait for the long task).
- To avoid losing the interrupted work, the runtime also appends a **resume token** for the interrupted task into a
  per-session **backlog** so it can continue after the steered turn completes.

Implementation notes:

- No time-based collect/debounce window in core.
- Backlog items are drained and merged into a single `[Backlog]` user message at the start of the next turn.
- If steering is not possible (no safe boundary), fall back to **backlog-only** for the new message (i.e. enqueue it and run after the current turn).

Defaults:

- External user messages: steer-backlog.
- Internal announce traffic: also uses backlog (steer is generally not meaningful), keeping announce text short and storing semantics in meta_json (best practice B).

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
- if a session already has an active turn, inbound messages are handled via **steer-backlog** (steer at safe boundary + backlog resume token).

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

### 10.4 Memory (**Step 0: refactor to sqlite-only**)

To reduce complexity and future coding surface area, Zeroclaw should adopt a **single memory backend**.

**Decision:** The first refactor step is **sqlite-only** memory (keep `brain.db` as source of truth).

Rationale:

- Multi-agent needs stable, testable primitives (FTS/vector/hybrid search, metadata, indexing, snapshots).
- Multiple backends (markdown + sqlite) create ambiguous semantics and multiply migration paths.
- sqlite aligns with the "ephemeral context, not persisted" rule: recall blocks are generated per turn and never written into chat history.

Implications:

- Deprecate/remove `MarkdownMemory` backend.
- Provide optional **export** commands (e.g. `memory export --format md|jsonl`) for human-readable audits and Git diffing.

Current state:

- Memory writes often pass `session_id=None` and use fixed keys like `user_msg`.

Target state (still shared/global memory, with better context control):

- Memory operations must always carry `session_id` (or `session_key`) for *ranking/weighting* and collision avoidance.
- Keys must be namespaced to avoid collisions.
- Recall should be configurable per agent via `context_policy` (memory on/off, budgets, filters), but **recall content is never persisted as chat messages**.

---

## 11. Module Impact Map (what changes where)

This section connects the design to concrete Zeroclaw modules so implementation work is scoped and reviewable.

### 11.1 Core runtime / loop

- `src/agent/loop_.rs`
  - Must support **steer-backlog** semantics (safe tool boundaries, resume token/backlog injection).
  - Must support "ephemeral context" injection for announce meta payloads (rendered only for the next turn, not persisted).

- `src/agent/agent.rs`
  - Ensure tool-loop boundaries are explicit enough for steering.

### 11.2 Session persistence & state

- `src/session/store.rs`
  - Add **AgentSpec registry** storage (new table) and session-level `active_agent_id` + model override fields.
  - Store per-session queue state (already stores `queue_mode`; we will collapse to steer-backlog-only).

- `src/session/backlog.rs`
  - Backlog queue used by steer-backlog (resume tokens + deferred messages).
  - Consider extending backlog entries from `String` to a structured JSON payload when needed (idempotency/meta).

### 11.3 Channels / routing

- `src/channels/mod.rs` (and per-channel adapters)
  - Resolve inbound messages → session_key/session_id.
  - Enforce **deliver gating** for internal message channel.
  - Expose `/models`, `/model`, `/agents`, `/agent`, `/queue` (steer-backlog-only) commands.

### 11.4 Memory

- `src/memory/*`
  - Step 0: sqlite-only backend (remove markdown backend).
  - Ensure memory recall is optional per agent via `context_policy`.

### 11.5 Skills & tools

- `src/skills/mod.rs`
  - Provide global skill discovery, but filter injected skill list per `AgentSpec.policy.skills`.

- `src/tools/mod.rs` + individual tools
  - Filter tool registry per `AgentSpec.policy.tools`.
  - Deprecate legacy delegate→subagent-spec mappings once AgentSpec is first-class.

### 11.6 Subagents (re-architecture)

- `src/subagent/*` + `src/tools/subagent_*`
  - Current `subagent_runs` queue is a compatibility layer.
  - Target: subagents are **child sessions** + announce meta payload + minimal injected message.

---

## 12. Detailed Implementation Plan

This plan is ordered to reduce complexity early and keep the system shippable at each step.

### Milestone 0 — Make memory sqlite-only (complexity reduction)

Goal: one memory backend; reduce migration/testing surface.

Tasks:

1) Remove/disable markdown backend selection
   - Update config schema to accept only `sqlite` (or default to sqlite and ignore others).
2) Delete or deprecate `MarkdownMemory` code paths
   - Ensure no tools/tests depend on it.
3) Add optional export tooling (non-blocking)
   - `memory export --format md|jsonl` for audits.

Acceptance:

- All tests pass with sqlite-only.
- No runtime flags reference markdown memory.

### Milestone 1 — AgentSpec registry + session-level switching

Goal: represent "agent" as a first-class profile that controls model + skills/tools + context policy.

Tasks:

1) DB schema
   - Add `agent_specs` table (id, name, config_json, created_at, updated_at).
   - Add session state fields for `active_agent_id`, `model_override` (if not present).
2) Commands
   - `/agents` lists specs + current session active agent.
   - `/agent <id|name>` exact switch.
   - `/models` and `/model` (provider/model) switch.
3) Runtime resolution
   - On each turn, resolve `active_agent_id` → AgentSpec → effective model/tools/skills/context policy.

Acceptance:

- Can create a few static AgentSpecs (seeded) and switch sessions between them.
- Switching changes model/tools exposure for subsequent turns.

### Milestone 2 — Skill/tool filtering by AgentSpec

Goal: make agent differences real (not just prompt text).

Tasks:

1) Skill injection filter
   - Only list allowed skills in system prompt.
2) Tool registry filter
   - Only register allowed tools.
   - High-risk tools default-deny for non-main agents unless explicitly allowed.

Acceptance:

- Switching agent changes available tools/skills immediately.
- Disallowed tools cannot be called.

### Milestone 3 — Steer-backlog-only queue semantics (correctness + UX)

Goal: match the steer-backlog semantics: interrupt long tool loops without losing the original task.

Tasks:

1) Identify "safe tool boundary" points in the tool loop.
2) When a new inbound message arrives during an active turn:
   - schedule the new message to run next (steer)
   - push a resume token for the interrupted task into backlog
3) On next turn start:
   - drain backlog and merge into a single `[Backlog]` user message

Acceptance:

- A long tool loop can be interrupted by a new user message.
- After responding to the new message, the previous task resumes from backlog.

### Milestone 4 — Subagents as child sessions + announce meta payload (B)

Goal: replace subagent-as-job with subagent-as-session.

Tasks:

1) Spawn child session (deliver=false, internal channel)
2) Parent→child message injection uses internal channel + deliver gating
3) Child→parent announce:
   - inject minimal user message (e.g. `[@agent] finish`)
   - attach semantic payload to `meta_json` (task/result/artifacts)
4) Parent consumes announce payload in next turn via ephemeral rendering (not persisted)

Acceptance:

- Parent can spawn N subagents in parallel (N child sessions).
- Child results are routed back without external delivery.

### Milestone 5 — Hardening (idempotency/TTL/observability)

Tasks:

- Deterministic idempotency keys for announce injections.
- TTL/hop enforcement.
- Basic debug logging for steering/backlog events.

Acceptance:

- No duplicate announces.
- No self-send loops.

### Milestone 6 — Cleanup & compatibility removal

Tasks:

- Deprecate/remove `subagent_runs` queue and oneshot wrappers.
- Remove legacy agent mappings.

Acceptance:

- Only Session + AgentSpec + TurnJob model remains.

### Non-goals (for this plan)

- Time-based collect/debounce window (explicitly out of scope).
- Agent-isolated memory stores (memory remains global; agents differ via model/skills/tools/context_policy).

---

## 13. Suggested Implementation Phases (legacy summary)

(Kept only as a short index; the detailed plan above is authoritative.)

- Phase 0: sqlite-only memory
- Phase 1: AgentSpec + switching
- Phase 2: skills/tools filtering
- Phase 3: steer-backlog correctness
- Phase 4: subagents as sessions + announce meta payload
- Phase 5: hardening + cleanup

### 13.1 Ergonomics (optional follow-ons)

- Agent builder (natural language creation).
- Workspace overlays (persona/bootstrap overrides).

---

## 14. Compatibility Notes (current Zeroclaw)

- Existing `subagent_spawn_oneshot` should become a **compatibility wrapper** or a **skill**.
- Existing `subagent_specs` can be migrated into AgentSpecs (model/system prompt fields map cleanly).

---

## Appendix A: OpenClaw References (for reviewers)

- `sessions_spawn` behavior: deliver=false child session + announce step.
- Internal channel gating: `deliver && channel !== INTERNAL_MESSAGE_CHANNEL`.
- Subagent announce flow: generate triggerMessage and inject via gateway `agent`.
- Queue/steer-backlog patterns to avoid interrupting active runs.

(See OpenClaw sources: `docs/concepts/session-tool.md`, `src/agents/subagent-announce.ts`, `src/gateway/server-methods/agent.ts`.)
