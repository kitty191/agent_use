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
   - **测试 C — 访达双击启动正式包，2026-11-25，北京时间**
     - 手动点「刷新额度」后，最近成功时间从 23:17:26 更新到 23:19:09。面板显示 7 天额度剩余 57%，11 月 29 日 14:57:17 重置。官方 Codex CLI 0.172.0 的 `/status` 同样显示周额度剩余 57%，11 月 29 日 14:57 重置，双方在官方显示精度内一致。
     - 应用运行时，独立 CLI 会话的第一条响应用量为 18,700 Token。未手动刷新用量，日志与数据库中对应响应均为一条。23:20:54 的面板今日总数为 7,708,240，与截至该秒的数据库合计一致。
     - 用 ⌘Q 退出并确认进程结束后，第二条响应写入日志，计 20,878 Token。此时它在数据库中为零条，今日累计仍为 7,708,240。从访达重开后该响应入库一次，23:22:07 的面板与数据库均为 9,335,079。
     - 再次退出并从访达启动后，两条验收响应仍各一条，合计 39,578 Token。截至 23:22:07 的历史合计仍为 9,335,079。验收聊天同时产生了其他响应，因此采用响应 ID 和固定时间范围核对去重。
     - 已验证正式包的可见数字自动更新，未测量日志落盘到界面绘制的精确延迟。此前开发版两次测得的延迟上界分别为 538 ms 和 484 ms。


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
| **Token 明细（本机对话日志）** | Desktop App `2.35.1` | 本机 `~/.gemini/antigravity/brain/<conv-id>/.system_generated/logs/transcript.jsonl` + `/docs/cli/statusline.md` | 无需额外授权（本地只读文件） | **已核验口径并接入本机用量卡片**。实测 5 个对话共 158 条 `type: "PLANNER_RESPONSE"`、`status: "DONE"` 记录（仅此事件含 Token 字段），详见下方口径核验。 |

