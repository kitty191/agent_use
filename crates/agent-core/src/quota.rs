use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
    time::timeout,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub pool: String,
    pub minutes: Option<u64>,
    pub used: f64,
    pub reset: Option<i64>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub remaining_percent: Option<f64>,
    #[serde(default)]
    pub remaining_amount: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditBalance {
    pub amount: f64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub account: String,
    pub plan: Option<String>,
    pub checked_at: i64,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub ai_credits: Option<CreditBalance>,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaRefreshResult {
    pub quota: Option<Quota>,
    pub fresh: bool,
    pub message: Option<String>,
}

/// Pure-Rust FIPS 180-4 SHA-256 for deterministic, non-reversible account IDs without extra crate deps.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (dst, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(add);
        }
    }
    let mut out = String::with_capacity(64);
    for word in h {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{word:08x}");
    }
    out
}

pub fn stable_account_id(provider: &str, raw_identity: &str) -> Option<String> {
    let norm = raw_identity.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return None;
    }
    Some(format!("sha256:{}", sha256_hex(format!("{provider}:{norm}").as_bytes())))
}

pub fn mask_email(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((local, domain)) = trimmed.split_once('@') {
        if !local.is_empty() && !domain.is_empty() {
            return format!("{}***@{}", local.chars().next().unwrap_or('*'), domain);
        }
    }
    if trimmed.is_empty() {
        "未知账号".to_string()
    } else {
        trimmed.to_string()
    }
}

struct Rpc {
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    seq: u32,
}
impl Rpc {
    async fn send(&mut self, msg: Value) -> Result<()> {
        self.input
            .write_all(format!("{}\n", msg).as_bytes())
            .await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.seq += 1;
        self.send(json!({"id":self.seq,"method":method,"params":params}))
            .await?;
        while let Some(line) = self.output.next_line().await? {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if msg["id"] != self.seq {
                continue;
            }
            if let Some(err) = msg.get("error").filter(|v| !v.is_null()) {
                bail!("{}", classify_rpc_error(method, err));
            }
            return msg.get("result").cloned().context("Codex 返回了未知格式");
        }
        bail!("Codex CLI 已提前退出或断开连接，请检查 CLI 与登录状态")
    }
}

fn classify_rpc_error(method: &str, err: &Value) -> String {
    let raw = err.to_string().to_lowercase();
    let code = err.get("code").and_then(|v| v.as_i64());
    if raw.contains("429") || raw.contains("rate limit") || raw.contains("too many requests") {
        return "额度查询受到限流，请稍后重试".into();
    }
    if code == Some(-32601)
        || raw.contains("method not found")
        || raw.contains("not supported")
        || raw.contains("unsupported")
        || raw.contains("unknown method")
        || raw.contains("unimplemented")
    {
        return format!("当前 Codex CLI 版本不支持 {method} 查询，请升级 Codex CLI 后重试");
    }
    if raw.contains("401")
        || raw.contains("403")
        || raw.contains("unauth")
        || raw.contains("login")
        || raw.contains("not signed in")
        || raw.contains("credential")
        || raw.contains("token expired")
        || raw.contains("invalid_token")
    {
        return "Codex 登录状态已失效，请先在 Codex 客户端重新登录".into();
    }
    if raw.contains("dns")
        || raw.contains("network")
        || raw.contains("connect")
        || raw.contains("timeout")
        || raw.contains("timed out")
        || raw.contains("tls")
        || raw.contains("ssl")
        || raw.contains("socket")
        || raw.contains("offline")
        || raw.contains("unreachable")
        || raw.contains("econnrefused")
        || raw.contains("enotfound")
        || raw.contains("ehostunreach")
        || raw.contains("reset by peer")
        || raw.contains("broken pipe")
        || raw.contains("transport")
    {
        return "Codex 网络查询失败，请检查网络连接后重试".into();
    }
    format!("Codex CLI 执行 {method} 返回未知错误，请检查本机 Codex 状态")
}

pub async fn query(binary: &Path) -> Result<Quota> {
    query_with_timeout(binary, Duration::from_secs(20)).await
}

