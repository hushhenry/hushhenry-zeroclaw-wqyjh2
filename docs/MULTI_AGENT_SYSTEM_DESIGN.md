# Zeroclaw Multi‑Agent System Design (RFC)

Status: **Design proposal** (no implementation yet)

Owner: @wqyjh + contributors

Last updated: 2026-02-19

---

## Table of Contents

1. Goals & Non‑Goals
2. Core Model (First Principles)
3. Data Model & Persistence
4. Runtime Semantics
   - Internal vs external delivery
   - Announce (meta payload, B)
   - Queue: steer‑backlog only
5. AgentSpec (model/skills/tools/context)
6. Modules Impact Map (what changes where)
7. Detailed Implementation Plan (milestones + acceptance)
8. Appendix: Prior art notes (OpenClaw references)

---

## 1. Goals & Non‑Goals

### 1.1 Goals

- **True multi‑agent**: multiple agent profiles (model + skills/tools policy + context policy) that can be switched per session.
- **Subagent = child session**: independent transcript + execution, but no external delivery by default.
- **Stable orchestration**: parent can run long tool loops and still respond to new user messages (steer‑backlog).
- **Minimal persistence noise**: keep chat transcripts readable; encode rich machine data in `meta_json`.
- **Reduce complexity first**: refactor memory to **sqlite‑only** early.

### 1.2 Non‑Goals

- Implementing additional queue modes (collect/followup/interrupt) beyond steer‑backlog.
- Agent‑isolated memory stores. Memory is global; agents differ by model/skills/tools/context policy.

---

## 2. Core Model (First Principles)

Multi‑agent requires three primitives:

- **Session (state)**: persistent transcript + per‑session config.
- **AgentSpec (capability)**: model defaults + skills/tools/context policies.
- **TurnJob (scheduling)**: one queued unit that executes exactly one session turn.

Everything else ("subagents", "announce", optional "lanes") is built from these.

### 2.1 Hard rules (system mechanics)

- **Per‑session serialization**: at most 1 active TurnJob per session.
- **Internal execution ≠ external delivery**: internal events default to `deliver=false`.
- **Ephemeral context**: recall/context blocks are generated per turn and are **not** stored as chat messages.
  - Rationale: prevents transcript bloat, avoids turning recall into permanent facts, keeps retrieval/debugging sane.
- **Loop prevention**: idempotency + TTL/hop + no self‑send.

---

## 3. Data Model & Persistence

### 3.1 Session transcript

A session is persisted as a message sequence. Messages may carry `meta_json`.

Key fields (conceptual):

- `session_id` / `session_key`
- `active_agent_id` (which AgentSpec drives turns)
- `provider_override` / `model_override`
- `deliver_policy` (whether outbound sends are allowed)
- route metadata (channel/chat/thread/sender)

### 3.2 AgentSpec registry (new)

Persist AgentSpecs in `memory/sessions.db`:

- `agent_id` (stable)
- `name` (unique)
- `config_json` (model defaults + policies)
- timestamps

### 3.3 Memory backend decision

**Decision:** keep a single memory backend: **SQLite** (`brain.db`).

- Remove/deprecate markdown memory backend.
- Optional: export tooling (`memory export`) for audits / Git diff, but SQLite remains source of truth.

---

## 4. Runtime Semantics

### 4.1 Internal channel & deliver gating

Introduce a reserved internal channel identifier:

- `INTERNAL_MESSAGE_CHANNEL = "agent"` (or `"internal"`)

Hard rule:

- External delivery is allowed **only if** `wants_deliver==true` **AND** `resolved_channel != INTERNAL_MESSAGE_CHANNEL`.

### 4.2 Subagents are sessions

A “subagent” is simply a **child session**:

- its inputs are injected by a parent session,
- its outputs are routed back to the parent session as a minimal injected message + `meta_json` payload.

### 4.3 Announce: **B = semantic payload in meta_json**

Child→parent handoff must avoid transcript bloat.

**Persisted parent message** (short, human readable):

- `[@agent:<name>#<agent_id>] finish`

**All semantics live in meta_json** attached to that message:

- `task`: label/prompt/tags
- `result`: status/summary/artifacts
- `source`: agent_id, child session key, run_id
- `stats`: duration, tokens/cost (optional)
- `idempotency_key`, `trace_id`, `hop`

The runtime may render a compact ephemeral context block from this meta for the **next turn**, but does not store that rendered block as a chat message.

### 4.4 Queue: **steer‑backlog only**

To keep the system minimal, we support exactly one queue behavior.

**Definition (steer‑backlog):**

- If a session is running a long turn (tool loop) and a new inbound user message arrives, the runtime **steers** to the new message at the next safe tool boundary (responsive UX).
- To avoid losing work, the runtime appends a **resume token** for the interrupted task into a per‑session **backlog** so the task can continue after the steered turn completes.

Backlog persistence/format:

- Backlog is stored per session.
- On the next turn start, backlog items are drained and merged into a single `[Backlog]...` user message.

Fallback:

- If steering is not possible (no safe boundary), enqueue the new message into backlog to run after the current turn.

---

## 5. AgentSpec (model/skills/tools/context)

Agent differences must be real (not just prompt text).

### 5.1 AgentSpec fields (minimum)

- `defaults.model`: provider/model/temperature
- `defaults.system_prompt` (optional)
- `policy.tools`: allow/deny tool names
- `policy.skills`: allow/deny skill names (controls prompt injection)
- `context_policy`: memory recall on/off + budgets/filters (memory is global)

### 5.2 Session switching

