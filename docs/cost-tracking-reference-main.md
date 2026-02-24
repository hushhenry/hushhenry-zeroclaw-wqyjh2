# Cost 追踪参考：zeroclaw-labs/zeroclaw main 分支

本文档基于 `origin/main`（zeroclaw-labs/zeroclaw，commit c47fb22）整理，说明 main 上 cost 与 usage 的设计与缺口，供本分支对齐参考。

## 1. 当前工作树 vs main：Provider 是否返回 usage

- **本分支 (vk/f782-cost)**  
  - `ChatResponse` 只有 `text`、`tool_calls`，**没有 usage 字段**。  
  - 因此 **provider 目前不会返回 usage**。

- **main**  
  - `ChatResponse` 有 **`usage: Option<TokenUsage>`**。  
  - `TokenUsage`（在 `src/providers/traits.rs`）为：
    ```rust
    pub struct TokenUsage {
        pub input_tokens: Option<u64>,
        pub output_tokens: Option<u64>,
    }
    ```
  - 各 provider 从 API 响应中解析 usage 并填入 `result.usage`。

## 2. main 上 Provider 如何填充 usage

- **anthropic**：`response.usage.map(|u| TokenUsage { input_tokens: u.input_tokens, output_tokens: u.output_tokens })`，并设置 `ProviderChatResponse { ..., usage }`。
- **openai / openrouter**：`native_response.usage.map(|u| TokenUsage { ... })`，同上。
- **compatible**：从 `chat_response.usage` 或 `native_response.usage` 映射。
- **copilot / gemini / bedrock / ollama**：各自从 API 的 usage 或 usage_metadata 映射为 traits 的 `TokenUsage`。

即：main 上 **provider 会返回 usage**（在 `ChatResponse.usage` 里），类型是 traits 里的「仅 input/output token 数」的轻量结构。

## 3. main 上 cost 模块与 gateway 的衔接

- **Gateway**  
  - 若 `config.cost.enabled`，则创建 `CostTracker::new(config.cost.clone(), &config.workspace_dir)`，放入 `AppState.cost_tracker: Option<Arc<CostTracker>>`。  
  - **GET /api/cost** 使用 `state.cost_tracker.get_summary()` 返回 dashboard 用的汇总（session/daily/monthly、by_model 等）。

- **Cost 模块（与本分支一致）**  
  - `cost::TokenUsage`：带 model、input/output/total_tokens、**cost_usd**、timestamp，用于持久化与汇总。  
  - `CostTracker::record_usage(cost::TokenUsage)`：写入 session 内存并持久化到 `workspace/state/costs.jsonl`。  
  - `CostTracker::get_summary()`：聚合 session/日/月。  
  - 单价来自 `CostConfig.prices`（每百万 token 美元），用于从 token 数算出 cost_usd。

## 4. main 上尚未接通的链路：record_usage

在 main 上 **没有任何调用点** 对 `CostTracker::record_usage` 进行调用（仅 cost 模块内部测试使用）。  
即：

- Provider **会**返回 `ChatResponse.usage`（input/output tokens）。  
- Observer / runtime_trace **会**收到并记录 token 数（如 `LlmResponse { input_tokens, output_tokens }`）。  
- Gateway **有** cost_tracker 和 GET /api/cost（get_summary）。  
- 但 **没有** 在每次 LLM 调用后：用 `resp.usage` + model + `CostConfig.prices` 构造 `cost::TokenUsage` 并调用 `record_usage`。  

因此 main 上 cost 统计的「写入」仍缺失，dashboard 的 get_summary 会返回的更多是「空/零」或历史测试数据。

## 5. 本分支若要对齐 main 并打通 cost 追踪，建议步骤

1. **与 main 对齐：Provider 返回 usage**  
   - 在 **providers/traits** 中为 `ChatResponse` 增加 `usage: Option<TokenUsage>`。  
   - 在 traits 中定义 **provider 层** 的 `TokenUsage { input_tokens: Option<u64>, output_tokens: Option<u64> }`（与 main 一致，与 cost 层的带 cost_usd 的 TokenUsage 区分）。  
   - 在各 provider（anthropic、openai、openrouter、compatible 等）中从 API 响应解析 usage 并填入 `ChatResponse.usage`。

2. **Gateway 侧（若本分支有 gateway）**  
   - 与 main 一致：当 `config.cost.enabled` 时创建 `CostTracker`，放入 state，并暴露 GET /api/cost（get_summary）。

3. **打通 record_usage（main 上也没有，可一并实现）**  
   - 在 **agent 循环**（或 gateway 侧执行 turn 的路径）中，在每次 LLM 调用成功后：  
     - 从 `resp.usage` 和当前 model 得到 input_tokens、output_tokens；  
     - 从 `CostConfig.prices` 取该 model 的 input/output 单价；  
     - 构造 **cost::TokenUsage::new(model, input_tokens, output_tokens, input_$/_1M, output_$/_1M)**；  
     - 若存在 `cost_tracker`，调用 `cost_tracker.record_usage(usage)`。  
   - 这样 cost 统计才会真实写入，get_summary 才有数据。

## 6. 参考文件（main 上的关键位置）

| 内容               | 路径 |
|--------------------|------|
| ChatResponse + usage | `src/providers/traits.rs` |
| Provider 层 TokenUsage | `src/providers/traits.rs` |
| Anthropic 填 usage   | `src/providers/anthropic.rs`（parse 处） |
| OpenAI / OpenRouter 填 usage | `src/providers/openai.rs`, `openrouter.rs` |
| Gateway cost_tracker 创建 | `src/gateway/mod.rs`（CostTracker::new） |
| GET /api/cost        | `src/gateway/api.rs`（handle_api_cost） |
| loop 中读取 resp.usage | `src/agent/loop_.rs`（resp.usage.as_ref().map(...)） |
| cost 层 TokenUsage / record_usage | `src/cost/types.rs`, `src/cost/tracker.rs` |

---

**结论**：  
- 本分支当前 **provider 不会返回 usage**（ChatResponse 无 usage）。  
- main 上 **provider 会返回 usage**，且 gateway 有 cost_tracker 与 /api/cost，但 **未调用 record_usage**，因此 main 上 cost 追踪的「统计写入」同样未接通。  
- 若要完整 cost 追踪：先与 main 对齐「Provider 返回 usage」+ gateway 侧 CostTracker/get_summary，再在 agent/gateway 的 turn 路径中增加「从 resp.usage + prices 构造 cost::TokenUsage 并 record_usage」的链路。
