# Phase 0 真机核验报告（macOS arm64）

- **核验环境**：`macOS 26.5.2 (arm64)` · `Python 3.14.4`
- **已探测客户端版本**：
  - **OpenAI Codex**：`codex-cli 0.154.0`（CLI 在 `PATH` 中，已采集 8 个 session JSONL 统计及 2 份最新会话脱敏样例）
  - **Claude Code**：日志记录版本 `2.1.278`（`claude` 未在默认 `PATH` 中，但 `~/.claude/projects/` 存在 1 份 JSONL 及 `~/.claude/ide` 目录）
  - **Google Antigravity**：Desktop App `2.35.1`（`com.google.antigravity`，`Antigravity` 与 `language_server` 进程运行中；`agy` / `antigravity` CLI 未在默认 `PATH` 中，存在 `~/.gemini/antigravity` 与 `~/Library/Application Support/Antigravity/bin`）

---

## 1. 关键实测发现（直接修正 Phase 1 实现设计）

### 1.1 OpenAI Codex（`codex-cli 0.154.0`）— JSONL 结构已实测验证
1. **双事件去重规则（防 2 倍重复统计）**：
   - 每次请求结束会在 1ms 内连续写入两条记录：`token_usage_record`（含 `turn_id`、`session_id`、`response_id` 与单次 `usage`）和 `event_msg:token_count`（含 `total_token_usage`、`last_token_usage` 与 `rate_limits`）。
   - **采集契约**：Token 账单仅以 `token_usage_record` 为唯一单次增量来源，幂等去重键为 `(session_id, response_id)`；`event_msg:token_count` 仅用于提取 `rate_limits` 配额快照与校对会话累计值，**绝不重复累加 Token**。
2. **Token 包含关系已实测确认**：
   - 样例核对：`input_tokens (157446) + output_tokens (402) == total_tokens (157848)`，且 `cached_input_tokens (156928) <= input_tokens`、`reasoning_output_tokens (168) <= output_tokens`。
   - **结论**：Codex 的 `cached_input_tokens` **已包含在** `input_tokens` 内，`reasoning_output_tokens` **已包含在** `output_tokens` 内；计算非缓存输入时应为 `input_tokens - cached_input_tokens`。
3. **模型归属支持单会话中途切换**：
   - 样例 File 1 在同一会话内从 `gpt-6.2-sol` 切换至 `gpt-6.2-astra`；每个 `turn_context` 事件携带 `(turn_id, model)`，需按 `turn_id` 关联到对应的 `token_usage_record`。
4. **配额窗口不能写死 `primary = 5h`**：
   - 样例 File 0（2026-10-11）包含 `primary (window_minutes: 300)` 与 `secondary (window_minutes: 10080)`；
   - 样例 File 1（2026-11-20）仅包含 `primary (window_minutes: 10080)` 且无 `secondary`。
   - **结论**：必须按 `window_minutes` 动态识别窗口类型（`300 -> 5 小时窗口`，`10080 -> 7 天周窗口`），不可硬编码字段名语义。
5. **待补充核验项（Phase 0b）**：
   - 账号标识与无新会话时的主动刷新：需核验 `codex login status` / `codex doctor` 输出及官方客户端同屏显示值对照。

---

### 1.2 Claude Code（日志版本 `2.1.278`）— JSONL 结构已实测验证
1. **强去重键必须为 `(sessionId, message.id)`（防 2–3 倍重复统计）**：
   - 实测样例中，同一轮回复会连续写入 2–3 条 `type: "assistant"` 记录，每条拥有**不同的 `uuid`**，但共享**相同的 `message.id`** 与相同的 `usage`（例如第 47、48、49 行共享 `message.id = hash:eaaf7ff30b`）。
   - **结论**：严禁按 `uuid` 或行号累加，必须按 `(sessionId, message.id)` 执行 Upsert 取终态。
