# agent_use

本机 AI Agent Token 用量与配额监控桌面软件（Tauri 2 + Rust + React + TypeScript + SQLite），支持 macOS / Windows / Linux，首批覆盖 **OpenAI Codex**、**Claude Code** 与 **Google Antigravity**。

- 完整产品与验收规格书：[docs/architecture_plan.md](docs/architecture_plan.md)

## 当前阶段：Phase 0（数据源真机核验）

在进入 Phase 1 正式开发前，请先在目标机器（当前以 macOS 为首发环境）运行只读脱敏探测脚本：

```bash
python3 scripts/phase0_probe.py > phase0_report.json
```

脚本仅使用 Python 3 标准库、100% 只读、零联网，不读取凭证/Cookie 文件或进程命令行参数，也不输出任何 prompt、回复正文、路径或邮箱。运行后请先人工检查 `phase0_report.json` 内容再用于 Phase 0 能力矩阵核对。
