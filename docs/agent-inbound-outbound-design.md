# Unified Agent Inbound/Outbound and Internal Channel (Design)

**Status:** Design proposal  
**Scope:** Agent lifecycle, session runner, steer-backlog, subagent↔parent communication  
**Related:** [MULTI_AGENT_SYSTEM_DESIGN.md](./MULTI_AGENT_SYSTEM_DESIGN.md) (M4/M5 announce, internal channel gating)

---

## 1. Goal

- **Unify** agent inbound and outbound: at initialization time, each agent knows where messages come from (inbound) and where replies go (outbound).
- **Each agent has exactly one internal queue** as its sole inbound; steer-backlog is implemented **inside** the agent by that queue, not by a separate session-level backlog.
- **Subagent↔agent** communication goes over a **dedicated internal channel** (first-class abstraction). External and internal “sources” both feed the **same** agent queue so that one session still has **one inbound (one queue)** and **one outbound (one reply channel)**.

---

## 2. Current State (Brief) — and why it’s wrong

- **Inbound:** Messages arrive as `ChannelMessage`; dispatch loop resolves `session_key` and sends to a per-session `tx`. The **same** `msg` is later used for outbound (reply_target, channel).
- **Outbound:** In `run_session_turn`, outbound is derived from the **inbound** message: `target_channel = ctx.channels_by_name.get(&msg.channel)`, `reply_target = msg.reply_target`.
- **Steer-backlog:** Implemented at **session_key** level (e.g. `session::backlog::enqueue(session_key, ...)`), and the session runner passes a `steer_at_checkpoint` closure that drains that global backlog. So the “queue” is not owned by the agent; the agent loop is driven by the runner and receives “current message” from outside. **This is wrong:** steer-backlog should be the agent’s **own** queue: at checkpoints the agent drains **its** queue; if non-empty, it steers (merge drained as one user message, push the interrupted message onto the queue, run AI, then process the queue again).
- **Internal:** `INTERNAL_MESSAGE_CHANNEL` is a string and a gating flag; subagent→parent is store append + ephemeral context. No dedicated internal channel object.

Correct flow: **process_channel_message** must **not** directly call `run_tool_call_loop`. It should: resolve `session_key` → get or create **agent instance** → **enqueue the message into that agent’s internal queue**. The agent’s loop is the only thing that consumes from that queue and runs the tool-call loop; at checkpoints it checks and drains its own queue for steer-backlog.

---

## 3. Unified Model

### 3.1 One agent = one internal queue (inbound) + one outbound

- **Inbound:** Exactly one **internal queue** per agent instance. The agent’s run loop is the **only** consumer of this queue. All messages that are “for this session” eventually land in this queue.
- **Outbound:** Exactly one reply channel (sink). Root agent → external channel + reply_target; child agent → internal channel to parent (or designated session).

So: **one session = one queue (inbound) + one channel (outbound)**. No “two inbounds” from the agent’s point of view.

### 3.2 Two producers, one queue (resolving “two sources”)

A session can receive messages from:

1. **External channel** (gateway): user messages, resolved by `session_key` to this session.
2. **Internal channel**: e.g. subagent announce to parent, or parent→child task.

Instead of giving the agent “two inbounds,” we keep **one queue** and **two producers** that push into it:

- **External producer:** The gateway dispatch (e.g. `process_channel_message`) resolves `session_key` → get/create agent → **push `ChannelMessage` (or an envelope) into that agent’s queue**.
- **Internal producer:** When the internal channel delivers a message to this session (e.g. announce, task), the internal channel implementation **pushes** that message (as an envelope) into **the same agent’s queue**.

From the agent’s perspective there is only **one** source: “my queue.” The agent never needs to know whether an item came from Telegram or from another agent; it just processes the queue in order. So **one session = one reply channel** is preserved, and we avoid the inelegance of “two inbounds.”

### 3.3 Message flow (no direct run_tool_call_loop from process_channel_message)

1. **Gateway** receives external message → `process_channel_message(ctx, msg)`.
2. **process_channel_message** resolves `session_key` from `msg`, then **get_or_create_agent(session_key)**. It does **not** call `run_tool_call_loop`. It **enqueues** `msg` (or a normalized work item) into **that agent’s internal queue** and returns.
3. **Agent run loop** (single consumer of the agent’s queue):
   - Waits for at least one item from its queue (or processes existing backlog).
   - Pops one “turn” (see steer-backlog below: at checkpoints it may merge multiple queued items).
   - Loads session state, builds history, runs **run_tool_call_loop**.
   - At **checkpoints** inside the tool loop: **check the agent’s own queue**. If there are new messages:
     - **Drain** the queue, merge into one user message (e.g. concatenate), and present that as the **new** user message to the model (steer).
     - **Push the current (interrupted) user message** onto the queue so it is processed later (backlog).
     - Run the model on the merged “steered” content; when the model replies, deliver to outbound, then **continue** by processing the queue again (the backlog).
   - So steer-backlog is entirely implemented by the agent’s internal queue and checkpoint logic.

### 3.4 Steer-backlog (correct semantics)

- **Checkpoint:** At a safe boundary in the tool-call loop (e.g. after a tool round, before the next LLM call), the agent **checks its internal queue**.
- **If queue is non-empty:**  
  - Drain queue and **merge** drained items into **one** user message (e.g. `[Backlog]\n...` or ordered concatenation).  
  - **Save the current user message** (the one that was being processed) by **pushing it back onto the queue** (backlog).  
  - Send the **merged** message to the model (steer to the new content).  
  - On response: deliver to outbound, then **process the queue again** (so the previously interrupted message and any others are handled in order).
- **If queue is empty:** Continue the current turn as usual.