2. **Token 包含关系与 Codex 相反**：
   - 实测样例第 23 行：`input_tokens = 227`，`cache_read_input_tokens = 31488`，`output_tokens = 381`。
   - **结论**：Claude Code 的 `cache_read_input_tokens` 与 `cache_creation_input_tokens` **不包含在** `input_tokens` 内；总输入 Token 为三者之和。
3. **非默认 PATH 与第三方模型路由兼容**：
   - 本机 `claude` 不在默认 `PATH`（存在 `~/.claude/ide`），且样例模型为 `deepseek-v4-pro` / `deepseek-v4-flash`（第三方模型路由场景）。
   - **结论**：不能把 `claude` CLI 在 `PATH` 当作检测前提；未命中官方价目表的第三方模型成本必须留空（`NULL` / `—`），且不可默认将第三方模型会话当作 Anthropic 订阅配额消耗。
4. **待补充核验项（Phase 0b）**：
   - `cost-state` 事件字段结构；官方 `/usage` 非交互读取与账号/额度显示值对照。

---

### 1.3 Google Antigravity（Desktop App `2.35.1`）— 运行形态已确认，数据入口仍待验证
1. **已确认事实**：
   - 本机以 macOS 桌面应用形式安装（`com.google.antigravity` `2.35.1`），运行中进程包含 `Antigravity` 与 `language_server`；默认 `PATH` 中无 `agy` 或 `antigravity` CLI，存在 `~/.gemini/antigravity` 与 `~/Library/Application Support/Antigravity/bin`。
2. **严守授权边界**：
   - 仅发现 `language_server` 进程和本地目录不能证明第三方获得了查询授权；在未核验 `bin/` 下官方 CLI 或受支持只读状态入口前，Antigravity 的账号、模型池配额、重置时间与 Token 明细均保持 **「待验证」** 状态，缺省允许留空。

---

## 2. 实测更新后的能力矩阵摘要

| 平台 | 能力项 | 验证状态 | 实测结论 / 待验证说明 |
| :--- | :--- | :--- | :--- |
| **Codex** (`0.154.0`) | 本次请求 Token | **指定版本已实测** | 来自 `token_usage_record.payload.usage`，去重键 `(session_id, response_id)`；`cached_input` 含于 `input`，`reasoning_output` 含于 `output` |
| **Codex** (`0.154.0`) | 累计 Token | **指定版本已实测** | 来自 `event_msg:token_count.info.total_token_usage`，或按 `token_usage_record` 聚合 |
| **Codex** (`0.154.0`) | 套餐（Plan） | **指定版本已实测** | 来自 `event_msg:token_count.rate_limits.plan_type`（样例值 `plus`） |
| **Codex** (`0.154.0`) | 额度百分比 & 重置时间 | **指定版本已实测**（日志结构）；同屏数值对照 **待验证** | 来自 `rate_limits.primary` / `secondary`，按 `window_minutes`（300 / 10080）动态分类，`resets_at` 为 Unix 秒级时间戳 |
| **Codex** (`0.154.0`) | 当前账号 & 主动刷新 | **待验证** | 需 Phase 0b 核验 `codex login status` / `codex doctor` 及无新会话刷新方式 |
| **Claude Code** (`2.1.278`) | 本次请求 & 累计 Token | **指定版本已实测**（JSONL 结构） | 来自 `~/.claude/projects/**/*.jsonl` 的 `assistant` 记录，必须按 `(sessionId, message.id)` 去重；`cache_read`/`cache_creation` 不含于 `input_tokens` |
| **Claude Code** (`2.1.278`) | 账号 / 套餐 / 额度 / 重置 / 主动刷新 | **待验证** | 本机 `claude` 不在默认 `PATH`，且存在第三方模型路由场景 |
| **Antigravity** (`2.35.1`) | 全部字段（账号 / 模型池 / 额度 / 重置 / Token） | **待验证** | 已确认 Desktop App `2.35.1` 运行形态；受支持的官方只读查询入口待 Phase 0b 核验 |
