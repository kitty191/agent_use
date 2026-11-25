import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import "./style.css";

type Agent = { id: string; name: string; status: string };
type Settings = { agents: Agent[]; enabled: string[]; confirmed: boolean };
type Usage = {
  today: number | null;
  latest: number | null;
  latest_at: number | null;
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
  ];
const capability = (id: string) =>
  id === "codex"
    ? "本机 Token · 联网查询账号额度"
    : id === "claude"
      ? "本机 Token · 离线读取"
      : id === "antigravity"
        ? "安装状态 · 账号/额度/重置/官方 Token 暂不可查"
        : "安装状态 · 用量来源待验证";
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
      <header>
        <div>
          <span className="eyebrow">账号额度</span>
          <h2>OpenAI Codex</h2>
        </div>
        <button onClick={refresh} disabled={busy || now * 1000 < retryAt}>
          {busy ? "正在查询…" : "刷新额度"}
        </button>
      </header>
      {error && (
        <p role="status" className="notice">
          {error}
        </p>
      )}
      {quota ? (
        <>
          <p className="account">
            {fresh ? "查询账号" : "上次成功查询的账号"} · {quota.account}{" "}
            {quota.plan && <strong>{quota.plan.toUpperCase()}</strong>}
          </p>
          {quota.windows.length === 0 && <p>当前账号未返回额度窗口</p>}
          {quota.windows.map((w, i) => {
            const remaining = Math.max(0, 100 - w.used);
            return (
              <div className="window" key={w.pool + i}>
                <div className="window-head">
                  <span>
                    {duration(w.minutes)} <small>{w.pool}</small>
                  </span>
                  <span>
                    剩余 <b>{remaining.toLocaleString()}%</b>
                  </span>
                </div>
                <progress
                  aria-label={duration(w.minutes) + "剩余额度"}
                  max="100"
                  value={remaining}
                />
                <div className="timing">
                  <span>
                    {w.reset == null
                      ? "重置时间未知"
                      : "重置时间 " + formatTime(w.reset)}
                  </span>
                  {w.reset != null && <span>{countdown(w.reset - now)}</span>}
                </div>
              </div>
            );
          })}
          <footer>
            {fresh ? "最近查询成功" : "历史快照"} ·{" "}
            {formatTime(quota.checked_at)}
          </footer>
        </>
      ) : (
        <p className="muted">
          {busy ? "正在通过本机 Codex 查询…" : "暂无已确认的额度数据"}
        </p>
      )}
    </section>
  );
}