- `/agents` lists available AgentSpecs
- `/agent <id|name>` switches the session’s `active_agent_id` (exact match)
- `/models` lists models
- `/model <provider>/<model>` sets session model override

---

## 6. Modules Impact Map (what changes where)

This is the review checklist: each item should have tests and clear diffs.

### 6.1 Core runtime / loop

- `src/agent/loop_.rs`
  - implement steer‑backlog semantics (safe boundaries + resume tokens)
  - consume announce meta payloads as ephemeral context (no transcript writes)

- `src/agent/agent.rs`
  - ensure tool loop exposes boundary points suitable for steering

### 6.2 Session persistence & state

- `src/session/store.rs`
  - AgentSpec registry tables
  - session state keys for `active_agent_id`, model override

- `src/session/backlog.rs`
  - backlog storage for resume tokens and queued messages
  - (optional) evolve backlog entries from plain strings → JSON payloads

### 6.3 Channels / routing

- `src/channels/mod.rs` (+ per channel)
  - route inbound messages to sessions
  - enforce deliver gating for internal channel
  - commands: `/models`, `/model`, `/agents`, `/agent`, `/queue` (steer‑backlog only)

### 6.4 Memory

- `src/memory/*`
  - sqlite‑only refactor
  - recall controlled by `context_policy`

### 6.5 Skills & tools

- `src/skills/mod.rs`
  - global discovery; per‑agent filtering for injection

- `src/tools/mod.rs` (+ tools)
  - per‑agent tool allow/deny

### 6.6 Subagents (re‑architecture)

- `src/subagent/*` + `src/tools/subagent_*`
  - migrate away from “subagent job queue” to “child sessions + announce meta payload”

---

## 7. Detailed Implementation Plan

### 7.1 Consistency checklist (use this to prevent stale sections)

Before shipping any milestone, scan the RFC for:

- **SQLite-only memory**: no markdown backend references or behavior dependencies.
- **Announce = meta_json payload (B)**: parent transcript stays minimal; semantics are in `meta_json`.
- **Ephemeral context**: any context blocks rendered from recall/announce meta are runtime-only (not persisted as messages).
- **Queue semantics = steer-backlog only**: no extra queue modes described.
- **Milestones match decisions**: acceptance criteria do not contradict the above.

Each milestone should be shippable and reviewable.

### Milestone 0 — sqlite‑only memory (complexity reduction)

Tasks:

1) config/schema: accept only sqlite backend (or default to sqlite)
2) remove/deprecate markdown memory code paths
3) optional: `memory export` tooling

Acceptance tests (behavior-level):

- Program starts with sqlite backend as the only memory backend.
- All tests pass with sqlite-only.
- No runtime flags/config paths reference markdown memory.

### Milestone 1 — AgentSpec registry + session switching

Tasks:

1) DB schema: `agent_specs` table
2) session state: `active_agent_id`, `model_override`
3) commands: `/agents`, `/agent`, `/models`, `/model`
4) turn resolution: session → AgentSpec → effective model/tools/skills/context

Acceptance tests (behavior-level):

- `/agents` lists AgentSpecs and shows current session active agent.
- `/agent <id|name>` switches `active_agent_id` for the session (exact match).
- After switching, subsequent turns use the new AgentSpec's model/tool surface.
- `/models` lists models; `/model <provider>/<model>` overrides session default for subsequent turns.

### Milestone 2 — Skills & tools filtering (make agents real)

Tasks:

1) system prompt: inject only allowed skills
2) tool registry: register only allowed tools

Acceptance tests (behavior-level):

- Switching agents changes the visible skill list and the callable tool set.
- A disallowed tool call is rejected (or the tool is absent) even if the model attempts it.
- Skill injection and tool registration are consistent (no "listed but unusable" or "hidden but callable").

### Milestone 3 — steer‑backlog correctness

Tasks:

1) define safe tool boundaries
2) steering: new message preempts long tool loop at boundary
3) backlog resume token: interrupted task continues after steered turn
4) backlog drain: merged into a single `[Backlog]...` user message

Acceptance tests (behavior-level):

- Start a long-running request that requires many tool calls.
- While the tool loop is active, send a new user message:
  - the runtime steers to handle the new message at the next safe boundary.
  - the interrupted work is preserved via a backlog resume token.
- After responding to the new message, the original task resumes from backlog and completes.

### Milestone 4 — Subagents as child sessions + announce meta payload (B)

Tasks:

1) spawn child session (internal channel, deliver=false)
2) parent→child injection uses internal channel
3) child→parent announce:
   - inject minimal parent message
   - attach semantic payload in meta_json
4) parent consumes announce meta as ephemeral context next turn

Acceptance tests (behavior-level):

- Parent can spawn N subagent sessions in parallel.
- Child sessions do not deliver externally by default.
- Each child completion produces a minimal parent-session message plus `meta_json` payload (task/result/artifacts).
- Parent can synthesize a user-visible response using those announce payloads without persisting any rendered context blocks into the transcript.

### Milestone 5 — Hardening (idempotency/TTL/observability)

Tasks:

- deterministic idempotency keys for announce
- TTL/hop enforcement
- basic tracing for steering/backlog decisions

Acceptance:

- no duplicate announces
- no self‑send loops

### Milestone 6 — Cleanup

Tasks:

- remove legacy subagent job queue paths
- remove legacy delegate mappings

Acceptance:

- only Session + AgentSpec + TurnJob remain as core primitives

---

## 8. Appendix: Prior art notes (OpenClaw references)

OpenClaw is useful as prior art for:

- internal channel + deliver gating
- steer‑backlog behavior
- subagent announce flow

(Reference only; Zeroclaw design above is first‑principles + minimal.)