- **Antigravity 本机日志计数口径与去重核验（2026-11-25，Desktop App `2.35.1`）**：
  1. **文件过滤规则（防 4 倍重复）**：`<brain>/<conv-id>/.system_generated/logs/` 下同时存在 `transcript.jsonl`、`transcript_full.jsonl`、`chunks/transcript/*.jsonl` 与 `chunks/transcript_full/*.jsonl`。采集器显式穿透隐藏目录 `.system_generated` 且仅匹配 `<brain>/<conv-id>/.system_generated/logs/transcript.jsonl`，忽略其余副本与分片。
  2. **计数单位（步骤级增量）**：一条用户消息可产生多个 `PLANNER_RESPONSE` 步骤；`output_tokens` 在连续步骤间上下波动（如 `471 → 1470 → 115 → 1578 → 221 → 295`），`input_tokens + cache_read_tokens` 对应单次模型调用的上下文长度，证明每条 `PLANNER_RESPONSE` (`status = "DONE"`) 为**单个已完成模型步骤的增量**（非会话累计快照）。卡片标注为「最近一次模型步骤」。
  3. **缓存口径与总量公式（已实测复核）**：
     - 实测公式：**`step_total = input_tokens + cache_read_tokens + output_tokens`**。
     - 脱敏数值证据（共 158 条记录中 **147 条**满足 `cache_read_tokens > input_tokens`，排除包含可能）：
       - 对话 `31f4df9a`：`step=3`（`in=30,017, cache_read=0, out=1,470`）→ `step=5`（`in=3,701, cache_read=30,018, out=115`，`in+cache_read=33,719`，`total=33,834`）→ `step=11`（`in=2,553, cache_read=33,835, out=221`）；
       - 对话 `b9c97261`：`step=1`（`in=14,688, cache_read=0, out=558, total=15,246`）→ `step=3`（`in=3,239, cache_read=12,631, out=394, in+cache_read=15,870`）→ `step=7`（`in=2,062, cache_read=15,247, out=341, in+cache_read=17,309`）→ `step=9`（`in=7,311, cache_read=17,310, out=182, in+cache_read=24,621`）；
       - 对话 `ccc9c45b`（含 `model=null` 历史步与 Claude/Gemini 切换）：`step=4`（`in=2,053, cache_read=80,789, out=1,273`），`model` 可为空，因此解析不要求 `model` 非空。
  4. **去重键、缺失缓存、亚秒精度与旧库升级**：
     - 去重键为 `(provider="antigravity", session=<conversation_id>, response=<step_index>)`（实测 5 个对话 158 条记录内 `step_index` 0 重复）；
     - 仅计入 `status == "DONE"` 的终态记录，忽略 `RUNNING` 等中间态与缺少换行符的半行；
     - 区分 `cache_read_tokens: 0`（有效观测，正常计入完整总量）与字段缺失/`null`（保留已知分项并标为 `complete = 0`「用量不完整」，不计入完整今日总量；同一步骤补报完整值时原地升级且不翻倍，重放较早的不完整记录不覆盖已确认完整值）；
     - `created_at` 保留亚秒纳秒精度（`at_ns`），「最近一次模型步骤」按 `COALESCE(at_ns, at * 1e9) DESC` 选取；时间戳完全相同时按稳定全序并列规则 `session DESC` → 数值步号/响应 ID 降序确定，保证跨导入顺序与重启一致；今日统计仍按本地时区归日；
     - 旧库（`PRAGMA user_version < 2`）升级为 v2 时，事务内补齐列并仅将已有 `antigravity` 行标为 `legacy = 1`、仅重置 `antigravity` 游标回读一次（不碰 Codex / Claude 游标与历史）；仍在本机的日志自动补齐亚秒精度与缓存完整态（`legacy = 0`），已删除日志保留历史并如实标注旧版迁移状态。
  5. **最小脱敏样例（仅保留解析所需字段，不含正文与思考内容）**：
     ```json
     {"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T11:51:12Z","model":"gemini-4-neon-high","input_tokens":14688,"cache_read_tokens":0,"output_tokens":558}
     {"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-25T11:51:27Z","model":"gemini-4-neon-high","input_tokens":3239,"cache_read_tokens":12631,"output_tokens":394}
     ```
  6. **本机正式包（`Agent Use.app`）实测核对（2026-11-25）**：
     - 独立 Python 汇总本机 5 份 `transcript.jsonl`（忽略 `transcript_full.jsonl` 与 `chunks/`）：共 `163` 条已完成步骤（累计 `18,238,329` Token），今日（本地日期 `2026-11-25`）`75` 条步骤合计 `10,851,132` Token（`input=538,510`、`cache_read=10,177,279`、`output=135,343`），最近步骤 `272,887` Token。
     - 启动新版 `Agent Use.app` 后读取 `monitor.sqlite`（`provider = "antigravity"`）：全量 `163` 条 / `18,238,329` Token、今日 `75` 条 / `10,851,132` Token、最近步骤 `272,887` Token，游标仅记录 5 份 `transcript.jsonl`，与独立脚本 **100% 逐项一致**；退出并重开一次后总数完全不变（无重复计数）；同库中 Codex（`297` 条 / `48,071,996` Token）保持独立。
     - **新建对话实时采集（11 月 25 日，正式包）**：对照前 5 份日志共 166 个完整步骤，逐条核对数据库无差异。面板今日基线为 11,681,602 Token。用户在 Antigravity 新建第 6 个对话后，23:37:33 完成了 1 个模型步骤，输入 14,658、缓存读取 0、输出 25，共 14,683 Token。未点击刷新，正式包面板显示今日 11,696,285、最近步骤 14,683。167 个步骤与数据库逐条一致。以上时间为北京时间。未测量日志落盘至界面绘制的精确延迟。
     - **退出后补读与重启去重**：确认 Agent Use 已退出后，同一对话于 23:38:45 产生第 2 个完成步骤，输入 2,157、缓存读取 12,601、输出 22，共 14,780 Token。重开前日志有 168 个完整步骤，数据库仍为 167 条。重开后面板今日累计为 11,711,065，最近步骤为 14,780。再次退出重开，两项数字保持一致。最终 6 份日志的全部 168 个步骤与数据库按对话、步骤号、时间（秒）和 Token 总数逐条一致，今日共 80 步。
  7. **仍待确认的范围与边界**：
     - 账号归属、模型/额度池、剩余额度与重置时间仍暂不可查（不绑定 Google 账号，不要求安装 `agy` CLI）；
     - 本机日志覆盖 Antigravity 桌面端对话及子代理在 `brain/<conv-id>/.system_generated/logs/transcript.jsonl` 中落盘的规划步骤，是否包含 IDE Tab 补全、`output_tokens` 是否含思考 Token 拆分以及与官方「Work Done」配额折算关系仍待官方进一步说明。