So the **only** queue involved in steer-backlog is the **agent’s internal queue**. No separate session_key-level backlog; the agent owns the queue and the semantics.

### 3.5 Dedicated internal channel

- **Role:** Carry messages **between agents** (parent↔child). Delivery to a session = **push an envelope into that agent’s queue** (when the agent exists); otherwise persist (e.g. append to session store) so the agent sees it when it next runs.
- **Semantics:** Typed envelopes, e.g. `InternalMessage { from_session_id, to_session_id, kind, payload, meta }` (`Task`, `Announce`, `Cancel`, …). Internal channel implementation routes by `to_session_id`: if that session has an agent with a queue, push to that queue; else store and/or notify when the agent is created.
- So: **one session = one queue (inbound) + one outbound**. The queue has two producers (external + internal); the agent sees a single ordered stream.

---

## 4. Design implications

### 4.1 Agent lifecycle and queue ownership

- When we **get-or-create a session** (by session_key), we **get-or-create an agent instance** that **owns** an internal queue (inbound).
- The agent is constructed with:
  - **Inbound:** The agent’s **own** internal queue (single consumer = this agent’s run loop). External dispatch and internal channel both **push** into this queue (two producers, one queue).
  - **Outbound:** For root sessions: external channel + reply_target (set/updated when the session is bound to a chat). For child sessions: internal channel to parent.
- The **agent run loop** is the only code path that calls `run_tool_call_loop`; it consumes from the agent’s queue and, at checkpoints, drains the queue for steer-backlog. There is no “session runner” that owns a separate rx and invokes the turn: the agent owns the queue and the loop.
- Tools get “current session” / “where to send” from the agent (outbound or session_id) at construction time, not from a global map.

### 4.2 Internal channel implementation options

- **Option A – In-memory only:** Internal channel resolves `to_session_id` to the agent’s queue sender (if the agent exists) and pushes. **Downside:** parent must have an active agent loop to receive; otherwise we need fallback (e.g. store append).
- **Option B – Store-backed:** Internal channel “delivery” = append to target session transcript (as today) and/or push to that session’s agent queue if it exists. So “append to parent session + meta” is one implementation of internal channel delivery; when the parent agent runs, it can load from store or from its queue.
- **Option C – Hybrid:** If target session has an agent with a queue, push to that queue; else persist to store. When the agent is later created, it drains from store (or is notified) so it sees the message. Avoids a global map that never shrinks (queue senders are owned by the agent and dropped when the agent is dropped).

Recommended direction: **Option B or C** so that parent does not need to be “live” to receive an announce; the internal channel abstraction hides whether we used in-memory push or store append.

### 4.3 Unification of “where to send”

- **Root agent:** Outbound is “external channel + reply_target”. That can be held in the agent (or in the session’s route metadata) and set/updated when the session is bound to a chat (e.g. first message of the conversation). So at init we have either a default or “unknown”; on first inbound we set outbound = (channel, reply_target) for that conversation.
- **Subagent:** Outbound is always “internal channel → parent_session_id”. So at init we know it; no need to infer from the current message.

This gives a single mental model: **every agent has an inbound and an outbound defined at init**; for root agents, outbound can be refined on first use.

---

## 5. Modules impact (high level)

- **`src/agent/`**: Agent struct **owns** an internal queue (inbound) and holds an outbound sink. The **agent run loop** is the only consumer of the queue and the only caller of `run_tool_call_loop`; at checkpoints it drains the queue for steer-backlog (merge, push current to queue, steer, then process queue again).
- **`src/channels/mod.rs`**: **process_channel_message** does **not** call `run_tool_call_loop`. It resolves `session_key` → get_or_create_agent(session_key) → **enqueue** the message into that agent’s internal queue and returns. No “session runner” that holds rx and invokes run_session_turn; the agent owns the queue and runs its own loop.
- **`src/channels/` or `src/internal_channel/`**: Internal channel abstraction: `InternalMessage` type, `send(to_session_id, message)`. Delivery = push into that session’s **agent queue** if it exists, else persist to store (and optionally notify when agent is created). No global map of all sessions; only “agent has a queue” and internal channel pushes to it when resolving to_session_id.
- **`src/subagent/`**: When creating a child session, create an agent with inbound = its own queue (fed by internal channel for tasks to this session), outbound = internal channel to parent. Subagent tools get “send to parent” from the agent’s outbound at construction.
- **`src/tools/subagent_send.rs` (and similar):** “Enqueue a task” = internal channel `send(to_session_id = child, InternalMessage::Task { … })`; the child agent’s queue receives it; the child’s run loop processes it.

---

## 6. Non-goals (for this design)

- Changing the external channel trait (`Channel`, `send`, `listen`) or the shape of `ChannelMessage` for gateway↔runtime.
- Adding new queue modes beyond steer-backlog.
- Agent-to-agent discovery or routing beyond parent/child (e.g. arbitrary session_id targeting is enough for now).

---

## 7. Summary

- **One agent = one internal queue (inbound) + one outbound.** Steer-backlog is implemented **by** that queue: at checkpoints the agent drains its queue; if non-empty, merge and steer, push current message to queue, run model, then process queue again. No separate session-level backlog; the agent owns the queue.
- **process_channel_message** never calls `run_tool_call_loop`; it resolves session_key → get/create agent → enqueue message to the agent’s queue. Only the agent run loop consumes the queue and runs the tool loop.
- **Two producers, one queue:** External channel (gateway) and internal channel (subagent↔parent) both **push** into the same agent queue. So one session still has **one** inbound (one queue) and **one** outbound (one reply channel); no “two inbounds” from the agent’s view.
- **Internal channel:** First-class path for agent↔subagent; delivery = push to target agent’s queue (or store if agent not live). Benefits: clear lifecycle, no global tx map; outbound known at init; single ordered stream per session.
