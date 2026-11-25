# Phase 0 真机核验报告（macOS arm64）

- **核验环境**：`macOS 26.5.2 (arm64)` · `Python 3.14.4`
- **已探测客户端版本**：
  - **OpenAI Codex**：初测为 `codex-cli 0.154.0`，后续本机升级至 `codex-cli 0.172.0`（`/opt/homebrew/bin/codex`，独立应用包已基于 `0.172.0` 复测）
  - **Claude Code**：本机未安装客户端或 CLI，仅保留历史日志（日志版本 `2.1.278`，`~/.claude/projects/` 存在 1 份 JSONL 及 `~/.claude/ide` 目录）
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
5. **Phase 0b 主动查询（用户提供的 macOS 真机结果）**
   - Codex 0.154.0 的 app-server 通过 stdio 完成初始化。无需模型请求，`account/read`（`refreshToken: false`）返回当前 ChatGPT 账号及 Plus 套餐，耗时 6 ms。`account/rateLimits/read` 耗时 1562 ms。
   - 返回的 7 天窗口已用 16%，用户核对官方显示剩余 84%。该样本未返回第二个窗口。后续桌面对照中，两端均显示剩余 76%，官方 CLI 显示 11 月 29 日 14:57，与面板重置时间 14:57:17 一致（官方精度为分钟）。
   - 后续跨终端测试完成 3 次回复，日志的单次 Token 分别为 19,895、19,908、19,926。面板游标停在 55,239 字节，文件已增长到 122,507 字节。三条用量行结束于 101,395、111,377、121,415 字节，均已换行，时间戳带时区，原库中对应请求行数均为零。旧版已发现该文件但没有继续读到用量行。旧版未记录当时的 FSEvents 类别，无法还原具体漏掉的回调。
   - **2026-11-25 macOS 真机复测（`038f327`）**：启动面板后，历史补读基线为 59,729 Token。另一终端新建会话并发起一次请求，面板今日增量 19,618，最近一次响应 19,618，符合只计一次的预期。WebView 可见帧采样时间为 19:41:51.538（UTC+8）。事件到界面显示的延迟上界为 **538 ms**（事件时间向下取整至秒），该样本通过 2 秒目标。
   - 此次 macOS 监听跟踪实际收到相同新 JSONL 路径的 `Create(File)`、`Modify(Metadata(Any))`、`Modify(Metadata(Extended))`，三条均为 `rescan=false`。三类均进入现有路径队列。
   - 离线快照按额度事件自身的时间排序。旧日志追加设置记录也会改变文件修改时间。
6. **macOS 独立应用包（`Agent Use.app`）测试记录（2026-11-25，基于 `codex-cli 0.172.0`）**
   - **构建**：`npm run tauri build` 生成 arm64 原生 `target/release/bundle/macos/Agent Use.app`。
   - **测试 A — 隔离首次启动（极简 `PATH`，直接运行包内可执行文件）**：
     - 条件：`PATH=/usr/bin:/bin:/usr/sbin:/sbin` + `mktemp -d` 临时 `AGENT_USE_DATA_DIR`，直接启动 `Agent Use.app/Contents/MacOS/agent-use`（不改动用户原有数据库）。
     - 验证范围：确认在无终端 `PATH` 下能新建独立 `monitor.sqlite`，并完成一次性发现——`codex`（`installed`，`/opt/homebrew/bin/codex`）、`antigravity`（`installed`）、`claude`（`data`），确认选择前未启用平台且未读取用量。
   - **测试 B — 终端 `open` 启动正式包（复用已有数据库，注意环境边界）**：
     - 条件：在无 `tauri dev` / Vite 进程状态下执行 `open "target/release/bundle/macos/Agent Use.app"`；因 macOS 的 `open` 可能继承调用终端环境，该项仅验证正式包本身可恢复已有配置/历史、调用 `codex app-server` 完成无模型请求额度查询，以及退出重开后已统计响应无重复计入，**不单独等同于从访达双击后的极简 `PATH` 额度查询验收**。
     - 当次观测快照（仅作当时记录，后续实测以面板实时基线为准）：已有配置 `enabled = ["codex", "antigravity"]` 保留，启动后 `checked_at` 更新（`20:01:13` → `20:14:29` → 退出重开后 `20:14:37`），返回 `t***@outlook.com`、`PLUS`、7 天额度已用 `38%`（界面显示剩余 `62%`）、重置时间 `2026-11-29 14:57:17+08:00`；退出重开后已统计响应数与总数保持不变。
   - **开发版实测 vs 正式包待用户从访达双击实测项**：
     - 此前开发版（`verify:codex`）两次单响应终端测试的界面更新延迟上界为 `538 ms` 与 `484 ms`；正式包不加载验收探针，延迟记为「延迟未测」。
     - 正式包尚待用户从访达实际双击完成最后三项真机验收：① 点「刷新额度」确认查询成功时间更新并与官方额度/重置时间对照；② 本机 Codex 发一条消息验证自动刷新并与该轮日志响应总和核对增量；③ `⌘Q` 退出后再发一条消息、重新双击打开验证补读且再次重开无重复。


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
4. **当前验证范围说明（Claude Code 未测原因）**：
   - 本机未安装 `claude` CLI 或 Claude Code 客户端（`claude` 不在 `PATH` 及常见安装目录中），仅保留历史 JSONL 日志（1 份，13 条去重后响应，累计 461,956 Token）。
   - 现有 `agent-core` 已覆盖 Claude Code 的 JSONL 解析、按 `(sessionId, message.id)` 去重与缓存字段包含口径单测（`claude_updates_message_and_includes_cache` 通过），本轮保留该功能并跳过真机会话与 `/usage` 验收，无需额外安装 Claude Code。