- **本轮产品处理结论**：勾选 Antigravity 后卡片展示本机对话日志统计的「今日已记录 Token」「最近一次模型步骤」及时间，并明确注明数据来源与账号/额度/重置时间暂不可查；无日志目录时后端返回 `waiting` 状态、卡片显示「等待首次对话记录」且顶部不亮监听绿灯。

---

## 2. 实测更新后的能力矩阵摘要

| 平台 | 能力项 | 验证状态 | 实测结论 / 待验证说明 |
| :--- | :--- | :--- | :--- |
| **Codex** (`0.154.0`) | 本次请求 Token | **指定版本已实测** | 来自 `token_usage_record.payload.usage`，去重键 `(session_id, response_id)`；`cached_input` 含于 `input`，`reasoning_output` 含于 `output` |
| **Codex** (`0.154.0`) | 累计 Token | **指定版本已实测** | 来自 `event_msg:token_count.info.total_token_usage`，或按 `token_usage_record` 聚合 |
| **Codex** (`0.154.0`) | 套餐（Plan） | **指定版本已实测** | 来自 `event_msg:token_count.rate_limits.plan_type`（样例值 `plus`） |
| **Codex** (`0.154.0`) | 额度百分比 & 重置时间 | **指定版本已实测**（日志结构）；百分比 **已核对**，重置时间 **已核对至分钟** | 来自 `rate_limits.primary` / `secondary`，按 `window_minutes`（300 / 10080）动态分类，`resets_at` 为 Unix 秒级时间戳 |
| **Codex** (`0.154.0` / `0.172.0`) | 当前账号 & 主动刷新 | **指定版本已实测** | app-server 可在无模型请求时查询账号与额度。使用 0.172.0 从访达启动正式包并手动刷新，周额度百分比和分钟级重置时间均与官方 `/status` 一致。 |
| **Claude Code** (`2.1.278`) | 本次请求 & 累计 Token | **指定版本已实测**（历史日志 + 单测） | 来自 `~/.claude/projects/**/*.jsonl` 的 `assistant` 记录，按 `(sessionId, message.id)` 去重；`cache_read`/`cache_creation` 不含于 `input_tokens`。本机未安装客户端，本轮跳过真机会话验收 |
| **Claude Code** (`2.1.278`) | 账号 / 套餐 / 额度 / 重置 / 主动刷新 | **未测（未安装）** | 本机未安装 `claude` CLI / 客户端，本轮无需安装 |
| **Antigravity** (`2.35.1`) | 账号 / 模型池 / 剩余额度 / 重置时间 | **已调查 · 暂不可查** | Desktop App `2.35.1` 无受支持的只读外部接口；`/usage` 仅限交互 UI，`statusLine` 仅限未安装的 `agy` CLI 运行态回调 |
| **Antigravity** (`2.35.1`) | Token 明细（本机对话日志） | **指定版本已实测并接入** | 来自 `~/.gemini/antigravity/brain/<conv-id>/.system_generated/logs/transcript.jsonl` 的 `PLANNER_RESPONSE`（`status = "DONE"`），去重键 `(conversation_id, step_index)`，公式 `input_tokens + cache_read_tokens + output_tokens`；已忽略 `transcript_full.jsonl` 与 `chunks/` |

---

