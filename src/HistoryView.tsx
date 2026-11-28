import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

function localDateKey(d = new Date()) {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

export type Granularity = "day" | "week" | "month" | "year";

export type TokenParts = {
  input: number | null;
  cache_read: number | null;
  output: number | null;
};

export type ProviderOption = {
  id: string;
  name: string;
  unit: string;
  enabled: boolean;
};

export type ProviderBreakdown = {
  id: string;
  name: string;
  unit: string;
  verified_tokens: number | null;
  verified_steps: number;
  incomplete_steps: number;
  incomplete_known: TokenParts | null;
  legacy_steps: number;
  legacy_tokens: number | null;
};

export type HistoryBucket = {
  key: string;
  label: string;
  start_date: string;
  end_date: string;
  is_current: boolean;
  verified_tokens: number | null;
  verified_steps: number;
  incomplete_steps: number;
  incomplete_known: TokenParts | null;
  legacy_steps: number;
  legacy_tokens: number | null;
  providers: ProviderBreakdown[];
};

export type HistorySummary = {
  verified_tokens: number | null;
  verified_steps: number;
  incomplete_steps: number;
  incomplete_known: TokenParts | null;
  legacy_steps: number;
  legacy_tokens: number | null;
  providers: ProviderBreakdown[];
};

export type HistoryPage = {
  granularity: Granularity;
  bucket_granularity: Granularity;
  is_current: boolean;
  offset: number;
  provider: string | null;
  range_start: string;
  range_end: string;
  earliest_date: string | null;
  has_older: boolean;
  has_newer: boolean;
  available_providers: ProviderOption[];
  summary: HistorySummary;
  buckets: HistoryBucket[];
  drill_key?: string | null;
  drill?: HistoryPage | null;
};

const PROVIDER_COLORS: Record<string, string> = {
  codex: "var(--chart-codex)",
  antigravity: "var(--chart-antigravity)",
  claude: "var(--chart-claude)",
};

function providerColor(id: string) {
  return PROVIDER_COLORS[id] ?? "var(--chart-other)";
}

function formatCompact(n: number) {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(n >= 10_000_000 ? 1 : 2) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(n >= 100_000 ? 0 : 1) + "K";
  return n.toLocaleString();
}

function formatPartsText(parts: TokenParts | null) {
  if (!parts) return "分项全部未知";
  const items: string[] = [];
  if (parts.input != null) items.push(`输入 ${parts.input.toLocaleString()}`);
  if (parts.cache_read != null)
    items.push(`缓存读取 ${parts.cache_read.toLocaleString()}`);
  if (parts.output != null) items.push(`输出 ${parts.output.toLocaleString()}`);
  return items.length > 0 ? items.join(" · ") : "分项全部未知";
}

type BucketKind = "verified" | "zero" | "incomplete" | "legacy" | "empty";

function bucketKind(b: {
  verified_tokens: number | null;
  incomplete_steps: number;
  legacy_steps: number;
}): BucketKind {
  if (b.verified_tokens != null) {
    return b.verified_tokens === 0 ? "zero" : "verified";
  }
  if (b.incomplete_steps > 0) return "incomplete";
  if (b.legacy_steps > 0) return "legacy";
  return "empty";
}

function primaryValueLabel(b: {
  verified_tokens: number | null;
  incomplete_steps: number;
  legacy_steps: number;
}) {
  if (b.verified_tokens != null) return b.verified_tokens.toLocaleString();
  if (b.incomplete_steps > 0) return "用量不完整";
  if (b.legacy_steps > 0) return "仅含旧版记录";
  return "无已记录用量";
}

function formatBucketPeriod(b: HistoryBucket, granularity: Granularity) {
  if (granularity === "day") return b.start_date;
  if (granularity === "week") return `${b.start_date} ~ ${b.end_date}`;
  return b.label;
}

function formatRangeSummary(page: HistoryPage) {
  if (page.granularity === "day") {
    return page.is_current ? `今天 · ${page.range_start}` : page.range_start;
  }
  const prefix = page.is_current
    ? page.granularity === "week"
      ? "本周 · "
      : page.granularity === "month"
        ? "本月 · "
        : "今年 · "
    : "";
  return `${prefix}${page.range_start} 至 ${page.range_end}`;
}

function formatChartTitle(page: HistoryPage) {
  if (page.granularity === "week") {
    return page.is_current
      ? "本周逐日趋势"
      : `${page.range_start} 至 ${page.range_end} 逐日趋势`;
  }
  if (page.granularity === "month") {
    if (page.is_current) return "本月逐日趋势";
    const [year, month] = page.range_start.split("-");
    return `${year}年${Number(month)}月逐日趋势`;
  }
  return page.is_current
    ? "今年逐月趋势"
    : `${page.range_start.slice(0, 4)}年逐月趋势`;
}

function pagerLabels(granularity: Granularity) {
  switch (granularity) {
    case "day":
      return { older: "上一天", newer: "下一天", current: "回到今天" };
    case "week":
      return { older: "上一周", newer: "下一周", current: "回到本周" };
    case "month":
      return { older: "上个月", newer: "下个月", current: "回到本月" };
    case "year":
      return { older: "上一年", newer: "下一年", current: "回到今年" };
  }
}

function TrendChart({
  buckets,
  granularity,
  selectedKey,
  onSelect,
  compact,
}: {
  buckets: HistoryBucket[];
  granularity: Granularity;
  selectedKey?: string | null;
  onSelect?: (bucket: HistoryBucket) => void;
  compact?: boolean;
}) {
  const [hoveredKey, setHoveredKey] = useState<string | null>(null);
  const maxTokens = Math.max(
    0,
    ...buckets.map((b) => (b.verified_tokens != null ? b.verified_tokens : 0)),
  );
  const activeBucket =
    buckets.find((b) => b.key === hoveredKey) ??
    buckets.find((b) => b.key === selectedKey) ??
    null;

  const chartHeight = compact ? 132 : 186;
  const padTop = 18;
  const padBottom = 28;
  const padLeft = 48;
  const padRight = 14;
  const innerH = chartHeight - padTop - padBottom;
  const viewWidth = Math.max(600, buckets.length * (compact ? 26 : 30) + padLeft + padRight);
  const innerW = viewWidth - padLeft - padRight;
  const slotW = buckets.length > 0 ? innerW / buckets.length : innerW;
  const barW = Math.max(8, Math.min(34, slotW * 0.62));
  const labelStep = buckets.length > 16 ? Math.ceil(buckets.length / 8) : 1;

  return (
    <div className="chart-wrap">
      <div className="chart-readout">
        {activeBucket ? (
          <>
            <strong>
              {formatBucketPeriod(activeBucket, granularity)}
              {activeBucket.is_current && granularity !== "day" ? "（截至今天）" : ""}
            </strong>
            <span className="readout-val">
              已核验完整：{primaryValueLabel(activeBucket)}
            </span>
            {activeBucket.providers.length > 0 && (
              <span className="readout-parts">
                {activeBucket.providers
                  .map(
                    (p) =>
                      `${p.name} ${primaryValueLabel(p)}${p.verified_steps > 0 ? ` (${p.verified_steps}${p.unit})` : ""}`,
                  )
                  .join(" · ")}
              </span>
            )}
            {activeBucket.incomplete_steps > 0 && (
              <span className="readout-warn">
                不完整 {activeBucket.incomplete_steps} 步
              </span>
            )}
            {activeBucket.legacy_steps > 0 && (
              <span className="readout-warn">
                旧版 {activeBucket.legacy_steps} 步（{activeBucket.legacy_tokens?.toLocaleString()}）
              </span>
            )}
          </>
        ) : (
          <span className="muted">
            {onSelect
              ? granularity === "year"
                ? "悬停柱状图查看精确数值，点击任意年份展开每月明细"
                : "悬停柱状图查看精确数值，点击任意周期展开每日明细"
              : granularity === "month"
                ? "悬停柱状图查看当月精确数值与平台分项"
                : "悬停柱状图查看当日精确数值与平台分项"}
          </span>
        )}
      </div>

      <svg
        className="trend-svg"
        viewBox={`0 0 ${viewWidth} ${chartHeight}`}
        role="img"
        aria-label="历史 Token 用量趋势图"
      >
        {[0, 0.5, 1].map((ratio) => {
          const y = padTop + innerH * (1 - ratio);
          const val = Math.round(maxTokens * ratio);
          return (
            <g key={ratio}>
              <line
                x1={padLeft}
                y1={y}
                x2={viewWidth - padRight}
                y2={y}
                className="chart-grid"
              />
              <text x={padLeft - 8} y={y + 4} textAnchor="end" className="chart-axis">
                {maxTokens > 0 ? formatCompact(val) : ratio === 0 ? "0" : ""}
              </text>
            </g>
          );
        })}

        {buckets.map((b, i) => {
          const cx = padLeft + i * slotW + slotW / 2;
          const x = cx - barW / 2;
          const baseY = padTop + innerH;
          const kind = bucketKind(b);
          const isSelected = selectedKey === b.key;
          const isHovered = hoveredKey === b.key;
          const clickable = Boolean(onSelect);
          const totalH =
            b.verified_tokens != null && maxTokens > 0 && b.verified_tokens > 0
              ? Math.max(4, (b.verified_tokens / maxTokens) * innerH)
              : 0;

          // Build stacked segments for verified complete tokens per provider
          let stackY = baseY;
          const segments =
            totalH > 0
              ? b.providers
                  .filter((p) => p.verified_tokens != null && p.verified_tokens > 0)
                  .map((p) => {
                    const segH =
                      b.verified_tokens && b.verified_tokens > 0
                        ? ((p.verified_tokens ?? 0) / b.verified_tokens) * totalH
                        : 0;
                    stackY -= segH;
                    return { id: p.id, y: stackY, h: segH };
                  })
              : [];

          const showTick =
            i % labelStep === 0 || i === buckets.length - 1;

          return (
            <g
              key={b.key}
              className={clickable ? "chart-col clickable" : "chart-col"}
              onMouseEnter={() => setHoveredKey(b.key)}
              onMouseLeave={() => setHoveredKey(null)}
              onClick={() => onSelect?.(b)}
            >
              <rect
                x={padLeft + i * slotW}
                y={padTop}
                width={slotW}
                height={innerH}
                className={
                  isSelected
                    ? "chart-hit selected"
                    : isHovered
                      ? "chart-hit hovered"
                      : "chart-hit"
                }
              />

              {kind === "verified" &&
                segments.map((seg, idx) => (
                  <rect
                    key={seg.id}
                    x={x}
                    y={seg.y}
                    width={barW}
                    height={Math.max(1, seg.h)}
                    rx={idx === segments.length - 1 ? 4 : 0}
                    fill={providerColor(seg.id)}
                    opacity={isHovered || isSelected ? 1 : 0.88}
                  />
                ))}

              {kind === "zero" && (
                <rect
                  x={x}
                  y={baseY - 4}
                  width={barW}
                  height={4}
                  rx={2}
                  className="chart-zero-bar"
                />
              )}

              {(kind === "incomplete" || kind === "legacy") && (
                <rect
                  x={x}
                  y={baseY - 12}
                  width={barW}
                  height={12}
                  rx={3}
                  className={
                    kind === "incomplete"
                      ? "chart-incomplete-bar"
                      : "chart-legacy-bar"
                  }
                />
              )}

              {kind === "empty" && (
                <line
                  x1={x + 2}
                  y1={baseY - 1}
                  x2={x + barW - 2}
                  y2={baseY - 1}
                  className="chart-empty-tick"
                />
              )}

              {kind === "verified" &&
                (b.incomplete_steps > 0 || b.legacy_steps > 0) && (
                  <circle
                    cx={cx}
                    cy={Math.max(padTop + 4, baseY - totalH - 6)}
                    r={3.2}
                    className="chart-flag-dot"
                  />
                )}

              {showTick && (
                <text
                  x={cx}
                  y={chartHeight - 8}
                  textAnchor="middle"
                  className={isSelected ? "chart-axis active" : "chart-axis"}
                >
                  {granularity === "week"
                    ? b.start_date.slice(5)
                    : granularity === "month"
                      ? b.label.replace(/^\d+年/, "")
                      : granularity === "year"
                        ? b.key
                        : b.label}
                </text>
              )}
            </g>
          );
        })}
      </svg>

      <div className="chart-legend">
        <span>
          <i className="legend-swatch" style={{ background: "var(--chart-codex)" }} />
          OpenAI Codex
        </span>
        <span>
          <i
            className="legend-swatch"
            style={{ background: "var(--chart-antigravity)" }}
          />
          Antigravity
        </span>
        <span>
          <i className="legend-swatch" style={{ background: "var(--chart-claude)" }} />
          Claude Code
        </span>
        <span>
          <i className="legend-swatch swatch-zero" />
          明确为 0
        </span>
        <span>
          <i className="legend-swatch swatch-incomplete" />
          用量不完整 / 仅旧版记录
        </span>
        <span>
          <i className="legend-swatch swatch-empty" />
          无已记录用量
        </span>
      </div>
    </div>
  );
}

function HistoryTable({
  buckets,
  granularity,
  selectedKey,
  onSelect,
}: {
  buckets: HistoryBucket[];
  granularity: Granularity;
  selectedKey?: string | null;
  onSelect?: (bucket: HistoryBucket) => void;
}) {
  const ordered = [...buckets].reverse();
  return (
    <div className="table-wrap">
      <table className="history-table">
        <thead>
          <tr>
            <th>周期</th>
            <th className="num">已核验完整 Token</th>
            <th>平台分项</th>
            <th>补充口径</th>
          </tr>
        </thead>
        <tbody>
          {ordered.map((b) => {
            const kind = bucketKind(b);
            const selected = selectedKey === b.key;
            const clickable = Boolean(onSelect);
            return (
              <tr
                key={b.key}
                className={
                  selected
                    ? "row-active"
                    : kind === "empty"
                      ? "row-empty"
                      : undefined
                }
              >
                <td className="period-cell">
                  <div className="period-main">
                    {clickable ? (
                      <button
                        type="button"
                        className="period-link"
                        onClick={() => onSelect?.(b)}
                      >
                        <span>{formatBucketPeriod(b, granularity)}</span>
                        <span className="drill-cue">
                          {selected
                            ? "已展开"
                            : granularity === "year"
                              ? "查看每月 ›"
                              : "查看每日 ›"}
                        </span>
                      </button>
                    ) : (
                      <span>{formatBucketPeriod(b, granularity)}</span>
                    )}
                    {b.is_current && (
                      <span className="current-badge">
                        {granularity === "day" ? "今天" : "截至今天"}
                      </span>
                    )}
                  </div>
                </td>
                <td className="num">
                  <span className={`val-badge val-${kind}`}>
                    {primaryValueLabel(b)}
                  </span>
                </td>
                <td>
                  {b.providers.length > 0 ? (
                    <div className="provider-chips">
                      {b.providers.map((p) => (
                        <span className="provider-chip" key={p.id}>
                          <i
                            className="chip-dot"
                            style={{ background: providerColor(p.id) }}
                          />
                          <b>{p.name}</b>
                          <span className="chip-val">
                            {primaryValueLabel(p)}
                          </span>
                          {p.verified_steps > 0 && (
                            <small>
                              {p.verified_steps} {p.unit}
                            </small>
                          )}
                        </span>
                      ))}
                    </div>
                  ) : (
                    <span className="muted-dash">—</span>
                  )}
                </td>
                <td>
                  {b.incomplete_steps > 0 || b.legacy_steps > 0 ? (
                    <details className="caveat-details">
                      <summary>
                        {b.incomplete_steps > 0 && (
                          <span className="caveat-pill caveat-incomplete">
                            不完整 {b.incomplete_steps} 步
                          </span>
                        )}
                        {b.legacy_steps > 0 && (
                          <span className="caveat-pill caveat-legacy">
                            旧版 {b.legacy_steps} 步
                          </span>
                        )}
                      </summary>
                      <div className="caveat-body">
                        {b.incomplete_steps > 0 && (
                          <p>
                            不完整步骤已知分项：{formatPartsText(b.incomplete_known)}
                          </p>
                        )}
                        {b.legacy_steps > 0 && (
                          <p>
                            旧版记录值（原日志已移除，未核验）：
                            {b.legacy_tokens?.toLocaleString() ?? "—"} Token（
                            {b.legacy_steps} 条）
                          </p>
                        )}
                      </div>
                    </details>
                  ) : (
                    <span className="muted-dash">—</span>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export function HistoryView({
  active,
  initialProvider,
}: {
  active: boolean;
  initialProvider?: string | null;
}) {
  const [granularity, setGranularity] = useState<Granularity>("day");
  const [offset, setOffset] = useState(0);
  const [provider, setProvider] = useState<string>(initialProvider || "all");
  const [pageState, setPageState] = useState<{ key: string; data: HistoryPage } | null>(null);
  const [availableProviders, setAvailableProviders] = useState<ProviderOption[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    if (initialProvider) {
      setProvider(initialProvider);
      setOffset(0);
    }
  }, [initialProvider]);

  const mountedRef = useRef(true);
  const reqIdRef = useRef(0);
  const lastLocalDateRef = useRef(localDateKey());

  const activeQueryKey = `${granularity}|${offset}|${provider}`;
  const page = pageState && pageState.key === activeQueryKey ? pageState.data : null;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      reqIdRef.current += 1;
    };
  }, []);

  const fetchSnapshot = useCallback(
    async ({ silent }: { silent: boolean }) => {
      const reqId = ++reqIdRef.current;
      const queryKey = `${granularity}|${offset}|${provider}`;
      if (!silent) {
        setLoading(true);
        setError("");
      }
      try {
        const data = await invoke<HistoryPage>("usage_history", {
          granularity,
          offset,
          provider: provider === "all" ? null : provider,
          rangeStart: null,
          rangeEnd: null,
          drillKey: null,
        });
        if (!mountedRef.current || reqId !== reqIdRef.current) return;
        setAvailableProviders(data.available_providers);
        setPageState({ key: queryKey, data });
        setError("");
        // Any superseding snapshot (including silent background poll) resolves foreground loading.
        setLoading(false);
      } catch (e) {
        if (!mountedRef.current || reqId !== reqIdRef.current) return;
        setLoading(false);
        setError(String(e));
      }
    },
    [granularity, offset, provider],
  );

  // Initial load whenever view activates or granularity / offset / provider changes.
  useEffect(() => {
    if (!active) return;
    lastLocalDateRef.current = localDateKey();
    void fetchSnapshot({ silent: false });
  }, [active, granularity, offset, provider, fetchSnapshot]);

  // Periodic read-only SQLite refresh + immediate midnight rollover detection while History is active.
  useEffect(() => {
    if (!active) return;
    let inFlight = false;
    const tick = async () => {
      const todayKey = localDateKey();
      const crossedMidnight = todayKey !== lastLocalDateRef.current;
      if (crossedMidnight) {
        lastLocalDateRef.current = todayKey;
      }
      if (document.visibilityState !== "visible" && !crossedMidnight) return;
      if (inFlight) return;
      inFlight = true;
      try {
        await fetchSnapshot({ silent: true });
      } finally {
        inFlight = false;
      }
    };
    const timer = window.setInterval(() => {
      void tick();
    }, 1500);
    const onVisible = () => {
      if (document.visibilityState === "visible") void tick();
    };
    window.addEventListener("focus", onVisible);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("focus", onVisible);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [active, fetchSnapshot]);

  const summary = page?.summary;
  const providersList = page?.available_providers ?? availableProviders;
  const labels = pagerLabels(granularity);
  const detailGranularity: Granularity = page?.bucket_granularity ?? (granularity === "year" ? "month" : "day");

  return (
    <div className="history-view">
      <section className="panel history-toolbar">
        <div className="toolbar-row">
          <div className="segmented" role="tablist" aria-label="时间粒度">
            {(
              [
                ["day", "按日"],
                ["week", "按周"],
                ["month", "按月"],
                ["year", "按年"],
              ] as const
            ).map(([g, label]) => (
              <button
                key={g}
                type="button"
                aria-current={granularity === g ? "true" : undefined}
                onClick={() => {
                  setGranularity(g);
                  setOffset(0);
                }}
              >
                {label}
              </button>
            ))}
          </div>

          <div className="filter-pills" role="group" aria-label="平台筛选">
            <button
              type="button"
              className={provider === "all" ? "filter-pill active" : "filter-pill"}
              onClick={() => {
                setProvider("all");
                setOffset(0);
              }}
            >
              全部有历史平台
            </button>
            {providersList.map((p) => (
              <button
                key={p.id}
                type="button"
                className={provider === p.id ? "filter-pill active" : "filter-pill"}
                onClick={() => {
                  setProvider(p.id);
                  setOffset(0);
                }}
              >
                <i
                  className="chip-dot"
                  style={{ background: providerColor(p.id) }}
                />
                <span>{p.name}</span>
                {!p.enabled && <small className="disabled-tag">已停用</small>}
              </button>
            ))}
          </div>
        </div>

        <div className="pager-row">
          <div className="pager-info">
            <span className="range-text">
              {page ? formatRangeSummary(page) : error ? "加载失败" : "载入中…"}
            </span>
            {!page && loading && <span className="count-pill">查询中…</span>}
            {page?.earliest_date && (
              <small className="earliest-note">
                最早记录 {page.earliest_date}
              </small>
            )}
          </div>
          <div className="pager-actions">
            <button
              type="button"
              className="btn-secondary btn-sm"
              disabled={loading || !page?.has_older}
              onClick={() => setOffset((o) => o + 1)}
            >
              ← {labels.older}
            </button>
            <button
              type="button"
              className="btn-secondary btn-sm"
              disabled={loading || !page?.has_newer}
              onClick={() => setOffset((o) => Math.max(0, o - 1))}
            >
              {labels.newer} →
            </button>
            {offset > 0 && (
              <button
                type="button"
                className="btn-ghost btn-sm"
                disabled={loading}
                onClick={() => setOffset(0)}
              >
                {labels.current}
              </button>
            )}
          </div>
        </div>
      </section>

      {error && (
        <p role="alert" className="notice">
          <span>{error}</span>
          <button
            className="btn-secondary btn-sm"
            onClick={() => void fetchSnapshot({ silent: false })}
          >
            重试
          </button>
        </p>
      )}

      {summary && (
        <div className="history-kpis">
          <div className="panel kpi-card">
            <small>本周期已核验完整 Token</small>
            <strong
              className={
                summary.verified_tokens == null && summary.incomplete_steps > 0
                  ? "metric-incomplete"
                  : undefined
              }
            >
              {primaryValueLabel(summary)}
            </strong>
            <span className="kpi-sub">
              {page.is_current && granularity !== "day"
                ? "统计至本地今天（与下方明细同源）"
                : "仅含已核验完整记录（按本地日历汇总）"}
            </span>
          </div>

          <div className="panel kpi-card">
            <small>平台构成（保留各自单位）</small>
            {summary.providers.length > 0 ? (
              <div className="kpi-providers">
                {summary.providers.map((p) => (
                  <div className="kpi-prov-row" key={p.id}>
                    <span className="kpi-prov-name">
                      <i
                        className="chip-dot"
                        style={{ background: providerColor(p.id) }}
                      />
                      {p.name}
                    </span>
                    <span className="kpi-prov-val">
                      {primaryValueLabel(p)}
                      {p.verified_steps > 0 && (
                        <small>
                          {" "}
                          · {p.verified_steps} {p.unit}
                        </small>
                      )}
                    </span>
                  </div>
                ))}
              </div>
            ) : (
              <span className="kpi-sub">本周期无记录</span>
            )}
          </div>

          <div className="panel kpi-card">
            <small>不完整与旧版补充口径</small>
            {summary.verified_steps === 0 &&
            summary.incomplete_steps === 0 &&
            summary.legacy_steps === 0 ? (
              <span className="kpi-sub">本周期无记录</span>
            ) : summary.incomplete_steps === 0 && summary.legacy_steps === 0 ? (
              <span className="kpi-sub kpi-ok">
                当前周期全部记录均已核验完整
              </span>
            ) : (
              <div className="kpi-caveats">
                {summary.incomplete_steps > 0 && (
                  <p>
                    不完整：{summary.incomplete_steps} 步（
                    {formatPartsText(summary.incomplete_known)}）
                  </p>
                )}
                {summary.legacy_steps > 0 && (
                  <p>
                    旧版未核验：{summary.legacy_steps} 条 ·{" "}
                    {summary.legacy_tokens?.toLocaleString() ?? "—"} Token
                  </p>
                )}
              </div>
            )}
          </div>
        </div>
      )}

      {page && granularity !== "day" && (
        <section className="panel history-chart-panel">
          <div className="section-head">
            <div>
              <h2>{formatChartTitle(page)}</h2>
              <small>
                图表、明细与上方合计来自同一次 SQLite 读取快照；
                {detailGranularity === "month"
                  ? "按本地自然月汇总"
                  : "按本地 00:00 划分自然日"}
              </small>
            </div>
            {loading && <span className="count-pill">查询中…</span>}
          </div>
          <TrendChart
            buckets={page.buckets}
            granularity={detailGranularity}
          />
        </section>
      )}

      {page && (
        <section className="panel history-table-panel">
          <div className="section-head">
            <div>
              <h2>
                {granularity === "day"
                  ? "当日明细"
                  : granularity === "year"
                    ? "逐月明细"
                    : "逐日明细"}
              </h2>
              <small>
                {granularity === "day"
                  ? `展示 ${page.range_start} 单日记录`
                  : granularity === "year"
                    ? `共 ${page.buckets.length} 个自然月`
                    : `共 ${page.buckets.length} 个自然日`}
              </small>
            </div>
            <div className="pager-actions">
              {granularity === "day" && loading && (
                <span className="count-pill">查询中…</span>
              )}
              <span className="count-pill">共 {page.buckets.length} 条</span>
            </div>
          </div>
          <HistoryTable
            buckets={page.buckets}
            granularity={detailGranularity}
          />
        </section>
      )}
    </div>
  );
}
