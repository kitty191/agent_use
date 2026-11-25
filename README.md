# agent_use

本机 AI Agent Token 用量与配额监控桌面软件（Tauri 2 + Rust + React + TypeScript + SQLite），支持 macOS / Windows / Linux，首批覆盖 **OpenAI Codex**、**Claude Code** 与 **Google Antigravity**。

- 完整产品与验收规格书：[docs/architecture_plan.md](docs/architecture_plan.md)

## macOS 桌面原型

在安装了 Rust 稳定版、Node.js 22.12 或更高版本以及 Xcode Command Line Tools 的 Mac 上，从仓库根目录运行：

```sh
npm ci
npm run tauri dev
```

使用 `npm run tauri build` 生成本机架构的开发版应用包，位置为 `target/release/bundle/macos/Agent Use.app`。额度查询需要本机已登录的 Codex CLI。

首次启动会检查注册清单内的应用、CLI 和数据目录。确认选择后才启用平台。SQLite 保存扫描结果（含空结果）、已选项、本机用量游标和最近一次成功的 Codex 额度快照。再次启动直接恢复面板；安装或移除工具后点击「重新扫描」更新列表。

- **Codex**：启动、手动刷新及每五分钟通过本机 app-server 查询当前账号和额度，按返回窗口时长显示剩余百分比与重置倒计时。查询失败时，已有快照保留其原账号标签和查询时间。
- **Codex / Claude Code**：首次读取已启用来源的 JSONL，之后通过文件系统事件增量更新。面板也会检查最近使用的日志是否增长，每 30 秒校验已启用平台的日志目录，以补读遗漏的文件。面板显示本机今日 Token 与最近一次响应。缺少记录的字段显示为「—」。
- **Antigravity、DeepSeek 桌面端等**：当前展示本机发现状态，账号、Token 和额度采集取决于各自数据源后续验证。

核心验证可运行 `cargo test -p agent-core` 和 `python3 scripts/test_phase0_snapshot.py`。macOS 官方重置时间已核对至分钟，跨终端用量补读延迟仍待真机复测。

## Phase 0 数据源探测

在目标机器上可运行只读脱敏探测脚本：

```bash
python3 scripts/phase0_probe.py > phase0_report.json
```

脚本仅使用 Python 3 标准库、100% 只读、零联网，不读取凭证/Cookie 文件或进程命令行参数，也不输出任何 prompt、回复正文、路径或邮箱。运行后请先人工检查 `phase0_report.json` 内容再用于 Phase 0 能力矩阵核对。
