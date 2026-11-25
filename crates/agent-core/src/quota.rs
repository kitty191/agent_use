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
            let msg: Value = serde_json::from_str(&line)?;
            if msg["id"] != self.seq {
                continue;
            }
            if let Some(err) = msg.get("error") {
                if err.to_string().contains("429") {
                    bail!("额度查询受到限流，请稍后重试")
                }
                bail!("{method} 查询失败，请检查 Codex 登录和网络")
            }
            return msg.get("result").cloned().context("Codex 返回了未知格式");
        }
        bail!("Codex 已断开连接")
    }
}
pub async fn query(binary: &Path) -> Result<Quota> {
    let mut cmd = Command::new(binary);
    // npm-installed CLI launchers may use /usr/bin/env node.
    if let Some(parent) = binary.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(parent.to_owned()).chain(std::env::split_paths(&path));
        cmd.env("PATH", std::env::join_paths(dirs)?);
    }
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
        .context("无法启动 Codex CLI，请重新扫描")?;
    let mut rpc = Rpc {
        input: child.stdin.take().context("无法连接 Codex")?,
        output: BufReader::new(child.stdout.take().context("无法读取 Codex")?).lines(),
        seq: 0,
    };
    let result = timeout(Duration::from_secs(20), async {
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
        ensure!(!account["account"].is_null(), "请先在 Codex 客户端登录");
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
    result.map_err(|_| anyhow!("Codex 查询超时，请稍后再试"))?
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
