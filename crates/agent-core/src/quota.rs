use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
    time::timeout,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub pool: String,
    pub minutes: Option<u64>,
    pub used: f64,
    pub reset: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub account: String,
    pub plan: Option<String>,
    pub checked_at: i64,
    pub windows: Vec<Window>,
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
    let label = account["email"]
        .as_str()
        .and_then(|s| s.split_once('@'))
        .map(|(local, domain)| format!("{}***@{}", local.chars().next().unwrap_or('*'), domain))
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
                windows.push(Window {
                    pool: pool.into(),
                    minutes: w["windowDurationMins"].as_u64(),
                    used,
                    reset: w["resetsAt"].as_i64(),
                });
            }
        }
    }
    Ok(Quota {
        account: label,
        plan: account["planType"].as_str().map(String::from),
        checked_at: chrono::Utc::now().timestamp(),
        windows,
    })
}
