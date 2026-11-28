import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { HistoryView, type TokenParts } from "./HistoryView";
import "./style.css";

type Agent = { id: string; name: string; status: string };
type Settings = { agents: Agent[]; enabled: string[]; confirmed: boolean };
type SourceStatus = "active" | "partial" | "waiting";
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
  unreadable_files?: number;
  warning?: string | null;
};
type WindowData = {
  pool: string;
  minutes: number | null;
  used: number;
  reset: number | null;
  label?: string | null;
  models?: string[];
  remaining_percent?: number | null;
  remaining_amount?: number | null;
  unit?: string | null;
  disabled?: boolean;
};
type CreditBalance = {
  amount: number;
  unit?: string | null;
};
type Quota = {
  provider?: string;
  account_id?: string | null;
  account: string;
  plan: string | null;
  windows: WindowData[];
  checked_at: number;
  source?: string;
  source_kind?: string;
  ai_credits?: CreditBalance | null;
};
type QuotaRefreshResult = {
  quota: Quota | null;
  fresh: boolean;
  message: string | null;
};
type BridgeInfo = {
  script_path: string;
  snapshot_path: string;
  settings_snippet: string;
  wrap_command: string;
};
type ThemeMode = "system" | "light" | "dark";
type ViewMode = "overview" | "history" | "settings";
type UsageReadState = "loading" | "active" | "partial" | "waiting" | "error";
type PendingAction = "idle" | "initial" | "rescan" | "save";

const USAGE_PROVIDERS = new Set(["codex", "claude", "antigravity"]);
const QUOTA_PROVIDERS = new Set(["codex", "antigravity"]);
const supportsLocalUsage = (id: string) => USAGE_PROVIDERS.has(id);
const supportsQuota = (id: string) => QUOTA_PROVIDERS.has(id);

const formatTime = (seconds: number) =>
  new Date(seconds * 1000).toLocaleString("zh-CN", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });

const formatPercent = (value: number) =>
  Number.isInteger(value)
    ? value.toLocaleString()
    : Number(value.toFixed(1)).toLocaleString();

const windowRemainingPercent = (w: WindowData): number | null => {
  if (w.remaining_percent != null && Number.isFinite(w.remaining_percent)) {
    return Math.max(0, Math.min(100, w.remaining_percent));
  }
  return null;
};

const statusText = (a: Agent) =>
  ({
    installed: "已检测到",
    data: "仅发现历史记录",
    history: "仅有本地历史",
    missing: "未检测到",
  })[a.status] ?? a.status;

const capability = (id: string) =>
  id === "codex"
    ? "本机 Token · 在线账号与模型限额"
    : id === "claude"
      ? "本机对话日志 Token · 离线统计"
      : id === "antigravity"
        ? "本机对话日志 Token · CLI statusLine / 官方设置限额"
        : "已发现平台 · 用量与限额来源待验证";

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