---

### 1.3 Google Antigravity（Desktop App `2.35.1`）— 数据来源专项调查结论

- **核验对象**：本机 `/Applications/Antigravity.app`（`com.google.antigravity`，`2.35.1`）及官方文档 `https://antigravity.google/llms.txt`、`https://antigravity.google/docs/cli/commands/usage.md`、`https://antigravity.google/docs/cli/reference.md`、`https://antigravity.google/docs/cli/statusline.md`、`https://antigravity.google/docs/plans.md`、`https://antigravity.google/docs/cli/headless.md`、`https://antigravity.google/docs/cli/acp.md`。
- **本机安装事实**：
  - 仅安装 Desktop App `2.35.1`；`agy` / `antigravity` CLI **未安装**（`~/.gemini/antigravity-cli` 不存在）；
  - `/Applications/Antigravity.app/Contents/Resources/bin` 仅含内部组件（`agy-desktop-helper`、`agy-desktop-rtc`、`agy-desktop-script`、`cable_ble_scanner`、`language_server`），`~/Library/Application Support/Antigravity/bin` 仅含 `agy-node`，均非面向外部调用的官方只读查询 CLI；
  - 内置本地 `ConversationService`（Connect HTTP+JSON）仅在 Agent 自身执行环境注入 `ANTIGRAVITY_LS_ADDRESS` / `ANTIGRAVITY_CSRF_TOKEN`，且仅暴露会话管理方法（Create / List / Get / Update / Delete），不返回账号、额度池、剩余额度、重置时间或 Token 汇总。
- **逐项核对结果（不读取浏览器 Cookie、不提取客户端私有凭据、不发起模型请求）**：

| 核对项 | 客户端版本 | 排查来源 | 是否需要授权 | 实际读到的结果与结论 |
| :--- | :--- | :--- | :--- | :--- |
| **账号归属** | Desktop App `2.35.1` | 官方 CLI/App 文档、App bundle、本地目录 | — | **暂不可查**。Desktop App 无公开只读接口；官方仅在 `agy` CLI 的自定义 `statusLine` 回调 JSON 中提供 `email` / `plan_tier`（需安装并运行交互式 `agy` CLI，且需写入 `~/.gemini/antigravity-cli/settings.json`，本机未安装 `agy`）。 |
| **模型或额度池** | Desktop App `2.35.1` | `/docs/cli/commands/usage.md`、`/docs/plans.md`、`/docs/models.md` | — | **额度池暂不可查**。官方文档说明模型按组共享额度池（如 Gemini 组、第三方 Claude/GPT 组），但仅在 `agy` CLI 交互命令 `/usage`（`/quota`）或桌面端 Settings UI 查看，无公开非交互 CLI 参数或只读 API。（注：本地对话日志含单步 `model` 名，不含账户额度池映射。） |
| **剩余额度** | Desktop App `2.35.1` | `/docs/cli/commands/usage.md`、`/docs/cli/credits.md`、`/docs/cli/reference.md` | — | **暂不可查**。消费级套餐无公开只读 REST/RPC 接口，也无 `agy --usage` 类非交互查询参数；仅 `agy` CLI 交互态 `/usage`、`/credits`、状态栏或桌面端 UI 人工可见（`agy` CLI `statusLine` 回调含 `quota.<bucket>.remaining_fraction`，但仅限 CLI 运行态被动推送）。 |
| **重置时间** | Desktop App `2.35.1` | `/docs/plans.md`、`/docs/cli/commands/usage.md`、`/docs/cli/statusline.md` | — | **暂不可查**。官方文档仅说明 Pro/Ultra 按 5 小时滚动刷新至周上限、Free/Plus 为周限额；具体重置时间仅在官方 UI 人工可见（或 `agy` CLI `statusLine` 的 `quota.<bucket>.reset_time`）。 |
| **Token 明细** | Desktop App `2.35.1` | 本机 `~/.gemini/antigravity/brain/<conv-id>/.system_generated/logs/transcript.jsonl` + `/docs/cli/statusline.md` | 无需额外授权（本地只读文件） | **发现本地候选日志，但本轮未接入主卡片**。实测本机 4 个对话的 `.system_generated/logs/transcript.jsonl` 共含 106 条 `type: "PLANNER_RESPONSE"`、`status: "DONE"` 记录（`step_index` 在各对话内无重复），字段包含 `created_at`、`model`、`input_tokens`、`cache_read_tokens`、`output_tokens`，且连续轮次上下文增长验证 `cache_read_tokens` **不含于** `input_tokens`；官方文档 `statusline.md` 亦引用该 `transcript_path`。但该文件位于 `.system_generated/` 内部目录、未作为公开稳定统计契约文档化，且 Desktop App UI 未提供可交叉核对的官方 Token 总量（亦未明确 `output_tokens` 是否含思考 Token 或覆盖全部客户端活动），因此本轮暂不接入主卡片，保留为后续核对候选。 |

