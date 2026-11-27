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
    Year,
}

impl Granularity {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            _ => bail!("不支持的统计粒度: {raw}"),
        }
    }

    pub fn page_size(self) -> usize {
        let _ = self;
        1
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
    pub bucket_granularity: Granularity,
    pub is_current: bool,
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
    pub drill_key: Option<String>,
    pub drill: Option<Box<HistoryPage>>,
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

fn build_month_specs_range<Tz: TimeZone>(
    start: NaiveDate,
    end_inclusive: NaiveDate,
    today: NaiveDate,
    tz: &Tz,
) -> Result<Vec<BucketSpec>> {
    let mut specs = Vec::new();
    let mut y = start.year();
    let mut m = start.month();
    let end_ym = (end_inclusive.year(), end_inclusive.month());
    while (y, m) <= end_ym {
        if specs.len() >= 120 {
            bail!("单次月份明细范围不能超过 120 个月");
        }
        let (ms, me, _) = month_bounds(y, m)?;
        let effective_start = ms.max(start);
        let effective_end = me.min(end_inclusive);
        let exclusive_end = effective_end.succ_opt().context("日期溢出")?;
        let is_current = today >= effective_start && today <= effective_end;
        specs.push(BucketSpec {
            key: format!("{y:04}-{m:02}"),
            label: format!("{y}年{m}月"),
            start_date: effective_start,
            end_date: effective_end,
            start_sec: local_day_start_sec(effective_start, tz)?,
            end_sec: local_day_start_sec(exclusive_end, tz)?,
            is_current,
        });
        let (ny, nm) = shift_months(y, m, 1)?;
        y = ny;
        m = nm;
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
        return match granularity {
            Granularity::Month => build_month_specs_range(rs, re, today, tz),
            _ => {
                let span = (re - rs).num_days();
                if span > 366 {
                    bail!("单次日期明细范围不能超过 366 天");
                }
                build_day_specs(rs, re, today, tz)
            }
        };
    }

    match granularity {
        Granularity::Day => {
            let day = today - Duration::days(i64::from(offset));
            build_day_specs(day, day, today, tz)
        }
        Granularity::Week => {
            let mon = week_monday(today) - Duration::days(i64::from(offset) * 7);
            let sun = mon + Duration::days(6);
            let end = if offset == 0 && sun > today { today } else { sun };
            build_day_specs(mon, end, today, tz)
        }
        Granularity::Month => {
            let (y, m) = shift_months(today.year(), today.month(), -(offset as i32))?;
            let (start, end, _) = month_bounds(y, m)?;
            let clamped_end = if offset == 0 && end > today { today } else { end };
            build_day_specs(start, clamped_end, today, tz)
        }
        Granularity::Year => {
            let y = today.year() - (offset as i32);
            let start = NaiveDate::from_ymd_opt(y, 1, 1).context("无效年份")?;
            let end = NaiveDate::from_ymd_opt(y, 12, 31).context("无效年末")?;
            let clamped_end = if offset == 0 && end > today { today } else { end };
            build_month_specs_range(start, clamped_end, today, tz)
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
    query_with_drill(
        store,
        granularity,
        offset,
        provider,
        range_start,
        range_end,
        None,
    )
}

pub fn query_with_drill(
    store: &Store,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<&str>,
    range_end: Option<&str>,
    drill_key: Option<&str>,
) -> Result<HistoryPage> {
    let today = Local::now().date_naive();
    let rs = range_start
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").context("无效起始日期"))
        .transpose()?;
    let re = range_end
        .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").context("无效结束日期"))
        .transpose()?;
    query_with_tz_and_drill(
        store,
        granularity,
        offset,
        provider,
        rs,
        re,
        today,
        &Local,
        drill_key,
    )
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
    query_with_tz_and_drill(
        store,
        granularity,
        offset,
        provider,
        range_start,
        range_end,
        today,
        tz,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn query_with_tz_and_drill<Tz: TimeZone>(
    store: &Store,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<NaiveDate>,
    range_end: Option<NaiveDate>,
    today: NaiveDate,
    tz: &Tz,
    drill_key: Option<&str>,
) -> Result<HistoryPage> {
    query_with_tz_and_drill_with_hook(
        store,
        granularity,
        offset,
        provider,
        range_start,
        range_end,
        today,
        tz,
        drill_key,
        || {},
    )
}

#[allow(clippy::too_many_arguments)]
fn query_with_tz_and_drill_with_hook<Tz: TimeZone>(
    store: &Store,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<NaiveDate>,
    range_end: Option<NaiveDate>,
    today: NaiveDate,
    tz: &Tz,
    drill_key: Option<&str>,
    mut between_queries_hook: impl FnMut(),
) -> Result<HistoryPage> {
    let mut db = store.connect()?;
    db.pragma_update(None, "query_only", true)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
    let mut page = query_page_on_tx(
        &tx,
        granularity,
        offset,
        provider,
        range_start,
        range_end,
        today,
        tz,
    )?;

    between_queries_hook();

    if let Some(key) = drill_key.filter(|k| !k.is_empty()) {
        if granularity != Granularity::Day && range_start.is_none() && range_end.is_none() {
            if let Some(bucket) = page.buckets.iter().find(|b| b.key == key) {
                let child_gran = match granularity {
                    Granularity::Year => Granularity::Month,
                    _ => Granularity::Day,
                };
                let bs = NaiveDate::parse_from_str(&bucket.start_date, "%Y-%m-%d")
                    .context("无效明细起始日期")?;
                let be = NaiveDate::parse_from_str(&bucket.end_date, "%Y-%m-%d")
                    .context("无效明细结束日期")?;
                let child = query_page_on_tx(
                    &tx,
                    child_gran,
                    0,
                    provider,
                    Some(bs),
                    Some(be),
                    today,
                    tz,
                )?;
                page.drill_key = Some(key.to_string());
                page.drill = Some(Box::new(child));
            }
        }
    }

    Ok(page)
}

#[allow(clippy::too_many_arguments)]
fn query_page_on_tx<Tz: TimeZone>(
    db: &rusqlite::Connection,
    granularity: Granularity,
    offset: u32,
    provider: Option<&str>,
    range_start: Option<NaiveDate>,
    range_end: Option<NaiveDate>,
    today: NaiveDate,
    tz: &Tz,
) -> Result<HistoryPage> {
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

    let bucket_granularity = match granularity {
        Granularity::Year => Granularity::Month,
        _ => Granularity::Day,
    };

    Ok(HistoryPage {
        granularity,
        bucket_granularity,
        is_current: !is_drilldown && offset == 0,
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
        drill_key: None,
        drill: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Agent;
    use chrono::FixedOffset;
    use rusqlite::params;
    use tempfile::tempdir;

    #[test]
    fn snapshot_transaction_keeps_parent_and_drill_consistent_under_concurrent_write() -> Result<()> {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("monitor.sqlite"))?;
        store.settings(false, || {
            vec![Agent {
                id: "codex".into(),
                name: "OpenAI Codex".into(),
                status: "installed".into(),
                binary: None,
            }]
        })?;
        store.select(vec!["codex".into()])?;

        let ts1 = chrono::DateTime::parse_from_rfc3339("2026-11-25T10:00:00+08:00")?.timestamp();
        store.connect()?.execute(
            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
             VALUES('codex','s1','r1',?1,300,?2,1,0)",
            params![ts1, ts1 * 1_000_000_000],
        )?;

        let today = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let mut injected = false;
        let page1 = query_with_tz_and_drill_with_hook(
            &store,
            Granularity::Year,
            0,
            None,
            None,
            None,
            today,
            &tz,
            Some("2026-11"),
            || {
                if !injected {
                    injected = true;
                    let ts2 = chrono::DateTime::parse_from_rfc3339("2026-11-26T10:00:00+08:00")
                        .unwrap()
                        .timestamp();
                    store
                        .connect()
                        .unwrap()
                        .execute(
                            "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
                             VALUES('codex','s1','r2',?1,250,?2,1,0)",
                            params![ts2, ts2 * 1_000_000_000],
                        )
                        .unwrap();
                }
            },
        )?;

        let parent_nov = page1.buckets.iter().find(|b| b.key == "2026-11").unwrap();
        let drill1 = page1.drill.as_ref().unwrap();
        // Same snapshot: summary (300), monthly bucket (300), and daily drill (300) agree under concurrent write.
        assert_eq!(page1.summary.verified_tokens, Some(300));
        assert_eq!(parent_nov.verified_tokens, Some(300));
        assert_eq!(drill1.summary.verified_tokens, Some(300));

        // Subsequent query sees 550 everywhere atomically.
        let page2 = query_with_tz_and_drill(
            &store,
            Granularity::Year,
            0,
            None,
            None,
            None,
            today,
            &tz,
            Some("2026-11"),
        )?;
        let parent_nov2 = page2.buckets.iter().find(|b| b.key == "2026-11").unwrap();
        let drill2 = page2.drill.as_ref().unwrap();
        assert_eq!(page2.summary.verified_tokens, Some(550));
        assert_eq!(parent_nov2.verified_tokens, Some(550));
        assert_eq!(drill2.summary.verified_tokens, Some(550));
        Ok(())
    }
}
