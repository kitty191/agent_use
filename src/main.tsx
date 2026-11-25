import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { HistoryView, type TokenParts } from "./HistoryView";
import "./style.css";

type Agent = { id: string; name: string; status: string };
type Settings = { agents: Agent[]; enabled: string[]; confirmed: boolean };
type SourceStatus = "active" | "waiting";
type Usage = {
  today: number | null;
  today_complete_steps: number;
  today_incomplete_steps: number;
  today_incomplete_known: TokenParts | null;
  today_legacy_steps: number;
  today_legacy_tokens: number | null;
  latest: number | null;
  latest_at: number | null;
  latest_at_ms: number | null;
  latest_incomplete: boolean;
  latest_parts: TokenParts | null;
  latest_legacy: boolean;
  latest_legacy_tokens: number | null;
  status: SourceStatus;
};
type WindowData = {
  pool: string;
  minutes: number | null;
  used: number;
  reset: number | null;
};
type Quota = {
  account: string;
  plan: string | null;
  windows: WindowData[];
  checked_at: number;
};
type ThemeMode = "system" | "light" | "dark";
type ViewMode = "overview" | "history" | "settings";
type UsageReadState = "loading" | "active" | "waiting" | "error";
type PendingAction = "idle" | "initial" | "rescan" | "save";

const USAGE_PROVIDERS = new Set(["codex", "claude", "antigravity"]);
const supportsLocalUsage = (id: string) => USAGE_PROVIDERS.has(id);

const formatTime = (seconds: number) =>
  new Date(seconds * 1000).toLocaleString("zh-CN", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });

const statusText = (a: Agent) =>
  ({ installed: "已检测到", data: "仅发现本地数据", missing: "未检测到" })[
    a.status
  ] ?? a.status;

const capability = (id: string) =>
  id === "codex"
    ? "本机 Token · 联网查询账号额度"
    : id === "claude"
      ? "本机 Token · 离线读取"
      : id === "antigravity"
        ? "本机对话日志 Token · 账号/额度/重置暂不可查"
        : "安装状态 · 用量来源待验证";

function formatParts(parts: TokenParts | null, showMissing: boolean) {
  if (!parts) return "";
  const items: string[] = [];
  if (parts.input != null) items.push(`输入 ${parts.input.toLocaleString()}`);
  else if (showMissing) items.push("输入未知");
  if (parts.cache_read != null)
    items.push(`缓存读取 ${parts.cache_read.toLocaleString()}`);
  else if (showMissing) items.push("缓存未知");
  if (parts.output != null) items.push(`输出 ${parts.output.toLocaleString()}`);
  else if (showMissing) items.push("输出未知");
  return items.join(" · ");
}

function duration(minutes: number | null) {
  if (!minutes) return "额度窗口";
  if (minutes % 1440 === 0) return minutes / 1440 + " 天额度";
  if (minutes % 60 === 0) return minutes / 60 + " 小时额度";
  return minutes + " 分钟额度";
}

function countdown(seconds: number) {
  if (seconds <= 0) return "已到预计重置时间，等待确认";
  const d = Math.floor(seconds / 86400),
    h = Math.floor(seconds / 3600) % 24,
    m = Math.floor(seconds / 60) % 60;
  return [
    d ? d + "天" : "",
    h || d ? h + "小时" : "",
    m + "分",
    (seconds % 60) + "秒",
  ]
    .filter(Boolean)
    .join(" ");
}

function quotaTone(remaining: number) {
  if (remaining <= 15) return "danger";
  if (remaining <= 35) return "warn";
  return "healthy";
}

function ProviderIcon({ id }: { id: string }) {
  if (id === "codex") {
    return (
      <span className="provider-icon provider-codex" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <polyline points="8 8 4 12 8 16" />
          <polyline points="16 8 20 12 16 16" />
          <line x1="14" y1="6" x2="10" y2="18" />
        </svg>
      </span>
    );
  }
  if (id === "claude") {
    return (
      <span className="provider-icon provider-claude" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M12 3v18M3 12h18M5.6 5.6l12.8 12.8M18.4 5.6 5.6 18.4" />
        </svg>
      </span>
    );
  }
  if (id === "antigravity") {
    return (
      <span className="provider-icon provider-antigravity" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M12 4 5 19h3.4l1.5-3.4h4.2L15.6 19H19L12 4Z" />
          <circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none" />
        </svg>
      </span>
    );
  }
  return (
    <span className="provider-icon provider-default" aria-hidden="true">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
        <rect x="4" y="5" width="16" height="14" rx="3" />
        <path d="m8 10 2.5 2L8 14M13 14h3" />
      </svg>
    </span>
  );
}

