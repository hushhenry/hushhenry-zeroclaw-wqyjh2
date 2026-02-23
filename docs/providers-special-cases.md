# Provider-specific behavior (ZeroAI alignment)

This document describes provider-specific handling in Zeroclaw that aligns with [ZeroAI](https://github.com/hushhenry/zeroai) behavior: Claude setup-token / session token, OpenAI Codex endpoint, and Gemini API key vs OAuth.

## Anthropic: setup-token and Claude Code (session token)

- **Auth**
  - Credentials starting with `sk-ant-oat01-` (setup-token) or containing `sk-ant-sid` (Claude Code / OAuth session) use **Bearer** in `Authorization`. Regular API keys use `x-api-key`.
  - Resolution order: config → `ANTHROPIC_OAUTH_TOKEN` → `ANTHROPIC_API_KEY`.
- **Session token (Claude Code)**  
  When the credential contains `sk-ant-sid`, the provider sends:
  - `anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14`
  - `user-agent: claude-cli/2.1.2 (external, cli)`
- **Tool names**
  - When using setup-token or session token, tool names sent to the API are mapped to **PascalCase** for known Claude Code official tools (e.g. `read` → `Read`, `bash` → `Bash`). Response tool names are mapped back to the requested tool names so the agent still sees the original IDs.

## OpenAI Codex (OAuth / ChatGPT backend)

- **Endpoint**
  - Standard OpenAI: `https://api.openai.com/v1` with `/chat/completions` or `/v1/responses`.
  - Codex OAuth backend (e.g. ChatGPT): base URL like `https://chatgpt.com/backend-api`, with path **`/codex/responses`** (not `/v1/responses`). Request/response shape follows the Responses API; tool schema may be flattened (no nested `function` wrapper).
- **In Zeroclaw**
  - There is no dedicated `codex` provider yet. To use Codex-style OAuth:
    - Run [zeroai-proxy](https://github.com/hushhenry/zeroai) and point Zeroclaw at the proxy with `custom:http://127.0.0.1:8787`, or
    - Add a future `openai-codex` provider that uses the Codex base URL and `/codex/responses` path.

## Gemini: API key vs OAuth (Cloud Code Assist)

- **API key (Generative Language API)**
  - Base URL: `https://generativelanguage.googleapis.com/v1beta`.
  - Auth: `?key=` query parameter.
  - Env: `GEMINI_API_KEY` or `GOOGLE_API_KEY`.
- **OAuth / Gemini CLI (Cloud Code Assist)**
  - Base URL: `https://cloudcode-pa.googleapis.com` (different service).
  - Auth: `Authorization: Bearer <access_token>`, with JSON payload including `projectId`.
  - Credential format: JSON like `{"token": "...", "projectId": "..."}`.
- **In Zeroclaw**
  - The **gemini** provider supports both API key and OAuth token (e.g. from Gemini CLI). When the credential is an OAuth token, it is sent as Bearer; the same Generative Language API endpoint is used. For **Cloud Code Assist** (Gemini CLI / Antigravity-style) with a different base URL and request shape, use [zeroai-proxy](https://github.com/hushhenry/zeroai) or a future `gemini-cli` provider.

## References

- ZeroAI: [anthropic.rs](https://github.com/hushhenry/zeroai/blob/main/zeroai/src/providers/anthropic.rs) (tool mapping, Bearer vs x-api-key), [openai.rs](https://github.com/hushhenry/zeroai/blob/main/zeroai/src/providers/openai.rs) (Codex `/codex/responses`), [google.rs](https://github.com/hushhenry/zeroai/blob/main/zeroai/src/providers/google.rs) vs [google_gemini_cli.rs](https://github.com/hushhenry/zeroai/blob/main/zeroai/src/providers/google_gemini_cli.rs).