## 3. 历史用量统计核验记录（2026-11-26）

### 3.1 数据库结构与查询方式
- **数据库结构**：复用既有 SQLite `usage` 表（schema v2，`PRAGMA user_version = 2`，含 `at`、`at_ns`、`tokens`、`complete`、`input_tokens`、`cache_read_tokens`、`output_tokens`、`legacy`），本轮未新增表结构。
- **查询方式**：新增纯只读后端模块 `crates/agent-core/src/history.rs` 与 Tauri 命令 `usage_history`，直接按本地日历周期 `[start_sec, end_sec)` 查询 SQLite（不扫描文件系统、不重读日志、不联网查额度、不启用停用平台）。
- **三种口径严格分离**：主合计仅包含 `complete = 1 AND legacy = 0` 的已核验完整 Token；`complete = 0` 的不完整步骤（含三项全缺失的 `DONE` 步骤）与 `legacy = 1` 的旧版迁移值均单独列示；跨平台保留各自记录单位（Codex / Claude Code 为「次响应」，Antigravity 为「个模型步骤」）。

### 3.2 本机正式包真实历史数据核对（独立 SQLite 查询，快照时间 `2026-11-26 00:03+08:00`）
- 库状态：`PRAGMA user_version = 2`，共 3 个有历史平台（`antigravity`、`codex`、`claude`；其中 `claude` 当前未启用但历史可直接筛选查看），全部记录 `incomplete = 0`、`legacy = 0`。

| 周期维度 | 核对区间 | 平台分项（已核验完整 Token / 单位数） | 全部合计 | 核对结论 |
| :--- | :--- | :--- | :--- | :--- |
| **按日（已结束自然日）** | `2026-11-25` | OpenAI Codex：`23,355,613`（162 次响应）<br>Antigravity：`14,943,579`（90 个模型步骤） | **`38,299,192`** | 各平台之和 `23,355,613 + 14,943,579 = 38,299,192` 一致 |
| **按周（当前自然周，截至今天）** | `2026-11-23 ~ 2026-11-29` | OpenAI Codex：`23,355,613`（162 次响应）<br>Antigravity：`18,400,310`（126 个模型步骤） | **`41,755,923`** | 平台和一致；逐日和 `11-24 (1,944,989) + 11-25 (38,299,192) + 11-26 (1,511,742) = 41,755,923` 一致 |
| **按月（当前自然月，截至今天）** | `2026-11`（`11-01 ~ 11-30`） | OpenAI Codex：`28,373,534`（217 次响应）<br>Antigravity：`23,842,518`（182 个模型步骤） | **`52,216,052`** | 平台和一致；8 个有记录自然日逐日和 `2,802,890 + 1,861,184 + 2,215,031 + 1,116,958 + 2,464,066 + 1,944,989 + 38,299,192 + 1,511,742 = 52,216,052` 一致 |
| **更早自然月（跨月/停用平台）** | `2026-10` / `2026-09` | `2026-10` OpenAI Codex：`31,737,528`（149 次响应）<br>`2026-09` Claude Code（已停用）：`461,956`（13 次响应） | `31,737,528` / `461,956` | 已停用平台的历史可在「历史用量」直接筛选查询 |

### 3.3 边界与模拟测试覆盖（`cargo test -p agent-core` 15/15 通过）
- 跨日/跨周/跨月/跨年（北京时间 `2026-11-29 23:59:59`、`11-30 00:00:00`、`12-01 00:00:00` 及 `2025-12-31` → `2026-01-01`）与向前翻页；
- 同一步分次补报按字段合并并保留原日期（11 月 29 日报 `300/20` + 12 月 1 日补 `100` → 420 归入 11 月 29 日及其周、月，12 月不增加）；
- 空周期、明确零值、三项全缺失 `DONE` 步骤、旧版未核验记录四态区分；
- 停用平台历史可查且不重启采集；旧库升级与连续重开不翻倍；
- 夏令时 23 小时（春季跳时）与 25 小时（秋季回拨）自然日边界验证（非固定 86,400 秒）。