pub async fn query_with_timeout(binary: &Path, limit: Duration) -> Result<Quota> {
    ensure!(
        binary.is_file(),
        "未找到可执行的 Codex CLI（已记录路径不存在），请点击「重新扫描」"
    );
    let mut cmd = Command::new(binary);
    // Finder-launched apps have a minimal PATH; npm-installed launchers may use `/usr/bin/env node`.
    let mut dirs = Vec::new();
    if let Some(parent) = binary.parent() {
        dirs.push(parent.to_owned());
    }
    if let Ok(resolved) = binary.canonicalize() {
        if let Some(parent) = resolved.parent() {
            if !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_owned());
            }
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    dirs.extend(std::env::split_paths(&path));
    for fallback in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let fb = std::path::PathBuf::from(fallback);
        if !dirs.iter().any(|d| d == &fb) {
            dirs.push(fb);
        }
    }
    cmd.env("PATH", std::env::join_paths(dirs)?);
    let mut child = cmd
        .args([
            "-c",
            "analytics.enabled=false",
            "app-server",
            "--listen",
            "stdio://",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("无法启动 Codex CLI，请检查安装后点击「重新扫描」")?;
    let mut rpc = Rpc {
        input: child.stdin.take().context("无法连接 Codex")?,
        output: BufReader::new(child.stdout.take().context("无法读取 Codex")?).lines(),
        seq: 0,
    };
    let result = timeout(limit, async {
        rpc.call(
            "initialize",
            json!({"clientInfo":{"name":"agent_use","version":"0.1.0"},"capabilities":{}}),
        )
        .await?;
        rpc.send(json!({"method":"initialized","params":{}}))
            .await?;
        let account = rpc
            .call("account/read", json!({"refreshToken":false}))
            .await?;
        ensure!(
            !account["account"].is_null(),
            "Codex 尚未登录，请先在 Codex 客户端完成登录"
        );
        let limits = rpc.call("account/rateLimits/read", json!({})).await?;
        let confirm = rpc
            .call("account/read", json!({"refreshToken":false}))
            .await?;
        ensure!(account == confirm, "查询期间账号已切换，请刷新");
        normalize(&account["account"], &limits)
    })
    .await;
    drop(rpc);
    let _ = child.kill().await;
    result.map_err(|_| anyhow!("Codex 查询超时，请稍后重试"))?
}

fn normalize(account: &Value, limits: &Value) -> Result<Quota> {
    let raw_email = account["email"].as_str();
    let raw_identity = raw_email
        .or_else(|| account["id"].as_str())
        .or_else(|| account["accountId"].as_str())
        .or_else(|| account["sub"].as_str());
    let account_id = raw_identity.and_then(|id| stable_account_id("codex", id));
    let label = raw_email
        .map(mask_email)
        .unwrap_or_else(|| account["type"].as_str().unwrap_or("未知账号").to_string());
    let mut buckets = Vec::new();
    if let Some(map) = limits["rateLimitsByLimitId"].as_object() {
        buckets.extend(map.iter().map(|(k, v)| (k.as_str(), v)))
    }
    if buckets.is_empty() && limits["rateLimits"].is_object() {
        buckets.push(("codex", &limits["rateLimits"]))
    }
    let mut windows = Vec::new();
    for (pool, bucket) in buckets {
        for slot in ["primary", "secondary"] {
            let w = &bucket[slot];
            if let Some(used) = w["usedPercent"].as_f64() {
                let remaining = (100.0 - used).clamp(0.0, 100.0);
                windows.push(Window {
                    pool: pool.into(),
                    minutes: w["windowDurationMins"].as_u64(),
                    used,
                    reset: w["resetsAt"].as_i64(),
                    label: None,
                    models: Vec::new(),
                    remaining_percent: Some(remaining),
                    remaining_amount: None,
                    unit: None,
                    disabled: false,
                });
            }
        }
    }
    Ok(Quota {
        provider: "codex".into(),
        account_id,
        account: label,
        plan: account["planType"].as_str().map(String::from),
        checked_at: chrono::Utc::now().timestamp(),
        source: "Codex CLI app-server".into(),
        source_kind: "live_query".into(),
        ai_credits: None,
        windows,
    })
}

pub fn discover_antigravity_statusline_path(data_dir: &Path, home: &Path) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("AGENT_USE_ANTIGRAVITY_STATUSLINE_PATH") {
        if !explicit.is_empty() {
            let p = PathBuf::from(explicit);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    [
        data_dir.join("antigravity_statusline.json"),
        home.join(".gemini/antigravity-cli/statusline.json"),
        home.join(".gemini/antigravity-cli/statusline-snapshot.json"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn parse_timestamp_sec(val: Option<&Value>) -> Option<i64> {
    let v = val?;
    if let Some(sec) = v.as_i64() {
        return Some(sec);
    }
    if let Some(sec_f) = v.as_f64() {
        if sec_f.is_finite() {
            return Some(sec_f as i64);
        }
    }
    if let Some(raw) = v.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Some(dt.timestamp());
        }
    }
    None
}

fn push_unique_model(list: &mut Vec<String>, name: &str) {
    let trimmed = name.trim();
    if !trimmed.is_empty() && !list.iter().any(|existing| existing == trimmed) {
        list.push(trimmed.to_string());
    }
}

fn parse_ai_credits(val: &Value) -> Option<CreditBalance> {
    let raw = val.get("ai_credits").or_else(|| val.get("credits"))?;
    if let Some(amount) = raw.as_f64().filter(|n| n.is_finite() && *n >= 0.0) {
        return Some(CreditBalance {
            amount,
            unit: Some("Credits".into()),
        });
    }
    let obj = raw.as_object()?;
    let amount = obj
        .get("remaining_amount")
        .or_else(|| obj.get("remaining"))
        .or_else(|| obj.get("amount"))
        .or_else(|| obj.get("balance"))
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)?;
    let unit = obj
        .get("unit")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| Some("Credits".into()));
    Some(CreditBalance { amount, unit })
}

pub fn parse_antigravity_statusline_value(val: &Value, fallback_captured_at: i64) -> Result<Quota> {
    let obj = val
        .as_object()
        .context("Antigravity statusLine 快照格式无效")?;
    if let Some(product) = obj.get("product").and_then(|v| v.as_str()) {
        ensure!(
            product.eq_ignore_ascii_case("antigravity"),
            "statusLine 快照的 product 字段不属于 antigravity"
        );
    }

    let checked_at = parse_timestamp_sec(obj.get("checked_at"))
        .or_else(|| parse_timestamp_sec(obj.get("captured_at")))
        .or_else(|| parse_timestamp_sec(obj.get("timestamp")))
        .or_else(|| parse_timestamp_sec(obj.get("updated_at")))
        .unwrap_or(fallback_captured_at);

    let raw_email = obj.get("email").and_then(|v| v.as_str());
    let raw_identity = raw_email
        .or_else(|| obj.get("account_id").and_then(|v| v.as_str()))
        .or_else(|| obj.get("user_id").and_then(|v| v.as_str()));
    let account_id = raw_identity.and_then(|id| stable_account_id("antigravity", id));
    let account = raw_email
        .map(mask_email)
        .unwrap_or_else(|| "未提供账号".into());
    let plan = obj
        .get("plan_tier")
        .or_else(|| obj.get("plan"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut grouped: BTreeMap<String, Window> = BTreeMap::new();
    if let Some(quota_map) = obj.get("quota").and_then(|v| v.as_object()) {
        for (raw_key, entry) in quota_map {
            let Some(eobj) = entry.as_object() else {
                continue;
            };
            let pool_id = eobj
                .get("pool_id")
                .or_else(|| eobj.get("quota_pool"))
                .or_else(|| eobj.get("group"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(raw_key.as_str())
                .to_string();

            let label = eobj
                .get("label")
                .or_else(|| eobj.get("display_name"))
                .or_else(|| eobj.get("title"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            let remaining_percent = eobj
                .get("remaining_fraction")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
                .map(|f| (f * 100.0).clamp(0.0, 100.0));

            let remaining_amount = eobj
                .get("remaining_amount")
                .and_then(|v| v.as_f64())
                .filter(|a| a.is_finite() && *a >= 0.0);

            let unit = eobj
                .get("unit")
                .or_else(|| eobj.get("amount_unit"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            let disabled = eobj
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let reset = parse_timestamp_sec(eobj.get("reset_time")).or_else(|| {
                eobj.get("reset_in_seconds")
                    .and_then(|v| v.as_i64())
                    .filter(|&secs| secs >= 0)
                    .map(|secs| checked_at.saturating_add(secs))
            });

            if remaining_percent.is_none()
                && remaining_amount.is_none()
                && reset.is_none()
                && !disabled
            {
                continue;
            }

            let minutes = eobj.get("window_minutes").and_then(|v| v.as_u64());
            let used = remaining_percent.map(|p| 100.0 - p).unwrap_or(0.0);

            let mut entry_models = Vec::new();
            if let Some(arr) = eobj.get("models").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(s) = m.as_str() {
                        push_unique_model(&mut entry_models, s);
                    }
                }
            }
            if let Some(m) = eobj.get("model").and_then(|v| v.as_str()) {
                push_unique_model(&mut entry_models, m);
            }
            if entry_models.is_empty() && pool_id != *raw_key {
                push_unique_model(&mut entry_models, raw_key);
            }

            grouped
                .entry(pool_id.clone())
                .and_modify(|existing| {
                    if existing.label.is_none() {
                        existing.label = label.clone();
                    }
                    if existing.minutes.is_none() {
                        existing.minutes = minutes;
                    }
                    if existing.remaining_percent.is_none() {
                        existing.remaining_percent = remaining_percent;
                        if remaining_percent.is_some() {
                            existing.used = used;
                        }
                    }
                    if existing.remaining_amount.is_none() {
                        existing.remaining_amount = remaining_amount;
                    }
                    if existing.unit.is_none() {
                        existing.unit = unit.clone();
                    }
                    if existing.reset.is_none() {
                        existing.reset = reset;
                    }
                    existing.disabled |= disabled;
                    for m in &entry_models {
                        push_unique_model(&mut existing.models, m);
                    }
                })
                .or_insert_with(|| Window {
                    pool: pool_id,
                    minutes,
                    used,
                    reset,
                    label,
                    models: entry_models,
                    remaining_percent,
                    remaining_amount,
                    unit,
                    disabled,
                });
        }
    }

    Ok(Quota {
        provider: "antigravity".into(),
        account_id,
        account,
        plan,
        checked_at,
        source: "Antigravity CLI statusLine".into(),
        source_kind: "local_snapshot".into(),
        ai_credits: parse_ai_credits(val),
        windows: grouped.into_values().collect(),
    })
}

pub fn read_antigravity_statusline(path: &Path) -> Result<Quota> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("无法读取 Antigravity statusLine 快照 {}", path.display()))?;
    let fallback_ts = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let val: Value = serde_json::from_str(&raw)
        .with_context(|| format!("解析 Antigravity statusLine JSON 失败: {}", path.display()))?;
    parse_antigravity_statusline_value(&val, fallback_ts)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeInfo {
    pub script_path: String,
    pub snapshot_path: String,
    pub settings_snippet: String,
    pub wrap_command: String,
}

fn posix_shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

pub fn build_antigravity_bridge_info(script_path: &Path, snapshot_path: &Path) -> Result<BridgeInfo> {
    let script_str = script_path.to_string_lossy().into_owned();
    let snapshot_str = snapshot_path.to_string_lossy().into_owned();
    let quoted_script = posix_shell_quote(&script_str);
    let settings_obj = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": quoted_script,
            "stack_with_default": true
        }
    });
    let settings_snippet = serde_json::to_string_pretty(&settings_obj)?;
    let wrap_command = format!(
        "{} ~/.gemini/antigravity-cli/statusline.sh",
        posix_shell_quote(&script_str)
    );
    Ok(BridgeInfo {
        script_path: script_str,
        snapshot_path: snapshot_str,
        settings_snippet,
        wrap_command,
    })
}