function RefreshIcon({ spinning }: { spinning?: boolean }) {
  return (
    <svg
      className={spinning ? "icon spin" : "icon"}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M21 12a9 9 0 1 1-2.64-6.36" />
      <polyline points="21 3 21 9 15 9" />
    </svg>
  );
}

function ClockIcon() {
  return (
    <svg className="icon-xs" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="9" />
      <polyline points="12 7 12 12 15.5 14" />
    </svg>
  );
}

function ThemeIcon({ mode }: { mode: ThemeMode }) {
  if (mode === "dark") {
    return (
      <svg className="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z" />
      </svg>
    );
  }
  if (mode === "light") {
    return (
      <svg className="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        <circle cx="12" cy="12" r="4" />
        <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
      </svg>
    );
  }
  return (
    <svg className="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="8" />
      <path d="M12 4a8 8 0 0 0 0 16V4Z" fill="currentColor" stroke="none" />
    </svg>
  );
}

function QuotaPanel() {
  const [quota, setQuota] = useState<Quota | null>(null);
  const [fresh, setFresh] = useState(false),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  const [retryAt, setRetryAt] = useState(0);
  const alive = useRef(false),
    inFlight = useRef(false),
    unblock = useRef(0);

  const refresh = useCallback(async () => {
    if (inFlight.current || Date.now() < unblock.current) return;
    inFlight.current = true;
    setBusy(true);
    setError("");
    try {
      const q = await invoke<Quota>("refresh_quota");
      if (alive.current) {
        setQuota(q);
        setFresh(true);
      }
    } catch (err) {
      if (alive.current) {
        setError(String(err));
        setFresh(false);
        if (String(err).includes("限流")) {
          unblock.current = Date.now() + 300000;
          setRetryAt(unblock.current);
        }
      }
    } finally {
      inFlight.current = false;
      if (alive.current) setBusy(false);
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    async function open() {
      try {
        const q = await invoke<Quota | null>("last_quota");
        if (alive.current) setQuota(q);
      } catch (err) {
        if (alive.current) setError(String(err));
      }
      if (alive.current) void refresh();
    }
    void open();
    const poll = window.setInterval(refresh, 300000);
    const tick = window.setInterval(
      () => setNow(Math.floor(Date.now() / 1000)),
      1000,
    );
    return () => {
      alive.current = false;
      window.clearInterval(poll);
      window.clearInterval(tick);
    };
  }, [refresh]);

  return (
    <section className="quota panel">
      <header className="quota-header">
        <div className="title-cluster">
          <ProviderIcon id="codex" />
          <div>
            <span className="eyebrow">账号额度</span>
            <h2>OpenAI Codex</h2>
          </div>
        </div>
        <button
          className="btn-secondary"
          onClick={refresh}
          disabled={busy || now * 1000 < retryAt}
        >
          <RefreshIcon spinning={busy} />
          <span>{busy ? "正在查询…" : "刷新额度"}</span>
        </button>
      </header>

      {error && (
        <p role="status" className="notice">
          {error}
        </p>
      )}

      {quota ? (
        <>
          <div className="account-bar">
            <span className={fresh ? "dot live" : "dot stale"} aria-hidden="true" />
            <span className="account-label">
              {fresh ? "查询账号" : "上次成功查询的账号"}
            </span>
            <span className="account-id">{quota.account}</span>
            {quota.plan && (
              <strong className="plan-badge">{quota.plan.toUpperCase()}</strong>
            )}
          </div>

          {quota.windows.length === 0 && (
            <p className="muted">当前账号未返回额度窗口</p>
          )}

          <div className="windows-grid">
            {quota.windows.map((w, i) => {
              const remaining = Math.max(0, Math.min(100, 100 - w.used));
              const tone = quotaTone(remaining);
              return (
                <div className={`window tone-${tone}`} key={w.pool + i}>
                  <div className="window-head">
                    <div className="window-title">
                      <span>{duration(w.minutes)}</span>
                      <span className="pool-pill">{w.pool}</span>
                    </div>
                    <div className="window-percent">
                      <span className="percent-label">剩余</span>
                      <b>{remaining.toLocaleString()}%</b>
                    </div>
                  </div>
                  <div
                    className="progress-track"
                    role="progressbar"
                    aria-label={duration(w.minutes) + "剩余额度"}
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={remaining}
                  >
                    <div
                      className={`progress-fill tone-${tone}`}
                      style={{ width: `${remaining}%` }}
                    />
                  </div>
                  <div className="timing">
                    <span className="timing-reset">
                      <ClockIcon />
                      {w.reset == null
                        ? "重置时间未知"
                        : "重置时间 " + formatTime(w.reset)}
                    </span>
                    {w.reset != null && (
                      <span className="countdown-pill">
                        {countdown(w.reset - now)}
                      </span>
                    )}
                  </div>
                </div>
              );
            })}
          </div>

          <footer className="card-footer">
            <ClockIcon />
            <span>
              {fresh ? "最近查询成功" : "历史快照"} ·{" "}
              {formatTime(quota.checked_at)}
            </span>
          </footer>
        </>
      ) : (
        <div className="empty-well">
          <p className="muted">
            {busy ? "正在通过本机 Codex 查询…" : "暂无已确认的额度数据"}
          </p>
        </div>
      )}
    </section>
  );
}

function UsagePanel({
  agent,
  onStatusChange,
}: {
  agent: Agent;
  onStatusChange?: (id: string, status: UsageReadState | null) => void;
}) {
  const [usage, setUsage] = useState<Usage | null>(null),
    [err, setErr] = useState("");
  const supported = supportsLocalUsage(agent.id);

  useEffect(() => {
    if (!supported) {
      onStatusChange?.(agent.id, null);
      return;
    }
    onStatusChange?.(agent.id, "loading");
    let active = true,
      reading = false,
      timer = 0;
    async function read() {
      if (!active || reading) return;
      window.clearTimeout(timer);
      reading = true;
      try {
        const u = await invoke<Usage>("local_usage", { id: agent.id });
        if (active) {
          setUsage(u);
          setErr("");
          onStatusChange?.(
            agent.id,
            u.status === "active" ? "active" : "waiting",
          );
        }
      } catch (e) {
        if (active) {
          setErr(String(e));
          onStatusChange?.(agent.id, "error");
        }
      }
      reading = false;
      // Leave time within the two-second display target for IPC and WebView rendering.
      if (active) timer = window.setTimeout(read, 1000);
    }
    function onVisible() {
      if (document.visibilityState === "visible") void read();
    }
    window.addEventListener("focus", onVisible);
    document.addEventListener("visibilitychange", onVisible);
    void read();
    return () => {
      active = false;
      onStatusChange?.(agent.id, null);
      window.removeEventListener("focus", onVisible);
      document.removeEventListener("visibilitychange", onVisible);
      window.clearTimeout(timer);
    };
  }, [agent.id, supported, onStatusChange]);

  return (
    <article
      className="usage panel"
      data-provider={agent.id}
      data-latest-at={usage?.latest_at}
    >
      <header className="usage-header">
        <div className="title-cluster">
          <ProviderIcon id={agent.id} />
          <div>
            <h3>{agent.name}</h3>
            <small className="provider-sub">{capability(agent.id)}</small>
          </div>
        </div>
        <span className={`state state-${agent.status}`}>
          {statusText(agent)}
        </span>
      </header>

      {supported ? (
        <>
          <div className="metrics">
            <div className="metric-box">
              <small>今日已记录 Token</small>
              <strong
                className={
                  usage &&
                  usage.today == null &&
                  (usage.today_incomplete_steps > 0 ||
                    usage.today_legacy_steps > 0)
                    ? "metric-incomplete"
                    : undefined
                }
              >
                {usage?.today != null
                  ? usage.today.toLocaleString()
                  : usage && usage.today_incomplete_steps > 0
                    ? "用量不完整"
                    : usage && usage.today_legacy_steps > 0
                      ? "仅含旧版记录"
                      : "—"}
              </strong>
              {usage && usage.today_incomplete_steps > 0 && (
                <span className="metric-hint">
                  {usage.today != null
                    ? `仅含 ${usage.today_complete_steps} 步完整记录 · 另有 ${usage.today_incomplete_steps} 步不完整`
                    : `今日 ${usage.today_incomplete_steps} 步字段缺失`}
                  {usage.today_incomplete_known
                    ? `（已知 ${formatParts(usage.today_incomplete_known, false)}）`
                    : "（分项全部未知）"}
                </span>
              )}
              {usage && usage.today_legacy_steps > 0 && (
                <span className="metric-hint">
                  旧版记录 {usage.today_legacy_steps} 步（旧版值{" "}
                  {usage.today_legacy_tokens?.toLocaleString() ?? "—"}
                  ，原日志已移除未核验，未计入完整合计）
                </span>
              )}
            </div>
            <div className="metric-box">
              <small>
                {agent.id === "antigravity" ? "最近一次模型步骤" : "最近一次响应"}
              </small>
              <strong
                className={
                  usage?.latest_incomplete || usage?.latest_legacy
                    ? "metric-incomplete"
                    : undefined
                }
              >
                {usage?.latest_incomplete
                  ? "用量不完整"
                  : usage?.latest_legacy
                    ? "仅含旧版记录"
                    : usage?.latest != null
                      ? usage.latest.toLocaleString()
                      : "—"}
              </strong>
              {usage?.latest_incomplete && usage.latest_parts && (
                <span className="metric-hint">
                  {formatParts(usage.latest_parts, true)}
                </span>
              )}
              {!usage?.latest_incomplete && usage?.latest_legacy && (
                <span className="metric-hint">
                  旧版记录值 {usage.latest_legacy_tokens?.toLocaleString() ?? "—"}{" "}
                  Token（原日志已移除，时间精度为秒）
                </span>
              )}
            </div>
          </div>
          {agent.id === "antigravity" && (
            <p className="source-note">
              来源：Antigravity 本机对话日志（按本地日期统计已完成模型步骤；仅含本机已记录用量，账号、模型池、剩余额度与重置时间暂不可查）。
            </p>
          )}
          <footer className={err ? "card-footer footer-error" : "card-footer"}>
            <ClockIcon />
            <span>
              {err
                ? err
                : usage?.status === "waiting"
                  ? usage.latest_at
                    ? "等待首次对话记录 · 历史最近" +
                      (agent.id === "antigravity" ? "步骤 " : "用量 ") +
                      formatTime(usage.latest_at)
                    : "等待首次对话记录"
                  : usage?.latest_at
                    ? (agent.id === "antigravity" ? "最近步骤 " : "最近用量 ") +
                      formatTime(usage.latest_at)
                    : usage
                      ? "尚未读到用量记录"
                      : "读取本地记录中…"}
            </span>
          </footer>
        </>
      ) : (
        <div className="info-well">
          <p className="muted">用量与额度来源待验证</p>
        </div>
      )}
    </article>
  );
}

function App() {
  const [settings, setSettings] = useState<Settings | null>(null),
    [view, setView] = useState<ViewMode>("overview");
  const [draft, setDraft] = useState<string[]>([]),
    [pending, setPending] = useState<PendingAction>("idle"),
    [error, setError] = useState("");
  const [newIds, setNewIds] = useState<string[]>([]);
  const [usageStates, setUsageStates] = useState<Record<string, UsageReadState>>(
    {},
  );
  const [theme, setTheme] = useState<ThemeMode>(() => {
    const saved = window.localStorage.getItem("agent-use-theme");
    return saved === "light" || saved === "dark" ? saved : "system";
  });

  const editing = view === "settings";
  const busy = pending !== "idle";
  const rescanning = pending === "rescan";
  const saving = pending === "save";

  const handleUsageStatus = useCallback(
    (id: string, status: UsageReadState | null) => {
      setUsageStates((prev) => {
        if (status === null) {
          if (!(id in prev)) return prev;
          const next = { ...prev };
          delete next[id];
          return next;
        }
        if (prev[id] === status) return prev;
        return { ...prev, [id]: status };
      });
    },
    [],
  );

  useEffect(() => {
    if (theme === "system") {
      document.documentElement.removeAttribute("data-theme");
      window.localStorage.removeItem("agent-use-theme");
    } else {
      document.documentElement.setAttribute("data-theme", theme);
      window.localStorage.setItem("agent-use-theme", theme);
    }
  }, [theme]);

  const cycleTheme = () => {
    setTheme((prev) =>
      prev === "system" ? "light" : prev === "light" ? "dark" : "system",
    );
  };

  const themeLabel =
    theme === "system" ? "跟随系统外观" : theme === "light" ? "浅色外观" : "深色外观";

  async function load(rescan: boolean) {
    setPending(rescan ? "rescan" : "initial");
    setError("");
    try {
      const next = await invoke<Settings>("load_settings", { rescan });
      if (rescan && settings)
        setNewIds(
          next.agents
            .filter(
              (a) =>
                a.status !== "missing" &&
                !settings.agents.some(
                  (old) => old.id === a.id && old.status !== "missing",
                ),
            )
            .map((a) => a.id),
        );
      setSettings(next);
      if (!rescan || !editing) setDraft(next.enabled);
      if (rescan || !next.confirmed) setView("settings");
    } catch (e) {
      setError(String(e));
    } finally {
      setPending("idle");
    }
  }

  useEffect(() => {
    void load(false);
  }, []);

  async function save() {
    setPending("save");
    setError("");
    try {
      const next = await invoke<Settings>("save_selection", { ids: draft });
      setSettings(next);
      setView("overview");
      setNewIds([]);
    } catch (e) {
      setError(String(e));
    } finally {
      setPending("idle");
    }
  }

  const chosen =
    settings?.agents.filter((a) => settings.enabled.includes(a.id)) ?? [];
  const usageChosen = chosen.filter((a) => supportsLocalUsage(a.id));
  const activeCollectors = usageChosen.filter(
    (a) => usageStates[a.id] === "active",
  );
  const allCollectorsFailed =
    usageChosen.length > 0 &&
    usageChosen.every((a) => usageStates[a.id] === "error");
  const allCollectorsWaiting =
    usageChosen.length > 0 &&
    usageChosen.every((a) => usageStates[a.id] === "waiting");
  const found =
    settings?.agents.filter(
      (a) => a.status !== "missing" || settings.enabled.includes(a.id),
    ) ?? [];
  const missing = settings?.agents.filter((a) => !found.includes(a)) ?? [];

  return (
    <div className="shell">
      <nav className="topbar">
        <div className="topbar-inner">
          <div className="brand">
            <span className="brand-badge" aria-hidden="true">
              <span className="mark">
                <i />
                <i />
                <i />
              </span>
            </span>
            <span>Agent Use</span>
          </div>

          <div className="segmented" role="tablist" aria-label="视图切换">
            <button
              disabled={!settings?.confirmed || busy}
              aria-current={view === "overview" ? "page" : undefined}
              onClick={() => {
                setView("overview");
                setError("");
              }}
            >
              用量总览
            </button>
            <button
              disabled={!settings || busy}
              aria-current={view === "history" ? "page" : undefined}
              onClick={() => {
                setView("history");
                setError("");
              }}
            >
              历史用量
            </button>
            <button
              disabled={!settings || busy}
              aria-current={view === "settings" ? "page" : undefined}
              onClick={() => {
                setDraft(settings?.enabled ?? []);
                setView("settings");
                setError("");
              }}
            >
              平台管理
            </button>
          </div>

          <div className="topbar-actions">
            {settings && (
              <button
                className="btn-ghost"
                disabled={busy}
                onClick={() => void load(true)}
              >
                <RefreshIcon spinning={rescanning} />
                <span>{rescanning ? "扫描中…" : "重新扫描"}</span>
              </button>
            )}
            <button
              className="btn-icon"
              type="button"
              onClick={cycleTheme}
              title={themeLabel}
              aria-label={themeLabel}
            >
              <ThemeIcon mode={theme} />
            </button>
          </div>
        </div>
      </nav>

      <main>
        <div className="page-title">
          <div>
            <span className="eyebrow">本机工作区</span>
            <h1>
              {view === "settings"
                ? "平台管理"
                : view === "history"
                  ? "历史用量"
                  : "用量总览"}
            </h1>
          </div>
          {view === "overview" && usageChosen.length > 0 && (
            activeCollectors.length > 0 ? (
              <span className="live-pill" data-live-state="active">
                <span className="dot live pulse" aria-hidden="true" />
                本地增量监听中 ·{" "}
                {activeCollectors.map((a) => a.name).join("、")}
              </span>
            ) : allCollectorsFailed ? (
              <span className="live-pill pill-error" data-live-state="error">
                <span className="dot stale" aria-hidden="true" />
                本地采集读取失败
              </span>
            ) : allCollectorsWaiting ? (
              <span className="live-pill" data-live-state="waiting">
                <span className="dot idle" aria-hidden="true" />
                等待首次对话记录
              </span>
            ) : (
              <span className="live-pill" data-live-state="loading">
                <span className="dot idle" aria-hidden="true" />
                正在读取本地采集…
              </span>
            )
          )}
        </div>

        {error && (
          <p role="alert" className="notice">
            <span>{error}</span>
            {!settings && (
              <button
                className="btn-secondary btn-sm"
                onClick={() => void load(false)}
              >
                重试
              </button>
            )}
          </p>
        )}

        {!settings && !error && (
          <div className="panel loading-panel">正在载入本机平台…</div>
        )}

        {editing && settings && (
          <section className="panel settings">
            <header className="settings-header">
              <div>
                <h2>选择要监控的平台</h2>
                <p className="muted">
                  已发现{" "}
                  <strong>
                    {found.filter((a) => a.status !== "missing").length}
                  </strong>{" "}
                  个平台。安装或移除工具后可点击右上角「重新扫描」。
                </p>
              </div>
              <span className="count-pill">已勾选 {draft.length}</span>
            </header>

            <div className="agent-list">
              {found.map((a) => {
                const checked = draft.includes(a.id);
                return (
                  <label
                    className={checked ? "row row-selected" : "row"}
                    key={a.id}
                  >
                    <ProviderIcon id={a.id} />
                    <span className="row-info">
                      <b>{a.name}</b>
                      <small>{capability(a.id)}</small>
                    </span>
                    <span className="row-meta">
                      {newIds.includes(a.id) && (
                        <span className="new-pill">新发现</span>
                      )}
                      <span className={`state state-${a.status}`}>
                        {statusText(a)}
                      </span>
                      <span className="switch">
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={(e) =>
                            setDraft(
                              e.target.checked
                                ? [...draft, a.id]
                                : draft.filter((id) => id !== a.id),
                            )
                          }
                        />
                        <span className="switch-track" aria-hidden="true" />
                      </span>
                    </span>
                  </label>
                );
              })}
            </div>

            {missing.length > 0 && (
              <details className="missing-box">
                <summary>未检测到的平台（{missing.length}）</summary>
                <div className="chip-list">
                  {missing.map((a) => (
                    <span className="chip" key={a.id}>
                      {a.name}
                    </span>
                  ))}
                </div>
              </details>
            )}

            <div className="actions">
              <button className="primary" disabled={busy} onClick={save}>
                {saving
                  ? "保存中…"
                  : settings.confirmed
                    ? "保存选择"
                    : "确认选择"}
              </button>
            </div>
          </section>
        )}

        {settings && view === "history" && (
          <HistoryView active={view === "history"} />
        )}

        {settings?.confirmed && (
          <div hidden={view !== "overview"}>
            {chosen.some((a) => a.id === "codex") && <QuotaPanel />}
            {chosen.length > 0 ? (
              <>
                <div className="section-title">
                  <h2>本机用量</h2>
                  <span className="count-pill">{chosen.length} 个已选平台</span>
                </div>
                <div className="cards">
                  {chosen.map((a) => (
                    <UsagePanel
                      key={a.id}
                      agent={a}
                      onStatusChange={handleUsageStatus}
                    />
                  ))}
                </div>
              </>
            ) : (
              <section className="panel empty-state">
                <h2>尚未启用平台</h2>
                <p className="muted">前往平台管理，选择需要查看的本机工具。</p>
                <button
                  className="primary"
                  onClick={() => {
                    setDraft([]);
                    setView("settings");
                  }}
                >
                  选择平台
                </button>
              </section>
            )}
          </div>
        )}
      </main>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