- **本轮产品处理结论**：Antigravity 卡片继续展示本机安装发现状态（`已检测到`），明确写清「账号归属、模型/额度池、剩余额度、重置时间与官方 Token 明细暂不可查」；在确认受支持的只读授权能够获取额度数据前，不设计 Google 账号绑定。

---

## 2. 实测更新后的能力矩阵摘要

| 平台 | 能力项 | 验证状态 | 实测结论 / 待验证说明 |
| :--- | :--- | :--- | :--- |
| **Codex** (`0.154.0`) | 本次请求 Token | **指定版本已实测** | 来自 `token_usage_record.payload.usage`，去重键 `(session_id, response_id)`；`cached_input` 含于 `input`，`reasoning_output` 含于 `output` |
| **Codex** (`0.154.0`) | 累计 Token | **指定版本已实测** | 来自 `event_msg:token_count.info.total_token_usage`，或按 `token_usage_record` 聚合 |
| **Codex** (`0.154.0`) | 套餐（Plan） | **指定版本已实测** | 来自 `event_msg:token_count.rate_limits.plan_type`（样例值 `plus`） |
| **Codex** (`0.154.0`) | 额度百分比 & 重置时间 | **指定版本已实测**（日志结构）；百分比 **已核对**，重置时间 **已核对至分钟** | 来自 `rate_limits.primary` / `secondary`，按 `window_minutes`（300 / 10080）动态分类，`resets_at` 为 Unix 秒级时间戳 |
| **Codex** (`0.154.0` / `0.172.0`) | 当前账号 & 主动刷新 | **指定版本已实测** | app-server 的 `account/read` 与 `account/rateLimits/read` 在无模型请求时返回当前账号与额度；正式包已分别验证极简 `PATH` 下 CLI 发现与 `open` 启动查询，待访达双击最终确认 |
| **Claude Code** (`2.1.278`) | 本次请求 & 累计 Token | **指定版本已实测**（历史日志 + 单测） | 来自 `~/.claude/projects/**/*.jsonl` 的 `assistant` 记录，按 `(sessionId, message.id)` 去重；`cache_read`/`cache_creation` 不含于 `input_tokens`。本机未安装客户端，本轮跳过真机会话验收 |
| **Claude Code** (`2.1.278`) | 账号 / 套餐 / 额度 / 重置 / 主动刷新 | **未测（未安装）** | 本机未安装 `claude` CLI / 客户端，本轮无需安装 |
| **Antigravity** (`2.35.1`) | 账号 / 模型池 / 剩余额度 / 重置时间 | **已调查 · 暂不可查** | Desktop App `2.35.1` 无受支持的只读外部接口；`/usage` 仅限交互 UI，`statusLine` 仅限未安装的 `agy` CLI 运行态回调 |
| **Antigravity** (`2.35.1`) | Token 明细 | **已调查 · 发现候选日志未接入** | 本机 `.system_generated/logs/transcript.jsonl` 含 `PLANNER_RESPONSE` token 字段（`cache_read_tokens` 不含于 `input_tokens`），但属内部生成日志且无桌面端官方总量可核对，本轮不接入主卡片 |
