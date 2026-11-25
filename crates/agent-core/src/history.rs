use crate::{
    store::{Settings, Store},
    usage::TokenParts,
};
use anyhow::{bail, Context, Result};
use chrono::{Datelike, Duration, Local, LocalResult, NaiveDate, TimeZone};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    Day,
    Week,
    Month,
}

impl Granularity {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            _ => bail!("不支持的统计粒度: {raw}"),
        }
    }

    pub fn page_size(self) -> usize {
        match self {
            Self::Day => 30,
            Self::Week => 12,
            Self::Month => 12,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderOption {
    pub id: String,
    pub name: String,
    pub unit: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderBreakdown {
    pub id: String,
    pub name: String,
    pub unit: String,
    pub verified_tokens: Option<i64>,
    pub verified_steps: i64,
    pub incomplete_steps: i64,
    pub incomplete_known: Option<TokenParts>,
    pub legacy_steps: i64,
    pub legacy_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryBucket {
    pub key: String,
    pub label: String,
    pub start_date: String,
    pub end_date: String,
    pub is_current: bool,
    pub verified_tokens: Option<i64>,
    pub verified_steps: i64,
    pub incomplete_steps: i64,
    pub incomplete_known: Option<TokenParts>,
    pub legacy_steps: i64,
    pub legacy_tokens: Option<i64>,
    pub providers: Vec<ProviderBreakdown>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySummary {
    pub verified_tokens: Option<i64>,
    pub verified_steps: i64,
    pub incomplete_steps: i64,
    pub incomplete_known: Option<TokenParts>,
    pub legacy_steps: i64,
    pub legacy_tokens: Option<i64>,
    pub providers: Vec<ProviderBreakdown>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryPage {
    pub granularity: Granularity,
    pub offset: u32,
    pub provider: Option<String>,
    pub range_start: String,
    pub range_end: String,
    pub earliest_date: Option<String>,
    pub has_older: bool,
    pub has_newer: bool,
    pub available_providers: Vec<ProviderOption>,
    pub summary: HistorySummary,
    pub buckets: Vec<HistoryBucket>,
}

#[derive(Default, Clone)]
struct Accumulator {
    verified_steps: i64,
    verified_tokens: i64,
    incomplete_steps: i64,
    inc_in_cnt: i64,
    inc_in_sum: i64,
    inc_cr_cnt: i64,
    inc_cr_sum: i64,
    inc_out_cnt: i64,
    inc_out_sum: i64,
    legacy_steps: i64,
    legacy_tokens: i64,
}

impl Accumulator {
    fn add_row(
        &mut self,
        tokens: i64,
        complete: i64,
        input: Option<i64>,
        cache_read: Option<i64>,
        output: Option<i64>,
        legacy: i64,
    ) {
        if legacy == 1 {
            self.legacy_steps += 1;
            self.legacy_tokens = self.legacy_tokens.saturating_add(tokens);
        } else if complete == 1 {
            self.verified_steps += 1;
            self.verified_tokens = self.verified_tokens.saturating_add(tokens);
        } else {
            self.incomplete_steps += 1;
            if let Some(v) = input {
                self.inc_in_cnt += 1;
                self.inc_in_sum = self.inc_in_sum.saturating_add(v);
            }
            if let Some(v) = cache_read {
                self.inc_cr_cnt += 1;
                self.inc_cr_sum = self.inc_cr_sum.saturating_add(v);
            }
            if let Some(v) = output {
                self.inc_out_cnt += 1;
                self.inc_out_sum = self.inc_out_sum.saturating_add(v);
            }
        }
    }

    fn incomplete_known(&self) -> Option<TokenParts> {
        if self.incomplete_steps == 0
            || (self.inc_in_cnt == 0 && self.inc_cr_cnt == 0 && self.inc_out_cnt == 0)
        {
            return None;
        }
        Some(TokenParts {
            input: (self.inc_in_cnt > 0).then_some(self.inc_in_sum),
            cache_read: (self.inc_cr_cnt > 0).then_some(self.inc_cr_sum),
            output: (self.inc_out_cnt > 0).then_some(self.inc_out_sum),
        })
    }

    fn verified_tokens(&self) -> Option<i64> {
        (self.verified_steps > 0).then_some(self.verified_tokens)
    }

    fn legacy_tokens(&self) -> Option<i64> {
        (self.legacy_steps > 0).then_some(self.legacy_tokens)
    }

    fn to_breakdown(&self, id: &str) -> ProviderBreakdown {
        let (name, unit) = provider_meta(id);
        ProviderBreakdown {
            id: id.to_string(),
            name,
            unit,
            verified_tokens: self.verified_tokens(),
            verified_steps: self.verified_steps,
            incomplete_steps: self.incomplete_steps,
            incomplete_known: self.incomplete_known(),
            legacy_steps: self.legacy_steps,
            legacy_tokens: self.legacy_tokens(),
        }
    }
}

pub fn provider_meta(id: &str) -> (String, String) {
    match id {
        "codex" => ("OpenAI Codex".into(), "次响应".into()),
        "claude" => ("Claude Code".into(), "次响应".into()),
        "antigravity" => ("Antigravity".into(), "个模型步骤".into()),
        other => (other.to_string(), "条记录".into()),
    }
}

/// Computes the exact Unix timestamp (seconds) for local `00:00:00` of `date` in `tz`.
/// Handles DST transitions safely without assuming a fixed 86,400-second day.
pub fn local_day_start_sec<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Result<i64> {
    let midnight = date.and_hms_opt(0, 0, 0).context("无法构造本地零点")?;
    match tz.from_local_datetime(&midnight) {
        LocalResult::Single(dt) => Ok(dt.timestamp()),
        LocalResult::Ambiguous(earliest, _) => Ok(earliest.timestamp()),
        LocalResult::None => {
            // DST spring-forward gap right across midnight: step forward until first valid local instant.
            for mins in 1..=(24 * 60) {
                let candidate = midnight + Duration::minutes(mins);
                match tz.from_local_datetime(&candidate) {
                    LocalResult::Single(dt) => return Ok(dt.timestamp()),
                    LocalResult::Ambiguous(earliest, _) => return Ok(earliest.timestamp()),
                    LocalResult::None => continue,
                }
            }
            bail!("无法确定 {date} 的本地开始时间")
        }
    }
}

fn week_monday(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

fn shift_months(year: i32, month: u32, delta_months: i32) -> Result<(i32, u32)> {
    let zero_based = year * 12 + (month as i32 - 1) + delta_months;
    let next_year = zero_based.div_euclid(12);
    let next_month = (zero_based.rem_euclid(12) + 1) as u32;
    Ok((next_year, next_month))
}

fn month_bounds(year: i32, month: u32) -> Result<(NaiveDate, NaiveDate, NaiveDate)> {
    let start = NaiveDate::from_ymd_opt(year, month, 1).context("无效月份")?;
    let (ny, nm) = shift_months(year, month, 1)?;
    let next_start = NaiveDate::from_ymd_opt(ny, nm, 1).context("无效下月")?;
    let end_inclusive = next_start.pred_opt().context("无效月末")?;
    Ok((start, end_inclusive, next_start))
}

struct BucketSpec {
    key: String,
    label: String,
    start_date: NaiveDate,
    end_date: NaiveDate,
    start_sec: i64,
    end_sec: i64,
    is_current: bool,
}

fn build_day_specs<Tz: TimeZone>(
    start: NaiveDate,
    end_inclusive: NaiveDate,
    today: NaiveDate,
    tz: &Tz,
) -> Result<Vec<BucketSpec>> {
    let mut specs = Vec::new();
    let mut d = start;
    while d <= end_inclusive {
        let next = d.succ_opt().context("日期溢出")?;
        specs.push(BucketSpec {
            key: d.format("%Y-%m-%d").to_string(),
            label: d.format("%m/%d").to_string(),
            start_date: d,
            end_date: d,
            start_sec: local_day_start_sec(d, tz)?,
            end_sec: local_day_start_sec(next, tz)?,
            is_current: d == today,
        });
        d = next;
    }
    Ok(specs)
}

fn build_specs<Tz: TimeZone>(
    granularity: Granularity,
    offset: u32,
    range_start: Option<NaiveDate>,
    range_end: Option<NaiveDate>,
    today: NaiveDate,
    tz: &Tz,
) -> Result<Vec<BucketSpec>> {
    if let (Some(rs), Some(re)) = (range_start, range_end) {
        if rs > re {
            bail!("起始日期不能晚于结束日期");
        }
        let span = (re - rs).num_days();
        if span > 366 {
            bail!("单次日期明细范围不能超过 366 天");
        }
        return build_day_specs(rs, re, today, tz);
    }

    let count = granularity.page_size();
    match granularity {
        Granularity::Day => {
            let end = today - Duration::days(i64::from(offset) * count as i64);
            let start = end - Duration::days(count as i64 - 1);
            build_day_specs(start, end, today, tz)
        }
        Granularity::Week => {
            let current_mon = week_monday(today);
            let end_mon = current_mon - Duration::days(i64::from(offset) * count as i64 * 7);
            let start_mon = end_mon - Duration::days((count as i64 - 1) * 7);
            let mut specs = Vec::with_capacity(count);
            for i in 0..count {
                let mon = start_mon + Duration::days(i as i64 * 7);
                let sun = mon + Duration::days(6);
                let next_mon = mon + Duration::days(7);
                let same_year = mon.year() == sun.year();
                let label = if same_year {
                    format!("{} – {}", mon.format("%m/%d"), sun.format("%m/%d"))
                } else {
                    format!("{} – {}", mon.format("%Y/%m/%d"), sun.format("%Y/%m/%d"))
                };
                specs.push(BucketSpec {
                    key: format!(
                        "{}..{}",
                        mon.format("%Y-%m-%d"),
                        sun.format("%Y-%m-%d")
                    ),
                    label,
                    start_date: mon,
                    end_date: sun,
                    start_sec: local_day_start_sec(mon, tz)?,
                    end_sec: local_day_start_sec(next_mon, tz)?,
                    is_current: today >= mon && today <= sun,
                });
            }
            Ok(specs)
        }
        Granularity::Month => {
            let (end_y, end_m) =
                shift_months(today.year(), today.month(), -(offset as i32 * count as i32))?;
            let (start_y, start_m) = shift_months(end_y, end_m, -(count as i32 - 1))?;
            let mut specs = Vec::with_capacity(count);
            for i in 0..count {
                let (y, m) = shift_months(start_y, start_m, i as i32)?;
                let (start, end, next_start) = month_bounds(y, m)?;
                specs.push(BucketSpec {
                    key: format!("{y:04}-{m:02}"),
                    label: format!("{y}年{m}月"),
                    start_date: start,
                    end_date: end,
                    start_sec: local_day_start_sec(start, tz)?,
                    end_sec: local_day_start_sec(next_start, tz)?,
                    is_current: today >= start && today <= end,
                });
            }
            Ok(specs)
        }
    }
}

pub fn query(
    store: &Store,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<&str>,
    range_end: Option<&str>,
) -> Result<HistoryPage> {
    let today = Local::now().date_naive();
    let rs = range_start
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").context("无效起始日期"))
        .transpose()?;
    let re = range_end
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").context("无效结束日期"))
        .transpose()?;
    query_with_tz(store, granularity, offset, provider, rs, re, today, &Local)
}

pub fn query_with_tz<Tz: TimeZone>(
    store: &Store,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<NaiveDate>,
    range_end: Option<NaiveDate>,
    today: NaiveDate,
    tz: &Tz,
) -> Result<HistoryPage> {
    let db = store.connect()?;
    let enabled_set: HashSet<String> = db
        .query_row("SELECT data FROM settings WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok())
        .map(|s| s.enabled.into_iter().collect())
        .unwrap_or_default();

    let mut available_providers = Vec::new();
    {
        let mut stmt = db.prepare("SELECT DISTINCT provider FROM usage ORDER BY provider")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let id = row?;
            let (name, unit) = provider_meta(&id);
            let enabled = enabled_set.contains(&id);
            available_providers.push(ProviderOption {
                id,
                name,
                unit,
                enabled,
            });
        }
    }

    let filter = provider.filter(|p| !p.is_empty() && *p != "all");
    let earliest_sec: Option<i64> = match filter {
        Some(p) => db
            .query_row("SELECT MIN(at) FROM usage WHERE provider=?1", [p], |r| {
                r.get::<_, Option<i64>>(0)
            })?,
        None => db.query_row("SELECT MIN(at) FROM usage", [], |r| {
            r.get::<_, Option<i64>>(0)
        })?,
    };
    let earliest_date = earliest_sec
        .and_then(|sec| tz.timestamp_opt(sec, 0).single())
        .map(|dt| dt.date_naive());

    let specs = build_specs(granularity, offset, range_start, range_end, today, tz)?;
    let window_start_sec = specs.first().map(|s| s.start_sec).unwrap_or(0);
    let window_end_sec = specs.last().map(|s| s.end_sec).unwrap_or(0);

    let mut bucket_totals: Vec<Accumulator> = vec![Accumulator::default(); specs.len()];
    let mut bucket_providers: Vec<BTreeMap<String, Accumulator>> =
        vec![BTreeMap::new(); specs.len()];
    let mut summary_total = Accumulator::default();
    let mut summary_providers: BTreeMap<String, Accumulator> = BTreeMap::new();

    // Query SQLite directly for [window_start_sec, window_end_sec) without touching logs or monitors.
    let mut process_row = |prov: String,
                           at: i64,
                           tokens: i64,
                           complete: i64,
                           input: Option<i64>,
                           cache_read: Option<i64>,
                           output: Option<i64>,
                           legacy: i64| {
        // Binary search the bucket where `start_sec <= at < end_sec`.
        let idx = specs.partition_point(|s| s.start_sec <= at);
        if idx == 0 {
            return;
        }
        let b = idx - 1;
        if at >= specs[b].end_sec {
            return;
        }
        bucket_totals[b].add_row(tokens, complete, input, cache_read, output, legacy);
        bucket_providers[b]
            .entry(prov.clone())
            .or_default()
            .add_row(tokens, complete, input, cache_read, output, legacy);
        summary_total.add_row(tokens, complete, input, cache_read, output, legacy);
        summary_providers
            .entry(prov)
            .or_default()
            .add_row(tokens, complete, input, cache_read, output, legacy);
    };

    if let Some(p) = filter {
        let mut stmt = db.prepare(
            "SELECT provider, at, tokens, complete, input_tokens, cache_read_tokens, output_tokens, legacy
             FROM usage
             WHERE provider=?1 AND at>=?2 AND at<?3",
        )?;
        let rows = stmt.query_map(params![p, window_start_sec, window_end_sec], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        for row in rows {
            let (prov, at, tokens, complete, inp, cr, out, legacy) = row?;
            process_row(prov, at, tokens, complete, inp, cr, out, legacy);
        }
    } else {
        let mut stmt = db.prepare(
            "SELECT provider, at, tokens, complete, input_tokens, cache_read_tokens, output_tokens, legacy
             FROM usage
             WHERE at>=?1 AND at<?2",
        )?;
        let rows = stmt.query_map(params![window_start_sec, window_end_sec], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        for row in rows {
            let (prov, at, tokens, complete, inp, cr, out, legacy) = row?;
            process_row(prov, at, tokens, complete, inp, cr, out, legacy);
        }
    }

    let first_start = specs.first().map(|s| s.start_date).unwrap_or(today);
    let last_end = specs.last().map(|s| s.end_date).unwrap_or(today);
    let is_drilldown = range_start.is_some() && range_end.is_some();

    let buckets = specs
        .into_iter()
        .zip(bucket_totals.into_iter())
        .zip(bucket_providers.into_iter())
        .map(|((spec, acc), prov_map)| HistoryBucket {
            key: spec.key,
            label: spec.label,
            start_date: spec.start_date.format("%Y-%m-%d").to_string(),
            end_date: spec.end_date.format("%Y-%m-%d").to_string(),
            is_current: spec.is_current,
            verified_tokens: acc.verified_tokens(),
            verified_steps: acc.verified_steps,
            incomplete_steps: acc.incomplete_steps,
            incomplete_known: acc.incomplete_known(),
            legacy_steps: acc.legacy_steps,
            legacy_tokens: acc.legacy_tokens(),
            providers: prov_map
                .into_iter()
                .map(|(id, pacc)| pacc.to_breakdown(&id))
                .collect(),
        })
        .collect();

    let summary = HistorySummary {
        verified_tokens: summary_total.verified_tokens(),
        verified_steps: summary_total.verified_steps,
        incomplete_steps: summary_total.incomplete_steps,
        incomplete_known: summary_total.incomplete_known(),
        legacy_steps: summary_total.legacy_steps,
        legacy_tokens: summary_total.legacy_tokens(),
        providers: summary_providers
            .into_iter()
            .map(|(id, pacc)| pacc.to_breakdown(&id))
            .collect(),
    };

    Ok(HistoryPage {
        granularity,
        offset,
        provider: filter.map(String::from),
        range_start: first_start.format("%Y-%m-%d").to_string(),
        range_end: last_end.format("%Y-%m-%d").to_string(),
        earliest_date: earliest_date.map(|d| d.format("%Y-%m-%d").to_string()),
        has_older: !is_drilldown && earliest_date.is_some_and(|e| e < first_start),
        has_newer: !is_drilldown && offset > 0,
        available_providers,
        summary,
        buckets,
    })
}