function PlatformCard({
  agent,
  enabled,
  enabling,
  onEnable,
  onOpenHistory,
  onStatusChange,
}: {
  agent: Agent;
  enabled: boolean;
  enabling: boolean;
  onEnable: (id: string) => void;
  onOpenHistory: (id: string) => void;
  onStatusChange?: (id: string, status: UsageReadState | null) => void;
}) {
  const [usage, setUsage] = useState<Usage | null>(null);
  const [usageErr, setUsageErr] = useState("");
  const [quota, setQuota] = useState<Quota | null>(null);
  const [fresh, setFresh] = useState(false);
  const [quotaBusy, setQuotaBusy] = useState(false);
  const [quotaErr, setQuotaErr] = useState("");
  const [quotaNote, setQuotaNote] = useState("");
  const [bridgeInfo, setBridgeInfo] = useState<BridgeInfo | null>(null);
  const [copiedKey, setCopiedKey] = useState("");
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  const [retryAt, setRetryAt] = useState(0);

  const alive = useRef(false);
  const inFlight = useRef(false);
  const unblock = useRef(0);
  const manualReadRef = useRef<(() => Promise<void>) | null>(null);
  const copyTimerRef = useRef(0);

  const localSupported = supportsLocalUsage(agent.id);
  const quotaSupported = supportsQuota(agent.id);
  const isHistoryOnly = agent.status === "data";
  const isDbHistory = agent.status === "history";
  const hasSavedHistory = isHistoryOnly || isDbHistory;

  // Local usage collector (active polling only when enabled; read-only snapshot for disabled history-only/db-history).
  useEffect(() => {
    if (!localSupported) {
      setUsage(null);
      setUsageErr("");
      onStatusChange?.(agent.id, null);
      return;
    }
    if (!enabled || isDbHistory) {
      onStatusChange?.(agent.id, null);
      if (!hasSavedHistory) {
        setUsage(null);
        setUsageErr("");
        return;
      }
      let active = true;
      void invoke<Usage>("saved_usage", { id: agent.id })
        .then((u) => {
          if (active) {
            setUsage(u);
            setUsageErr("");
          }
        })
        .catch((e) => {
          if (active) setUsageErr(String(e));
        });
      return () => {
        active = false;
      };
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
          setUsageErr("");
          onStatusChange?.(
            agent.id,
            u.status === "active"
              ? "active"
              : u.status === "partial"
                ? "partial"
                : "waiting",
          );
        }
      } catch (e) {
        if (active) {
          setUsageErr(String(e));
          onStatusChange?.(agent.id, "error");
        }
      }
      reading = false;
      if (active) timer = window.setTimeout(read, 1000);
    }
    manualReadRef.current = read;
    function onVisible() {
      if (document.visibilityState === "visible") void read();
    }
    window.addEventListener("focus", onVisible);
    document.addEventListener("visibilitychange", onVisible);
    void read();
    return () => {
      active = false;
      manualReadRef.current = null;
      onStatusChange?.(agent.id, null);
      window.removeEventListener("focus", onVisible);
      document.removeEventListener("visibilitychange", onVisible);
      window.clearTimeout(timer);
    };
  }, [agent.id, enabled, hasSavedHistory, isDbHistory, localSupported, onStatusChange]);

  const refreshQuota = useCallback(async () => {
    if (!enabled || isDbHistory || !quotaSupported) return;
    if (inFlight.current || Date.now() < unblock.current) return;
    inFlight.current = true;
    setQuotaBusy(true);
    setQuotaErr("");
    try {
      const res = await invoke<QuotaRefreshResult>("refresh_provider_quota", {
        id: agent.id,
      });
      if (alive.current) {
        if (res.quota != null) setQuota(res.quota);
        setFresh(res.fresh);
        setQuotaNote(res.message ?? "");
      }
    } catch (err) {
      if (alive.current) {
        setQuotaErr(String(err));
        setFresh(false);
        if (String(err).includes("限流")) {
          unblock.current = Date.now() + 300000;
          setRetryAt(unblock.current);
        }
      }
    } finally {
      inFlight.current = false;
      if (alive.current) setQuotaBusy(false);
    }
  }, [agent.id, enabled, quotaSupported]);

  useEffect(() => {
    alive.current = true;
    if (!enabled || !quotaSupported) {
      setQuota(null);
      setFresh(false);
      setQuotaBusy(false);
      setQuotaErr("");
      setQuotaNote("");
      return () => {
        alive.current = false;
      };
    }

    async function open() {
      try {
        const cached = await invoke<Quota | null>("last_provider_quota", {
          id: agent.id,
        });
        if (alive.current && cached) {
          setQuota(cached);
          setFresh(false);
        }
      } catch (err) {
        if (alive.current) setQuotaErr(String(err));
      }
      if (alive.current) void refreshQuota();
    }
    void open();

    const pollMs = agent.id === "codex" ? 300000 : 30000;
    const poll = window.setInterval(refreshQuota, pollMs);
    const tick = window.setInterval(
      () => setNow(Math.floor(Date.now() / 1000)),
      1000,
    );
    return () => {
      alive.current = false;
      window.clearInterval(poll);
      window.clearInterval(tick);
    };
  }, [agent.id, enabled, quotaSupported, refreshQuota]);

  useEffect(() => {
    if (agent.id !== "antigravity" || !enabled || isDbHistory) {
      setBridgeInfo(null);
      return;
    }
    let active = true;
    void invoke<BridgeInfo>("antigravity_bridge_info")
      .then((info) => {
        if (active) setBridgeInfo(info);
      })
      .catch(() => {
        // Optional helper metadata; card still works without it.
      });
    return () => {
      active = false;
    };
  }, [agent.id, enabled, isDbHistory]);

  useEffect(() => {
    return () => {
      window.clearTimeout(copyTimerRef.current);
    };
  }, []);

  const copyBridgeText = useCallback(async (key: string, text: string) => {
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const el = document.createElement("textarea");
        el.value = text;
        el.style.position = "fixed";
        el.style.opacity = "0";
        document.body.appendChild(el);
        el.select();
        document.execCommand("copy");
        document.body.removeChild(el);
      }
      setCopiedKey(key);
      window.clearTimeout(copyTimerRef.current);
      copyTimerRef.current = window.setTimeout(() => setCopiedKey(""), 1800);
    } catch {
      // Ignore clipboard permission denial silently; code snippet remains selectable.
    }
  }, []);

  const handleRefreshCard = () => {
    if (localSupported && enabled && manualReadRef.current) {
      void manualReadRef.current();
    }
    if (enabled && quotaSupported) {
      void refreshQuota();
    }
  };

  const badgeClass = hasSavedHistory
    ? "state state-data"
    : enabled
      ? "state state-installed"
      : "state state-disabled";
  const badgeText = isDbHistory
    ? "仅有本地历史"
    : isHistoryOnly
      ? enabled
        ? "仅发现历史记录 · 离线采集"
        : "仅发现历史记录"
      : enabled
        ? "已启用"
        : "未启用";

  const showUsageBlock = localSupported && (enabled || hasSavedHistory);
  const isLocalSnapshot =
    quota != null &&
    (quota.source_kind === "local_snapshot" ||
      (quota.provider === "antigravity" && quota.source_kind !== "live_query"));

  return (
    <section
      className="quota panel platform-card"
      data-provider={agent.id}
      data-card-state={
        isDbHistory
          ? "history-only"
          : isHistoryOnly
            ? enabled
              ? "data-enabled"
              : "data-only"
            : enabled
              ? "enabled"
              : "disabled"
      }
      data-latest-at={usage?.latest_at}
    >
      <header className="quota-header">
        <div className="title-cluster">
          <ProviderIcon id={agent.id} />
          <div>
            <span className="eyebrow">用量与限额</span>
            <h2>{agent.name}</h2>
          </div>
        </div>
        <div className="platform-actions">
          <span className={badgeClass}>{badgeText}</span>
          {isDbHistory ? (
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => onOpenHistory(agent.id)}
            >
              查看历史
            </button>
          ) : !enabled ? (
            <>
              {isHistoryOnly && (
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => onOpenHistory(agent.id)}
                >
                  查看历史
                </button>
              )}
              <button
                type="button"
                className="primary btn-sm"
                disabled={enabling}
                onClick={() => onEnable(agent.id)}
              >
                {enabling
                  ? "启用中…"
                  : isHistoryOnly
                    ? "启用离线采集"
                    : "启用监控"}
              </button>
            </>
          ) : (
            (quotaSupported || localSupported) && (
              <button
                type="button"
                className="btn-secondary btn-sm"
                onClick={handleRefreshCard}
                disabled={quotaBusy || now * 1000 < retryAt}
              >
                <RefreshIcon spinning={quotaBusy} />
                <span>{quotaBusy ? "正在查询…" : "刷新"}</span>
              </button>
            )
          )}
        </div>
      </header>

      <div className="account-bar">
        {quota ? (
          <>
            <span
              className={!isLocalSnapshot && fresh ? "dot live" : "dot stale"}
              aria-hidden="true"
            />
            <span className="account-label">
              {isLocalSnapshot
                ? "快照账号"
                : fresh
                  ? "查询账号"
                  : "上次成功查询的账号"}
            </span>
            <span className="account-id">{quota.account}</span>
            {quota.plan && (
              <strong className="plan-badge">{quota.plan.toUpperCase()}</strong>
            )}
          </>
        ) : (
          <>
            <span className="dot idle" aria-hidden="true" />
            <span className="account-label">
              {isDbHistory
                ? "原应用与日志目录已移除，仅保留本地数据库历史"
                : !enabled
                  ? isHistoryOnly
                    ? "仅发现本机历史日志，未检测到应用安装或登录状态"
                    : "平台未启用 · 启用后开始本机采集与额度读取"
                  : agent.id === "antigravity"
                    ? "账号与在线模型配额可在 Antigravity 设置 → Models 或 CLI statusLine 查看"
                    : agent.id === "codex"
                      ? quotaBusy
                        ? "正在通过 Codex CLI 读取账号…"
                        : "暂未读取到 Codex 账号信息"
                      : localSupported
                        ? "本机离线日志统计（无在线账号接口）"
                        : "来源待验证"}
            </span>
          </>
        )}
      </div>

      {showUsageBlock ? (
        <>
          <div className="card-section-label">本机用量</div>
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
              {!usage?.latest_incomplete &&
                !usage?.latest_legacy &&
                usage?.latest_parts && (
                  <span className="metric-hint">
                    {formatParts(usage.latest_parts, false)}
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
        </>
      ) : !enabled ? (
        <div className="info-well">
          <p className="muted">
            已在本机检测到 {agent.name}，当前处于未启用状态，不会读取本地日志或联网查询；点击「启用监控」后开始展示。
          </p>
        </div>
      ) : (
        <div className="info-well">
          <p className="muted">用量与额度来源待验证</p>
        </div>
      )}

      {enabled && quotaSupported && (
        <>
          <div className="card-section-label">模型与账号限额</div>
          {quotaErr && (
            <p role="status" className="notice">
              {quotaErr}
            </p>
          )}
          {!quotaErr && quota && quotaNote && (
            <p className="source-note" style={{ marginTop: 10, marginBottom: 0 }}>
              {quotaNote}
            </p>
          )}
          {quota ? (
            <>
              {quota.windows.length > 0 ? (
                <div className="windows-grid">
                  {quota.windows.map((w, i) => {
                    const remainingPct = windowRemainingPercent(w);
                    const tone =
                      remainingPct != null
                        ? quotaTone(remainingPct)
                        : "healthy";
                    return (
                      <div
                        className={`window tone-${tone}`}
                        key={`${w.pool}-${i}`}
                      >
                        <div className="window-head">
                          <div className="window-title">
                            <span>{w.label || duration(w.minutes)}</span>
                            <span className="pool-pill">{w.pool}</span>
                            {w.disabled && (
                              <span className="state state-missing">已停用</span>
                            )}
                          </div>
                          <div className="window-percent">
                            <span className="percent-label">剩余</span>
                            {remainingPct != null ? (
                              <>
                                <b>{formatPercent(remainingPct)}%</b>
                                {w.remaining_amount != null && (
                                  <span className="percent-label">
                                    （{w.remaining_amount.toLocaleString()}
                                    {w.unit ? ` ${w.unit}` : ""}）
                                  </span>
                                )}
                              </>
                            ) : w.remaining_amount != null ? (
                              <>
                                <b>{w.remaining_amount.toLocaleString()}</b>
                                {w.unit && (
                                  <span className="percent-label">{w.unit}</span>
                                )}
                              </>
                            ) : (
                              <b className="quota-unprovided">未提供</b>
                            )}
                          </div>
                        </div>
                        {w.models && w.models.length > 0 && (
                          <div className="model-chips">
                            {w.models.map((m) => (
                              <span className="model-chip" key={m}>
                                {m}
                              </span>
                            ))}
                          </div>
                        )}
                        {remainingPct != null && (
                          <div
                            className="progress-track"
                            role="progressbar"
                            aria-label={(w.label || duration(w.minutes)) + "剩余额度"}
                            aria-valuemin={0}
                            aria-valuemax={100}
                            aria-valuenow={remainingPct}
                          >
                            <div
                              className={`progress-fill tone-${tone}`}
                              style={{ width: `${remainingPct}%` }}
                            />
                          </div>
                        )}
                        <div
                          className="timing"
                          style={remainingPct == null ? { marginTop: 10 } : undefined}
                        >
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
              ) : (
                !quotaNote && (
                  <div className="info-well">
                    <p className="muted">
                      {agent.id === "antigravity"
                        ? "当前未收到 Antigravity CLI statusLine 额度池；模型限额可在 Antigravity 设置 → Models 查看。"
                        : "当前账号未返回额度窗口"}
                    </p>
                  </div>
                )
              )}
              {quota.ai_credits && (
                <div className="credits-bar">
                  <span>AI Credits 余额（与账号模型配额分开统计）</span>
                  <strong>
                    {quota.ai_credits.amount.toLocaleString()}{" "}
                    {quota.ai_credits.unit ?? "Credits"}
                  </strong>
                </div>
              )}
            </>
          ) : (
            <div className="empty-well">
              <p className="muted">
                {agent.id === "antigravity"
                  ? quotaNote ||
                    "当前仅检测到 Antigravity 桌面版，模型限额请在 Antigravity 设置 → Models 查看；若使用 Antigravity CLI，可展开下方桥接配置接入 statusLine 快照。本地累计 Token 不用于推算官方剩余额度。"
                  : quotaBusy
                    ? "正在通过本机 Codex 查询额度…"
                    : "暂无已确认的额度数据"}
              </p>
              {agent.id === "antigravity" && bridgeInfo && (
                <details className="bridge-details">
                  <summary>Antigravity CLI statusLine 桥接配置（已内置于应用包）</summary>
                  <div className="bridge-body">
                    <div className="bridge-actions">
                      <button
                        type="button"
                        className="btn-secondary btn-sm"
                        onClick={() =>
                          void copyBridgeText(
                            "settings",
                            bridgeInfo.settings_snippet,
                          )
                        }
                      >
                        {copiedKey === "settings"
                          ? "已复制配置"
                          : "复制 settings.json 配置"}
                      </button>
                      <button
                        type="button"
                        className="btn-secondary btn-sm"
                        onClick={() =>
                          void copyBridgeText("wrap", bridgeInfo.wrap_command)
                        }
                      >
                        {copiedKey === "wrap"
                          ? "已复制包装命令"
                          : "复制已有脚本包装命令"}
                      </button>
                    </div>
                    <pre className="bridge-code">{bridgeInfo.settings_snippet}</pre>
                    <p className="bridge-hint">
                      已有自定义状态栏脚本时，可将 command 改为：
                      <code>{bridgeInfo.wrap_command}</code>
                    </p>
                  </div>
                </details>
              )}
            </div>
          )}
        </>
      )}

      {(quota || showUsageBlock) && (
        <div className="footer-stack">
          {quota && (
            <footer className="card-footer">
              <ClockIcon />
              <span>
                {quota.source ? `额度来源：${quota.source} · ` : "额度来源 · "}
                {isLocalSnapshot
                  ? "快照采集时间"
                  : fresh
                    ? "最近查询成功"
                    : "历史快照"}{" "}
                · {formatTime(quota.checked_at)}
              </span>
            </footer>
          )}
          {showUsageBlock && (
            <footer
              className={
                usageErr
                  ? "card-footer footer-error"
                  : usage?.status === "partial"
                    ? "card-footer footer-warn"
                    : "card-footer"
              }
            >
              <ClockIcon />
              <span>
                {isDbHistory
                  ? usage?.latest_at
                    ? `仅有本地数据库历史 · 最近${agent.id === "antigravity" ? "步骤 " : "用量 "}${formatTime(usage.latest_at)}`
                    : "仅有本地数据库历史（可在历史用量查看过往周期）"
                  : !enabled && isHistoryOnly
                    ? usage?.latest_at
                      ? `已保存历史记录 · 最近${agent.id === "antigravity" ? "步骤 " : "用量 "}${formatTime(usage.latest_at)}`
                      : "仅发现历史目录，当前未查到已保存 Token 记录"
                    : usageErr
                    ? usageErr
                    : usage?.status === "waiting"
                      ? usage.latest_at
                        ? "等待首次对话记录 · 历史最近" +
                          (agent.id === "antigravity" ? "步骤 " : "用量 ") +
                          formatTime(usage.latest_at)
                        : "等待首次对话记录"
                      : usage?.status === "partial"
                        ? (usage.warning ?? "部分会话日志不可读，当前统计可能遗漏部分用量") +
                          (usage.latest_at
                            ? " · 最近" +
                              (agent.id === "antigravity" ? "步骤 " : "用量 ") +
                              formatTime(usage.latest_at)
                            : "")
                        : usage?.latest_at
                          ? (agent.id === "antigravity"
                              ? "本机日志最近步骤 "
                              : "本机日志最近用量 ") + formatTime(usage.latest_at)
                          : usage
                            ? "尚未读到本机用量记录"
                            : "读取本地记录中…"}
              </span>
            </footer>
          )}
        </div>
      )}
    </section>
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

  const [enablingId, setEnablingId] = useState<string | null>(null);
  const [historyProvider, setHistoryProvider] = useState<string | null>(null);

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
      if (!next.confirmed) setView("settings");
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

  async function enableFromOverview(id: string) {
    if (!settings) return;
    setEnablingId(id);
    setError("");
    try {
      const nextIds = settings.enabled.includes(id)
        ? settings.enabled
        : [...settings.enabled, id];
      const next = await invoke<Settings>("save_selection", { ids: nextIds });
      setSettings(next);
      setDraft(next.enabled);
    } catch (e) {
      setError(String(e));
    } finally {
      setEnablingId(null);
    }
  }

  const chosen =
    settings?.agents.filter((a) => settings.enabled.includes(a.id)) ?? [];
  const usageChosen = chosen.filter((a) => supportsLocalUsage(a.id));
  const activeCollectors = usageChosen.filter(
    (a) => usageStates[a.id] === "active",
  );
  const partialCollectors = usageChosen.filter(
    (a) => usageStates[a.id] === "partial",
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
  const overviewAgents = [...found].sort((a, b) => {
    const rank = (x: Agent) => {
      const isEn = settings?.enabled.includes(x.id) ?? false;
      if (isEn && x.status === "installed") return 0;
      if (isEn) return 1;
      if (x.status === "installed") return 2;
      if (x.status === "data") return 3;
      return 4;
    };
    return rank(a) - rank(b);
  });
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
                  : "用量与限额"}
            </h1>
          </div>
          {view === "overview" && usageChosen.length > 0 && (
            allCollectorsFailed ? (
              <span className="live-pill pill-error" data-live-state="error">
                <span className="dot stale" aria-hidden="true" />
                本地采集读取失败
              </span>
            ) : partialCollectors.length > 0 ? (
              <span className="live-pill pill-warn" data-live-state="partial">
                <span className="dot warn" aria-hidden="true" />
                部分采集受限（有日志不可读）·{" "}
                {partialCollectors.map((a) => a.name).join("、")}
                {activeCollectors.length > 0
                  ? ` · 正常：${activeCollectors.map((a) => a.name).join("、")}`
                  : ""}
              </span>
            ) : activeCollectors.length > 0 ? (
              <span className="live-pill" data-live-state="active">
                <span className="dot live pulse" aria-hidden="true" />
                本地增量监听中 ·{" "}
                {activeCollectors.map((a) => a.name).join("、")}
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
                const dbHistoryOnly = a.status === "history" && !checked;
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
                      <span
                        className={`state state-${a.status === "history" ? "data" : a.status}`}
                      >
                        {statusText(a)}
                      </span>
                      {dbHistoryOnly ? (
                        <button
                          type="button"
                          className="btn-secondary btn-sm"
                          onClick={(e) => {
                            e.preventDefault();
                            setHistoryProvider(a.id);
                            setView("history");
                          }}
                        >
                          查看历史
                        </button>
                      ) : (
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
                      )}
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
          <HistoryView
            active={view === "history"}
            initialProvider={historyProvider}
          />
        )}

        {settings?.confirmed && (
          <div hidden={view !== "overview"}>
            {overviewAgents.length > 0 ? (
              <>
                <div className="section-title" style={{ marginTop: 0 }}>
                  <h2>已发现平台</h2>
                  <span className="count-pill">
                    共 {overviewAgents.length} 个平台 · 已启用 {chosen.length} 个
                  </span>
                </div>
                <div className="overview-stack">
                  {overviewAgents.map((a) => (
                    <PlatformCard
                      key={a.id}
                      agent={a}
                      enabled={settings.enabled.includes(a.id)}
                      enabling={enablingId === a.id}
                      onEnable={(id) => void enableFromOverview(id)}
                      onOpenHistory={(id) => {
                        setHistoryProvider(id);
                        setView("history");
                        setError("");
                      }}
                      onStatusChange={handleUsageStatus}
                    />
                  ))}
                </div>
              </>
            ) : (
              <section className="panel empty-state">
                <h2>尚未发现平台</h2>
                <p className="muted">
                  安装 Codex、Antigravity 或 Claude Code 等工具后，点击右上角「重新扫描」。
                </p>
              </section>
            )}
          </div>
        )}
      </main>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
