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
  - `session_state`：键值状态，含 `compaction_summary`、`compaction_after_message_id`

Session 模式下只持久化 **user** 和 **assistant**，**tool** 不入库。Memory 模式不写 session、不做 compaction。

---

## 2. 入库时机

| 消息 | 时机 | 说明 |
|------|------|------|
| **User** | 本轮开始 | 进入 `tool_call_loop_session`、确定 `user_content` 后立即 `append_message("user", ...)`；若有尾部孤立 user 则只写本条新内容，不写合并后整段。 |
| **Assistant** | 本轮结束 | 仅当最终为纯文本回复（无 tool_calls）时 `append_message("assistant", ...)`。 |
| **Tool** | 不写入 | 仅存在于内存 history。 |

`append_message` 返回新插入的 `session_messages.id`，用于压缩时写 `after_message_id`。Agent 维护 `history_message_ids: Vec<Option<i64>>` 与 `history` 一一对应，仅在已持久化的 user/assistant 上为 `Some(id)`。

---

## 3. 上下文重建（init）

- **加载一次**：Agent 初始化时从 DB 加载 `compaction_state` + `tail_messages`（`load_messages_after_id(session_id, after_message_id)`），构建初始 `history`，之后**只写不读**。
- **规范化**：对 `tail_messages` 做 **合并连续同角色**（`normalize_tail_messages`），content 用 `\n\n` 拼接，得到严格 user/assistant 交替的 tail，并保留每条逻辑消息的 last DB id 供压缩边界使用。
- **尾部孤立 user**：若 init 后 history 末尾是 user（上轮只写了 user、未写 assistant），保留；下一轮新 user 到来时见下节。

---

## 4. 尾部孤立 user + 新 user

- **内存**：若 `self.history` 最后一条已是 user，将新内容合并进最后一条（`last.content += "\n\n" + new_content`），不新增一条 `ChatMessage`；发给模型的 `user_content` 为合并后整段。
- **写库**：只 `append_message("user", new_content)`，不写合并后整段，避免下次 init 合并连续 user 时语义重复。
- **下次 init**：`load_messages_after_id` 得到 `…, user(orphan), user(new)`，normalize 后为一条 `user(orphan\n\nnew)`，与上一轮内存内容一致。

---

## 5. 压缩（in-memory）

- **输入**：当前内存 `self.history`（system + 可选 compaction summary + tail）。压缩对象是 **内存中的 tail**，不是 DB。
- **触发**：`estimate_tokens(&self.history) > SESSION_COMPACTION_AUTO_THRESHOLD_TOKENS` 或 context_exceeded 重试；手动 `/compact` 则从 DB 加载 tail 后走同一套压缩逻辑。
- **流程**：
  1. 从 history 中取 tail（去掉 system 和首条 compaction summary）。
  2. `to_compact = tail[..tail.len() - keep_recent]`，`kept_tail = tail[tail.len() - keep_recent ..]`。
  3. 用 `to_compact`（`ChatMessage`）生成 transcript，LLM 生成新 summary。
  4. 内存更新为 `[system, new_summary_msg, ...kept_tail]`；`history_message_ids` 同步截断。
  5. 写 DB：`session_state.compaction_summary`；`after_message_id` = to_compact 中**最后一条已入库消息**的 id（由 `history_message_ids` 得到），若无则保留原边界。
- **实现**：`compact_in_memory_history`（`src/session/compaction.rs`）；Agent 内通过 `run_compact_in_memory` 调用；手动 `/compact` 通过 `run_session_compaction` 先从 DB 加载再调用同一函数。

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
| User 入库 | 本轮开始；孤立 user 时只写新内容。 |
| Assistant 入库 | 本轮结束（最终纯文本回复）。 |
| Init | 从 DB 加载一次；normalize tail（合并连续同角色）；之后只写不读。 |
| 尾部孤立 + 新 user | 内存合并；写库只写 new_content。 |
| 压缩 | 基于内存 history；写 summary + after_message_id；边界仅引用已入库消息 id。 |