function UsagePanel({ agent }: { agent: Agent }) {
  const [usage, setUsage] = useState<Usage | null>(null),
    [err, setErr] = useState("");
  const supported = ["codex", "claude"].includes(agent.id);
  useEffect(() => {
    if (!supported) return;
    let active = true, reading = false, timer = 0;
    async function read() {
      if (!active || reading) return;
      window.clearTimeout(timer);
      reading = true;
      try {
        const u = await invoke<Usage>("local_usage", { id: agent.id });
        if (active) {
          setUsage(u);
          setErr("");
        }
      } catch (e) {
        if (active) setErr(String(e));
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
      window.removeEventListener("focus", onVisible);
      document.removeEventListener("visibilitychange", onVisible);
      window.clearTimeout(timer);
    };
  }, [agent.id, supported]);
  return (
    <article className="usage panel" data-provider={agent.id} data-latest-at={usage?.latest_at}>
      <header>
        <h3>{agent.name}</h3>
        <span className="state">{statusText(agent)}</span>
      </header>
      {supported ? (
        <>
          <div className="metrics">
            <div>
              <small>今日已记录 Token</small>
              <strong>{usage?.today?.toLocaleString() ?? "—"}</strong>
            </div>
            <div>
              <small>最近一次响应</small>
              <strong>{usage?.latest?.toLocaleString() ?? "—"}</strong>
            </div>
          </div>
          <footer>
            {err ||
              (usage?.latest_at
                ? "最近用量 " + formatTime(usage.latest_at)
                : usage
                  ? "尚未读到用量记录"
                  : "读取本地记录中…")}
          </footer>
        </>
      ) : agent.id === "antigravity" ? (
        <p className="muted">
          当前版本仅展示上方发现状态；账号归属、模型/额度池、剩余额度、重置时间与官方
          Token 明细暂不可查（桌面端无受支持的只读查询接口）。
        </p>
      ) : (
        <p className="muted">用量与额度来源待验证</p>
      )}
    </article>
  );
}

function App() {
  const [settings, setSettings] = useState<Settings | null>(null),
    [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<string[]>([]),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const [newIds, setNewIds] = useState<string[]>([]);
  async function load(rescan: boolean) {
    setBusy(true);
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
      if (rescan || !next.confirmed) setEditing(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  useEffect(() => {
    void load(false);
  }, []);
  async function save() {
    setBusy(true);
    setError("");
    try {
      const next = await invoke<Settings>("save_selection", { ids: draft });
      setSettings(next);
      setEditing(false);
      setNewIds([]);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  const chosen =
    settings?.agents.filter((a) => settings.enabled.includes(a.id)) ?? [];
  const found =
    settings?.agents.filter(
      (a) => a.status !== "missing" || settings.enabled.includes(a.id),
    ) ?? [];
  return (
    <>
      <nav>
        <div className="brand">
          <span className="mark" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
          Agent Use
        </div>
        <button
          disabled={!settings?.confirmed || busy}
          aria-current={!editing ? "page" : undefined}
          onClick={() => {
            setEditing(false);
            setError("");
          }}
        >
          用量总览
        </button>
        <button
          disabled={!settings || busy}
          aria-current={editing ? "page" : undefined}
          onClick={() => {
            setDraft(settings?.enabled ?? []);
            setEditing(true);
            setError("");
          }}
        >
          平台管理
        </button>
      </nav>
      <main>
        <div className="page-title">
          <div>
            <span className="eyebrow">本机工作区</span>
            <h1>{editing ? "平台管理" : "用量总览"}</h1>
          </div>
          {settings && (
            <button disabled={busy} onClick={() => void load(true)}>
              {busy ? "处理中…" : "重新扫描"}
            </button>
          )}
        </div>
        {error && (
          <p role="alert" className="notice">
            {error}{" "}
            {!settings && (
              <button onClick={() => void load(false)}>重试</button>
            )}
          </p>
        )}
        {!settings && !error && <p>正在载入本机平台…</p>}
        {editing && settings && (
          <section className="panel settings">
            <header>
              <div>
                <h2>选择要监控的平台</h2>
                <p className="muted">
                  已发现 {found.filter((a) => a.status !== "missing").length}{" "}
                  个平台。新增工具后可重新扫描。
                </p>
              </div>
            </header>
            {found.map((a) => (
              <label className="row" key={a.id}>
                <input
                  type="checkbox"
                  checked={draft.includes(a.id)}
                  onChange={(e) =>
                    setDraft(
                      e.target.checked
                        ? [...draft, a.id]
                        : draft.filter((id) => id !== a.id),
                    )
                  }
                />
                <span>
                  <b>{a.name}</b>
                  <small>{capability(a.id)}</small>
                </span>
                <em>
                  {newIds.includes(a.id) ? "新发现 · " : ""}
                  {statusText(a)}
                </em>
              </label>
            ))}
            {settings.agents.some((a) => !found.includes(a)) && (
              <details>
                <summary>未检测到的平台</summary>
                <p>
                  {settings.agents
                    .filter((a) => !found.includes(a))
                    .map((a) => a.name)
                    .join(" · ")}
                </p>
              </details>
            )}
            <div className="actions">
              <button className="primary" disabled={busy} onClick={save}>
                {settings.confirmed ? "保存选择" : "确认选择"}
              </button>
            </div>
          </section>
        )}
        {settings?.confirmed && (
          <div hidden={editing}>
            {chosen.some((a) => a.id === "codex") && <QuotaPanel />}
            {chosen.length > 0 ? (
              <>
                <div className="section-title">
                  <h2>本机用量</h2>
                  <small>{chosen.length} 个已选平台</small>
                </div>
                <div className="cards">
                  {chosen.map((a) => (
                    <UsagePanel key={a.id} agent={a} />
                  ))}
                </div>
              </>
            ) : (
              <section className="panel">
                <h2>尚未启用平台</h2>
                <p>前往平台管理，选择需要查看的本机工具。</p>
                <button
                  onClick={() => {
                    setDraft([]);
                    setEditing(true);
                  }}
                >
                  选择平台
                </button>
              </section>
            )}
          </div>
        )}
      </main>
    </>
  );
}
createRoot(document.getElementById("root")!).render(<App />);
