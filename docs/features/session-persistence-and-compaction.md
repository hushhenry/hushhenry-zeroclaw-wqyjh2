# Session 持久化与压缩

本文描述 ZeroClaw 的 session 持久化策略与基于内存的压缩设计：存什么、何时入库、如何重建上下文、如何压缩。

---

## 1. 架构与数据表

- **Session 解析**：`SessionResolver` 将 channel 元数据规范为 `SessionKey`；thread > group > DM。
- **存储**：SQLite `workspace/memory/sessions.db`。
- **相关表**：
  - `session_index`：session_key → active_session_id
  - `sessions`：session 元信息（status, title, updated_at 等）
  - `session_messages`：消息历史（id, session_id, role, content, created_at, meta_json）
  - `session_state`：键值状态，仅存 `compaction_after_message_id`（边界）；summary 作为 **assistant** 消息写入 `session_messages`，便于 `load_messages_after_id` 查到，不丢后续对话。

Session 模式下只持久化 **user** 和 **assistant**，**tool** 不入库。Memory 模式不写 session、不做 compaction。

---

## 2. 入库时机

| 消息 | 时机 | 说明 |
|------|------|------|
| **User** | 本轮结束后统一落库 | `tool_call_loop_session` 返回 `Ok(Some((user, assistant)))` 后，由调用方一次性 `append_message("user", ...)` 和 `append_message("assistant", ...)`。user 为内存中本 turn 的最后一条约 user（可能已与尾部孤立 user 或 steer 合并）。 |
| **Assistant** | 同上 | 仅当最终为纯文本回复（无 tool_calls）时，与 user 成对落库。 |
| **Compaction summary** | 压缩发生时 | 压缩在 loop 内触发时，先将 summary 以 **assistant** 消息 `append_message("assistant", ...)` 落库，再继续 loop；这样 DB 与内存中 summary 均位于当前（尚未落库的）user 之前。 |
| **Tool** | 不写入 | 仅存在于内存 history。 |

压缩时用 `last_message_id(session_id)` 作为边界，写 `after_message_id` 到 state，再将 summary 以 assistant 消息追加。

---

## 3. 上下文重建（init）

- **加载一次**：Agent 初始化时从 DB 加载 `compaction_state` + `tail_messages`（`load_messages_after_id(session_id, after_message_id)`），构建初始 `history`，之后**只写不读**。
- **规范化**：对 `tail_messages` 做 **合并连续同角色**（`normalize_tail_messages`），得到 tail 及每条对应的 DB id（`tail_ids`），与 history 一起维护 `history_message_ids`。
- **尾部孤立 user**：若 init 后 history 末尾是 user（上轮只写了 user、未写 assistant），保留；下一轮新 user 到来时见下节。

---

## 4. 尾部孤立 user + 新 user

- **内存**：若 `self.history` 最后一条已是 user，将新内容合并进最后一条（`last.content += "\n\n" + new_content`），不新增一条 `ChatMessage`；发给模型的 `user_content` 为合并后整段。
- **写库**：本轮不立即写；`tool_call_loop_session` 返回时带出内存中最后一条约 user（即合并后整段），由调用方统一 `append_message("user", ...)`，与 assistant 成对落库。
- **下次 init**：`load_messages_after_id` 得到 `…, user(orphan\n\nnew), assistant(...)`，与内存一致。

---

## 5. 压缩（in-memory）

- **输入**：当前内存 `self.history`（system + tail）。压缩对象是 tail 中 **最后一条约 user 之前** 的消息，不压缩最近一条 user 及其之后的内容。
- **触发**：`estimate_tokens(&self.history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS` 或 context_exceeded 重试；手动 `/compact` 则从 DB 加载 tail 后走同一套压缩逻辑。
- **流程**：
  1. 从 history 取 tail，找到最后一条约 user 的下标 `last_user_idx`；若没有 user 则跳过压缩。
  2. `to_compact = tail[..last_user_idx]`，`kept_tail = tail[last_user_idx..]`（保留最后一条 user 及之后）。
  3. 用 `to_compact` 生成 transcript，LLM 生成新 summary。
  4. 写 state：`after_message_id` = 当前 DB `last_message_id(session_id)`；summary 以 **assistant 消息** 写入 `session_messages`（内容 `[Session Compaction Summary]\n{summary}`）。
  5. 内存更新为 `[system, new_summary_assistant_msg, ...kept_tail]`，summary 位于最后一条 user 之前。
- **实现**：`compact_in_memory_history`（`src/session/compaction.rs`）。`/compact` 调用链：`agent.cmd_compact` → `agent.run_compact_in_memory` → `compact_in_memory_history`。

---

## 6. 配置

```toml
[session]
enabled = true
history_limit = 40   # 压缩时保留的最近消息条数
```

---

## 7. 小结

| 项目 | 行为 |
|------|------|
| 持久化角色 | 仅 user、assistant；tool 不存。 |
| User/Assistant 入库 | 本轮结束后由调用方统一落库（`tool_call_loop_session` 返回 `(user, assistant)`，可能含合并后的 user）。 |
| Compaction summary | 以 assistant 消息落库；压缩范围是「最后一条 user 之前」的消息。 |
| Init | 从 DB 加载一次；normalize tail（合并连续同角色）；之后只写不读。 |
| 尾部孤立 + 新 user | 内存合并；写库只写 new_content。 |
| 压缩 | 基于内存 history；写 summary + after_message_id；边界仅引用已入库消息 id。 |
