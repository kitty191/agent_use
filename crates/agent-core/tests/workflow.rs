use agent_core::{
    discovery::Agent,
    history::{self, Granularity},
    quota,
    store::Store,
    usage::{Monitor, SourceStatus},
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn agent(id: &str, status: &str) -> Agent {
    Agent {
        id: id.into(),
        name: id.into(),
        status: status.into(),
        binary: None,
    }
}

#[test]
fn empty_scan_restart_rescan_and_opt_in() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = dir.path().join("state.db");
    let store = Store::open(&path)?;
    assert!(store.settings(false, Vec::new)?.agents.is_empty());
    let reopened = Store::open(&path)?;
    reopened.settings(false, || panic!("second launch scanned again"))?;
    reopened.settings(true, || vec![agent("codex", "installed")])?;
    assert!(reopened.enabled_agent("codex").is_err());
    reopened.select(vec!["codex".into()])?;
    let s = reopened.settings(true, || {
        vec![agent("codex", "missing"), agent("claude", "data")]
    })?;
    assert_eq!(s.enabled, ["codex"]);
    assert!(s.confirmed);
    assert!(reopened.enabled_agent("claude").is_err());
    reopened.select(vec![])?;
    assert!(reopened.cached_quota().is_err());
    Ok(())
}

#[test]
fn watcher_handles_new_file_partial_line_replay_and_restart() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("state.db"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;
    let root = dir.path().join("sessions");
    fs::create_dir_all(&root)?;
    let mut mon = Monitor::default();
    assert_eq!(mon.update(&store, "codex", &root)?.today, None);
    let path = root.join("new.jsonl");
    let event=json!({"type":"token_usage_record","timestamp":chrono::Utc::now().to_rfc3339(),"payload":{"session_id":"s1","response_id":"r1","usage":{"total_tokens":27,"cached_input_tokens":5}}}).to_string();
    fs::write(&path, &event)?;
    assert_eq!(mon.update(&store, "codex", &root)?.latest, None);
    let mut f = fs::OpenOptions::new().append(true).open(&path)?;
    writeln!(f)?;
    writeln!(f, "{event}")?;
    writeln!(
        f,
        "{}",
        json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":27}}}})
    )?;
    let start = Instant::now();
    while mon.update(&store, "codex", &root)?.today != Some(27) {
        anyhow::ensure!(start.elapsed() < Duration::from_secs(5), "new usage missed");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(mon);
    assert_eq!(
        Monitor::default().update(&store, "codex", &root)?.today,
        Some(27)
    );
    Ok(())
}

#[test]
fn claude_updates_message_and_includes_cache() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("usage.db"))?;
    store.settings(false, || vec![agent("claude", "data")])?;
    store.select(vec!["claude".into()])?;
    let at = chrono::Utc::now().to_rfc3339();
    let rows=[1,8].map(|out|json!({"type":"assistant","timestamp":at,"sessionId":"s","message":{"id":"m","usage":{"input_tokens":2,"output_tokens":out,"cache_read_input_tokens":5,"cache_creation_input_tokens":1}}}).to_string());
    fs::write(dir.path().join("claude.jsonl"), rows.join("\n") + "\n")?;
    assert_eq!(
        Monitor::default()
            .update(&store, "claude", dir.path())?
            .today,
        Some(16)
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn quota_reads_only_status_methods_and_returns_one_real_window() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;
    let bin = dir.path().join("codex");
    fs::write(
        &bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized':continue
    if msg['method']=='initialize':result={}
    elif msg['method']=='account/read':
        assert msg['params']=={'refreshToken':False}
        result={'account':{'type':'chatgpt','email':'test@example.org','planType':'plus'}}
    elif msg['method']=='account/rateLimits/read':
        result={'rateLimitsByLimitId':{'codex':{'primary':{'windowDurationMins':10080,'usedPercent':16,'resetsAt':1795935437},'secondary':None}}}
    else: sys.exit(1)
    print(json.dumps({'id':msg['id'],'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
    let q = quota::query(&bin).await?;
    assert_eq!(q.account, "t***@example.org");
    assert_eq!(q.windows.len(), 1);
    assert_eq!(q.windows[0].minutes, Some(10080));
    assert_eq!(100.0 - q.windows[0].used, 84.0);
    Ok(())
}

#[test]
fn codex_live_rollout_in_new_date_directory_counts_each_response_once() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir(&sessions)?;
    let mut mon = Monitor::default();
    assert_eq!(mon.update(&store, "codex", &sessions)?.today, None);
    let date_dir = sessions.join(chrono::Local::now().format("%Y/%m/%d").to_string());
    fs::create_dir_all(&date_dir)?;
    let rollout = date_dir.join("rollout-example.jsonl");
    let mut writer = fs::File::create(&rollout)?;
    writeln!(
        writer,
        "{}",
        json!({"type":"session_meta","payload":{"id":"sample-session"}})
    )?;
    let mut total = 0;
    for (n, tokens) in [19895, 19908, 19926].into_iter().enumerate() {
        let timestamp = chrono::Utc::now().to_rfc3339();
        let counters = json!({"input_tokens":tokens-5,"cached_input_tokens":0,"output_tokens":5,
                              "reasoning_output_tokens":0,"total_tokens":tokens});
        total += tokens;
        let response = format!("sample-response-{n}");
        let record = json!({"type":"token_usage_record","timestamp":timestamp,"payload":{
            "session_id":"sample-session","thread_id":"sample-thread","response_id":response,
            "turn_id":format!("turn-{n}"),"root_turn_id":format!("turn-{n}"),
            "usage":counters,"turn_token_usage":counters,"thread_token_usage":{"total_tokens":total}
        }});
        let summary = json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"token_count",
            "info":{"last_token_usage":counters,"total_token_usage":{"total_tokens":total},
                    "model_context_window":128000},"rate_limits":null}});
        // While a JSONL line is incomplete, the cursor must stop before the line.
        write!(writer, "{record}")?;
        writer.flush()?;
        assert_eq!(
            mon.update(&store, "codex", &sessions)?.today,
            (n > 0).then_some(total - tokens)
        );
        writeln!(writer)?;
        writeln!(writer, "{summary}")?;
        writeln!(writer, "{record}")?;
        writer.flush()?;
        let start = Instant::now();
        loop {
            let state = mon.update(&store, "codex", &sessions)?;
            anyhow::ensure!(
                start.elapsed() < Duration::from_secs(2),
                "complete line missed the two-second backend budget"
            );
            if state.today == Some(total) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let db = store.connect()?;
        let (rows,stored):(i64,i64)=db.query_row(
            "SELECT COUNT(*), SUM(tokens) FROM usage WHERE provider='codex' AND session='sample-session' AND response=?1",
            [&response],|r|Ok((r.get(0)?,r.get(1)?)))?;
        assert_eq!((rows, stored), (1, tokens));
        let cursor: i64 = db.query_row(
            "SELECT pos FROM cursors WHERE provider='codex' AND path=?1",
            [rollout.to_str().unwrap()],
            |r| r.get(0),
        )?;
        assert_eq!(cursor as u64, writer.metadata()?.len());
    }
    assert_eq!(total, 59729);
    assert_eq!(
        Monitor::default().update(&store, "codex", &sessions)?.today,
        Some(59729)
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn quota_tolerates_null_error_and_distinguishes_failure_stages() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir()?;

    // 1. CLI missing
    let missing = dir.path().join("missing-codex");
    let err = quota::query(&missing).await.unwrap_err().to_string();
    assert!(err.contains("未找到可执行的 Codex CLI"), "got: {err}");

    // 2. Tolerates non-JSON stdout lines and explicit `"error": null`
    let ok_bin = dir.path().join("codex-ok");
    fs::write(
        &ok_bin,
        r#"#!/usr/bin/env python3
import json,sys
print("startup diagnostic banner", flush=True)
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize': result={}
    elif msg['method']=='account/read':
        result={'account':{'type':'chatgpt','email':'alice@example.org','planType':'plus'}}
    elif msg['method']=='account/rateLimits/read':
        result={'rateLimitsByLimitId':{'codex':{'primary':{'windowDurationMins':10080,'usedPercent':20,'resetsAt':1795935437},'secondary':None}}}
    print(json.dumps({'id':msg['id'],'error':None,'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&ok_bin, fs::Permissions::from_mode(0o700))?;
    let q = quota::query(&ok_bin).await?;
    assert_eq!(q.account, "a***@example.org");
    assert_eq!(q.windows.len(), 1);

    // 3. Not logged in
    let unauth_bin = dir.path().join("codex-unauth");
    fs::write(
        &unauth_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    result={} if msg['method']=='initialize' else {'account':None}
    print(json.dumps({'id':msg['id'],'result':result}),flush=True)
"#,
    )?;
    fs::set_permissions(&unauth_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unauth_bin).await.unwrap_err().to_string();
    assert!(err.contains("尚未登录"), "got: {err}");

    // 4. Network error during rateLimits/read
    let net_bin = dir.path().join("codex-net");
    fs::write(
        &net_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32000,'message':'dns lookup failed'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&net_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&net_bin).await.unwrap_err().to_string();
    assert!(err.contains("网络查询失败"), "got: {err}");

    // 5. Rate limit (429)
    let rl_bin = dir.path().join("codex-429");
    fs::write(
        &rl_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':429,'message':'HTTP 429 Too Many Requests'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&rl_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&rl_bin).await.unwrap_err().to_string();
    assert!(err.contains("限流"), "got: {err}");

    // 6. Method not supported (e.g., older Codex CLI) must not blame network
    let unsupported_bin = dir.path().join("codex-unsupported");
    fs::write(
        &unsupported_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32601,'message':'Method not found'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&unsupported_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unsupported_bin)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("不支持"), "got: {err}");
    assert!(!err.contains("网络"), "got: {err}");

    // 7. Unknown RPC error on rateLimits/read must not blame network either
    let unknown_bin = dir.path().join("codex-unknown");
    fs::write(
        &unknown_bin,
        r#"#!/usr/bin/env python3
import json,sys
for line in sys.stdin:
    msg=json.loads(line)
    if msg['method']=='initialized': continue
    if msg['method']=='initialize':
        print(json.dumps({'id':msg['id'],'result':{}}),flush=True)
    elif msg['method']=='account/read':
        print(json.dumps({'id':msg['id'],'result':{'account':{'type':'chatgpt','email':'a@b.com','planType':'plus'}}}),flush=True)
    elif msg['method']=='account/rateLimits/read':
        print(json.dumps({'id':msg['id'],'error':{'code':-32099,'message':'unexpected internal state'}}),flush=True)
"#,
    )?;
    fs::set_permissions(&unknown_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query(&unknown_bin).await.unwrap_err().to_string();
    assert!(err.contains("未知错误"), "got: {err}");
    assert!(!err.contains("网络"), "got: {err}");

    // 8. Overall timeout when CLI hangs on initialize must not blame network
    let hang_bin = dir.path().join("codex-hang");
    fs::write(
        &hang_bin,
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.readline()
time.sleep(30)
"#,
    )?;
    fs::set_permissions(&hang_bin, fs::Permissions::from_mode(0o700))?;
    let err = quota::query_with_timeout(&hang_bin, Duration::from_millis(250))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "Codex 查询超时，请稍后重试");
    assert!(!err.contains("网络"), "got: {err}");

    Ok(())
}

#[test]
fn antigravity_hidden_transcript_dedup_partial_line_restart_and_waiting_recovery()
-> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || {
        vec![
            agent("codex", "installed"),
            agent("antigravity", "installed"),
        ]
    })?;

    // Only Codex enabled: Antigravity must not be readable.
    store.select(vec!["codex".into()])?;
    let brain = dir.path().join("brain");
    assert!(Monitor::default().update(&store, "antigravity", &brain).is_err());

    // Enable Antigravity before brain directory exists -> Waiting (not Active, not error).
    store.select(vec!["codex".into(), "antigravity".into()])?;
    let mut mon = Monitor::default();
    let waiting = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(waiting.status, SourceStatus::Waiting);
    assert_eq!(waiting.today, None);
    assert_eq!(waiting.latest, None);

    // Brain directory appears -> auto-recovers to Active; empty directory still has None (not fake 0).
    fs::create_dir_all(&brain)?;
    let empty_active = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(empty_active.status, SourceStatus::Active);
    assert_eq!(empty_active.today, None);

    // Create a new conversation under hidden `.system_generated/logs/transcript.jsonl`,
    // plus duplicate `transcript_full.jsonl` and `chunks/transcript/00000000.jsonl` which must be ignored.
    let conv_logs = brain.join("conv-alpha/.system_generated/logs");
    let chunks_dir = conv_logs.join("chunks/transcript");
    fs::create_dir_all(&chunks_dir)?;

    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32| {
        today
            .and_hms_opt(0, 5, sec)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };
    let step1_running = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "RUNNING",
        "created_at": ts(1),
        "input_tokens": 1000,
        "cache_read_tokens": 0,
        "output_tokens": 50
    });
    let step1_done = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2),
        "model": "gemini-4-neon-high",
        "input_tokens": 14688,
        "cache_read_tokens": 0,
        "output_tokens": 558
    });
    let step3_done = json!({
        "step_index": 3,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(3),
        "input_tokens": 3239,
        "cache_read_tokens": 12631,
        "output_tokens": 394
    });

    let transcript = conv_logs.join("transcript.jsonl");
    // Write duplicate copies into transcript_full.jsonl and chunks/ to verify they are NOT read.
    let dup_payload = format!("{step1_done}\n{step3_done}\n");
    fs::write(conv_logs.join("transcript_full.jsonl"), &dup_payload)?;
    fs::write(chunks_dir.join("00000000.jsonl"), &dup_payload)?;

    // Write RUNNING + DONE + duplicate DONE + incomplete trailing line (no newline yet).
    let mut f = fs::File::create(&transcript)?;
    writeln!(f, "{step1_running}")?;
    writeln!(f, "{step1_done}")?;
    writeln!(f, "{step1_done}")?;
    write!(f, "{step3_done}")?;
    f.flush()?;

    // Only step 1 DONE should be counted (14688 + 0 + 558 = 15246); incomplete step 3 waits.
    let u1 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u1.status, SourceStatus::Active);
    assert_eq!(u1.today, Some(15246));
    assert_eq!(u1.latest, Some(15246));

    // Finish line for step 3 (3239 + 12631 + 394 = 16264); total = 31510.
    writeln!(f)?;
    f.flush()?;
    let start = Instant::now();
    loop {
        let u2 = mon.update(&store, "antigravity", &brain)?;
        anyhow::ensure!(start.elapsed() < Duration::from_secs(2), "missed step 3");
        if u2.today == Some(31510) {
            assert_eq!(u2.latest, Some(16264));
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Simulate app closed: drop monitor, create a second conversation with a real-zero step + normal step.
    drop(mon);
    let conv2_logs = brain.join("conv-beta/.system_generated/logs");
    fs::create_dir_all(&conv2_logs)?;
    let step_zero = json!({
        "step_index": 0,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(4),
        "input_tokens": 0,
        "cache_read_tokens": 0,
        "output_tokens": 0
    });
    let step_next = json!({
        "step_index": 2,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(5),
        "input_tokens": 2062,
        "cache_read_tokens": 15247,
        "output_tokens": 341
    });
    fs::write(
        conv2_logs.join("transcript.jsonl"),
        format!("{step_zero}\n{step_next}\n"),
    )?;

    // Reopen twice: catches up once (31510 + 0 + 17650 = 49160), second reopen does not duplicate.
    for _ in 0..2 {
        let u3 = Monitor::default().update(&store, "antigravity", &brain)?;
        assert_eq!(u3.status, SourceStatus::Active);
        assert_eq!(u3.today, Some(49160));
        assert_eq!(u3.latest, Some(17650));
    }

    // Separate provider isolation: Codex has no rows in this store.
    let codex_dir = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_dir)?;
    let codex_u = Monitor::default().update(&store, "codex", &codex_dir)?;
    assert_eq!(codex_u.today, None);

    // If brain directory disappears later, returns Waiting while preserving historical totals.
    fs::remove_dir_all(&brain)?;
    let preserved = Monitor::default().update(&store, "antigravity", &brain)?;
    assert_eq!(preserved.status, SourceStatus::Waiting);
    assert_eq!(preserved.today, Some(49160));
    assert_eq!(preserved.latest, Some(17650));

    // If path is a regular file (unreadable as directory), returns an error.
    fs::write(&brain, "not-a-dir")?;
    assert!(Monitor::default().update(&store, "antigravity", &brain).is_err());

    Ok(())
}

#[test]
fn antigravity_cache_zero_vs_missing_and_complete_backfill() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-cache/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let transcript = logs.join("transcript.jsonl");

    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(10, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };

    // Case 1: explicit cache_read_tokens = 0 -> complete total = 320.
    let explicit_zero = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 100),
        "input_tokens": 300,
        "cache_read_tokens": 0,
        "output_tokens": 20
    });
    fs::write(&transcript, format!("{explicit_zero}\n"))?;
    let mut mon = Monitor::default();
    let u1 = mon.update(&store, "antigravity", &brain)?;
    assert_eq!(u1.today, Some(320));
    assert_eq!(u1.today_complete_steps, 1);
    assert_eq!(u1.today_incomplete_steps, 0);
    assert_eq!(u1.latest, Some(320));
    assert!(!u1.latest_incomplete);

    // Case 2: missing cache_read_tokens in a separate fresh store -> incomplete (not 320!).
    let dir2 = tempdir()?;
    let store2 = Store::open(&dir2.path().join("monitor.sqlite"))?;
    store2.settings(false, || vec![agent("antigravity", "installed")])?;
    store2.select(vec!["antigravity".into()])?;
    let brain2 = dir2.path().join("brain");
    let logs2 = brain2.join("conv-missing/.system_generated/logs");
    fs::create_dir_all(&logs2)?;
    let transcript2 = logs2.join("transcript.jsonl");

    let missing_cache = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 100),
        "input_tokens": 300,
        "output_tokens": 20
    });
    let mut f2 = fs::File::create(&transcript2)?;
    writeln!(f2, "{missing_cache}")?;
    f2.flush()?;

    let mut mon2 = Monitor::default();
    let u_missing = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_missing.today, None);
    assert_eq!(u_missing.today_complete_steps, 0);
    assert_eq!(u_missing.today_incomplete_steps, 1);
    assert_eq!(u_missing.latest, None);
    assert!(u_missing.latest_incomplete);
    let parts = u_missing.latest_parts.expect("expected known parts");
    assert_eq!((parts.input, parts.cache_read, parts.output), (Some(300), None, Some(20)));

    // Explicit null cache_read_tokens on same step keeps incomplete state.
    let null_cache = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 200),
        "input_tokens": 300,
        "cache_read_tokens": null,
        "output_tokens": 20
    });
    writeln!(f2, "{null_cache}")?;
    f2.flush()?;
    let u_null = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_null.today, None);
    assert_eq!(u_null.today_incomplete_steps, 1);
    assert!(u_null.latest_incomplete);

    // Same (conv, step) later reports cache_read_tokens = 100 -> upgrades to 420, still 1 row.
    let backfilled = json!({
        "step_index": 5,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 300,
        "cache_read_tokens": 100,
        "output_tokens": 20
    });
    writeln!(f2, "{backfilled}")?;
    f2.flush()?;
    let u_full = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_full.today, Some(420));
    assert_eq!(u_full.today_complete_steps, 1);
    assert_eq!(u_full.today_incomplete_steps, 0);
    assert_eq!(u_full.latest, Some(420));
    assert!(!u_full.latest_incomplete);
    let count: i64 = store2
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count, 1);

    // Replaying an earlier incomplete record must NOT overwrite the confirmed complete value.
    writeln!(f2, "{missing_cache}")?;
    f2.flush()?;
    let u_replay = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_replay.today, Some(420));
    assert_eq!(u_replay.latest, Some(420));
    assert!(!u_replay.latest_incomplete);
    let count_after: i64 = store2
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count_after, 1);

    // Mixed day: adding a second incomplete step leaves today = Some(420) (complete steps only)
    // while flagging today_incomplete_steps = 1 and latest_incomplete = true.
    let step6_incomplete = json!({
        "step_index": 6,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(3, 0),
        "input_tokens": 150,
        "output_tokens": 10
    });
    writeln!(f2, "{step6_incomplete}")?;
    f2.flush()?;
    let u_mixed = mon2.update(&store2, "antigravity", &brain2)?;
    assert_eq!(u_mixed.today, Some(420));
    assert_eq!(u_mixed.today_complete_steps, 1);
    assert_eq!(u_mixed.today_incomplete_steps, 1);
    assert_eq!(u_mixed.latest, None);
    assert!(u_mixed.latest_incomplete);
    Ok(())
}

#[test]
fn antigravity_subsecond_latest_and_deterministic_tie_breaker() -> anyhow::Result<()> {
    let today = chrono::Local::now().date_naive();
    let ts = |sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(10, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };

    let conv_a_event = json!({
        "step_index": 90,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 100),
        "input_tokens": 80,
        "cache_read_tokens": 10,
        "output_tokens": 10
    });
    let conv_b_event = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(1, 900),
        "input_tokens": 150,
        "cache_read_tokens": 30,
        "output_tokens": 20
    });

    // Verify both import orders (A then B, and B then A) yield latest = 200 and today = 300.
    for order in [["conv-a", "conv-b"], ["conv-b", "conv-a"]] {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("monitor.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain)?;
        let mut mon = Monitor::default();

        for conv in order {
            let logs = brain.join(conv).join(".system_generated/logs");
            fs::create_dir_all(&logs)?;
            let ev = if conv == "conv-a" { &conv_a_event } else { &conv_b_event };
            fs::write(logs.join("transcript.jsonl"), format!("{ev}\n"))?;
            mon.update(&store, "antigravity", &brain)?;
        }

        let u = mon.update(&store, "antigravity", &brain)?;
        assert_eq!(u.latest, Some(200), "order={order:?}");
        assert_eq!(u.today, Some(300), "order={order:?}");
    }

    // Exact same timestamp tie-breaker: independent of import order and survives restart.
    let tie_a = json!({
        "step_index": 90,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 100,
        "cache_read_tokens": 0,
        "output_tokens": 11
    });
    let tie_b = json!({
        "step_index": 1,
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": ts(2, 500),
        "input_tokens": 200,
        "cache_read_tokens": 0,
        "output_tokens": 22
    });

    let mut observed = Vec::new();
    for order in [["conv-a", "conv-b"], ["conv-b", "conv-a"]] {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("monitor.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        fs::create_dir_all(&brain)?;
        let mut mon = Monitor::default();

        for conv in order {
            let logs = brain.join(conv).join(".system_generated/logs");
            fs::create_dir_all(&logs)?;
            let ev = if conv == "conv-a" { &tie_a } else { &tie_b };
            fs::write(logs.join("transcript.jsonl"), format!("{ev}\n"))?;
            mon.update(&store, "antigravity", &brain)?;
        }

        let u1 = mon.update(&store, "antigravity", &brain)?;
        let u2 = Monitor::default().update(&store, "antigravity", &brain)?;
        assert_eq!(u1.latest, u2.latest);
        assert_eq!(u1.today, Some(333));
        assert_eq!(u2.today, Some(333));
        observed.push(u1.latest);
    }
    // session DESC picks conv-b (222) deterministically in both import orders.
    assert_eq!(observed, vec![Some(222), Some(222)]);
    Ok(())
}

#[test]
fn legacy_db_upgrade_backfills_antigravity_preserves_deleted_logs_and_keeps_codex_cursors()
-> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("monitor.sqlite");
    let brain = dir.path().join("brain");
    let codex_root = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_root)?;

    let today = chrono::Local::now().date_naive();
    let ts_dt = |hour: u32, sec: u32, ms: u32| {
        today
            .and_hms_milli_opt(hour, 0, sec, ms)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
    };

    // On-disk Antigravity logs:
    // - conv-a step 90 at 10:00:01.100 (100 tokens, complete)
    // - conv-b step 1  at 10:00:01.900 (200 tokens, complete)
    // - conv-c step 1  at 09:30:00.500 (input 300, output 20, missing cache_read -> incomplete)
    let logs_a = brain.join("conv-a/.system_generated/logs");
    let logs_b = brain.join("conv-b/.system_generated/logs");
    let logs_c = brain.join("conv-c/.system_generated/logs");
    fs::create_dir_all(&logs_a)?;
    fs::create_dir_all(&logs_b)?;
    fs::create_dir_all(&logs_c)?;

    let file_a = logs_a.join("transcript.jsonl");
    let file_b = logs_b.join("transcript.jsonl");
    let file_c = logs_c.join("transcript.jsonl");
    let codex_file = codex_root.join("rollout.jsonl");

    fs::write(
        &file_a,
        format!(
            "{}\n",
            json!({
                "step_index": 90, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(10, 1, 100).to_rfc3339(),
                "input_tokens": 80, "cache_read_tokens": 10, "output_tokens": 10
            })
        ),
    )?;
    fs::write(
        &file_b,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(10, 1, 900).to_rfc3339(),
                "input_tokens": 150, "cache_read_tokens": 30, "output_tokens": 20
            })
        ),
    )?;
    fs::write(
        &file_c,
        format!(
            "{}\n",
            json!({
                "step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": ts_dt(9, 30, 500).to_rfc3339(),
                "input_tokens": 300, "output_tokens": 20
            })
        ),
    )?;
    fs::write(
        &codex_file,
        format!(
            "{}\n",
            json!({
                "type": "token_usage_record",
                "timestamp": ts_dt(8, 0, 0).to_rfc3339(),
                "payload": {"session_id": "s1", "response_id": "r1", "usage": {"total_tokens": 5000}}
            })
        ),
    )?;

    // Build an authentic legacy v0 SQLite database (5-column usage table, EOF cursors).
    {
        let con = rusqlite::Connection::open(&db_path)?;
        con.execute_batch(
            "PRAGMA user_version=0;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;

        // Seed legacy rows: Codex (5000), Antigravity A/B/C (on disk), Antigravity D (source log deleted!).
        con.execute(
            "INSERT INTO usage VALUES('codex','s1','r1',?1,5000)",
            [ts_dt(8, 0, 0).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-a','90',?1,100)",
            [ts_dt(10, 1, 100).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-b','1',?1,200)",
            [ts_dt(10, 1, 900).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-c','1',?1,320)",
            [ts_dt(9, 30, 500).timestamp()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-deleted','1',?1,777)",
            [ts_dt(9, 0, 0).timestamp()],
        )?;

        // Seed EOF cursors for Codex and Antigravity (using dummy fingerprints & exact file lengths).
        con.execute(
            "INSERT INTO cursors VALUES('codex', ?1, 'codex-fp', ?2)",
            rusqlite::params![codex_file.to_string_lossy(), codex_file.metadata()?.len() as i64],
        )?;
        for f in [&file_a, &file_b, &file_c] {
            con.execute(
                "INSERT INTO cursors VALUES('antigravity', ?1, 'old-fp', ?2)",
                rusqlite::params![f.to_string_lossy(), f.metadata()?.len() as i64],
            )?;
        }
    }

    // Open with new Store -> upgrades schema to v3, resets ONLY Antigravity cursors.
    let store = Store::open(&db_path)?;
    {
        let con = store.connect()?;
        let ver: i32 = con.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(ver, 3);
        assert!(store.antigravity_repair_pending()?);
        let codex_cursor_fp: String = con.query_row(
            "SELECT fingerprint FROM cursors WHERE provider='codex'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(codex_cursor_fp, "codex-fp", "Codex cursor must stay untouched");
        let ag_cursors_before: i64 = con.query_row(
            "SELECT COUNT(*) FROM cursors WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(ag_cursors_before, 0, "Antigravity cursors should be cleared once for backfill");
    }

    // Run Antigravity monitor after upgrade:
    // - conv-a & conv-b recover sub-second precision -> latest = 200 (conv-b 10:00:01.900)
    // - conv-c is corrected from false-complete 320 to incomplete (today_incomplete_steps = 1)
    // - conv-deleted is preserved SEPARATELY as legacy history (777 tokens, today_legacy_steps = 1)
    // - verified complete today sum = 100 + 200 = 300 (legacy 777 is NOT mixed into verified sum!)
    let u_after = Monitor::default().update(&store, "antigravity", &brain)?;
    assert!(!store.antigravity_repair_pending()?);
    assert_eq!(u_after.latest, Some(200));
    assert_eq!(u_after.today, Some(300));
    assert_eq!(u_after.today_complete_steps, 2);
    assert_eq!(u_after.today_incomplete_steps, 1);
    assert_eq!(u_after.today_legacy_steps, 1);
    assert_eq!(u_after.today_legacy_tokens, Some(777));
    assert!(!u_after.latest_incomplete);

    let ag_row_count: i64 = store
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(ag_row_count, 4);

    // Reopening Store and Monitor again must NOT clear cursors or double history.
    let store_reopened = Store::open(&db_path)?;
    let ag_cursors_after: i64 = store_reopened.connect()?.query_row(
        "SELECT COUNT(*) FROM cursors WHERE provider='antigravity'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(ag_cursors_after, 3);

    let u_reopened = Monitor::default().update(&store_reopened, "antigravity", &brain)?;
    assert_eq!(u_reopened.latest, Some(200));
    assert_eq!(u_reopened.today, Some(300));
    assert_eq!(u_reopened.today_incomplete_steps, 1);
    assert_eq!(u_reopened.today_legacy_steps, 1);
    assert_eq!(u_reopened.today_legacy_tokens, Some(777));

    // Also verify History daily/weekly/monthly totals do not double across reopens.
    let hist_day = history::query(&store_reopened, Granularity::Day, 0, Some("antigravity"), None, None)?;
    let today_bucket = hist_day.buckets.last().unwrap();
    assert_eq!(today_bucket.verified_tokens, Some(300));
    assert_eq!(today_bucket.incomplete_steps, 1);
    assert_eq!(today_bucket.legacy_steps, 1);
    assert_eq!(today_bucket.legacy_tokens, Some(777));

    // Codex history remained untouched (5000, legacy = 0).
    let (codex_tokens, codex_legacy): (i64, i64) = store_reopened.connect()?.query_row(
        "SELECT tokens, legacy FROM usage WHERE provider='codex'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((codex_tokens, codex_legacy), (5000, 0));

    Ok(())
}

#[test]
fn history_cross_day_week_month_and_cross_year_boundaries() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate, TimeZone};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions)?;
    let write_ev = |id: &str, rfc: &str, tokens: i64| {
        json!({
            "type": "token_usage_record",
            "timestamp": rfc,
            "payload": {"session_id": "s1", "response_id": id, "usage": {"total_tokens": tokens}}
        })
    };
    fs::write(
        sessions.join("boundaries.jsonl"),
        [
            // Cross-year: 2025-12-31 23:59:59+08:00 (50) and 2026-01-01 00:00:00+08:00 (75)
            write_ev("y1", "2025-12-31T23:59:59+08:00", 50),
            write_ev("y2", "2026-01-01T00:00:00+08:00", 75),
            // Acceptance example: 2026-11-29 Sun 23:59:59 (100), 11-30 Mon 00:00:00 (200), 12-01 Tue 00:00:00 (300)
            write_ev("r1", "2026-11-29T23:59:59+08:00", 100),
            write_ev("r2", "2026-11-30T00:00:00+08:00", 200),
            write_ev("r3", "2026-12-01T00:00:00+08:00", 300),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "codex", &sessions)?;

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();

    // Daily check: 11-29 = 100, 11-30 = 200, 12-01 = 300
    let day_page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    assert_eq!(day_page.buckets.len(), 30);
    assert!(day_page.has_older);
    assert!(!day_page.has_newer);
    let by_day = |k: &str| day_page.buckets.iter().find(|b| b.key == k).unwrap().verified_tokens;
    assert_eq!(by_day("2026-11-29"), Some(100));
    assert_eq!(by_day("2026-11-30"), Some(200));
    assert_eq!(by_day("2026-12-01"), Some(300));

    // Weekly check (Monday start): 11-23..11-29 = 100, 11-30..12-06 = 500 (is_current)
    let week_page = history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(week_page.buckets.len(), 12);
    let w_prev = week_page.buckets.iter().find(|b| b.key == "2026-11-23..2026-11-29").unwrap();
    let w_curr = week_page.buckets.iter().find(|b| b.key == "2026-11-30..2026-12-06").unwrap();
    assert_eq!(w_prev.verified_tokens, Some(100));
    assert!(!w_prev.is_current);
    assert_eq!(w_curr.verified_tokens, Some(500));
    assert!(w_curr.is_current);

    // Drill-down on current week sums to 500
    let w_drill = history::query_with_tz(
        &store,
        Granularity::Day,
        0,
        None,
        Some(NaiveDate::from_ymd_opt(2026, 11, 30).unwrap()),
        Some(NaiveDate::from_ymd_opt(2026, 12, 6).unwrap()),
        today,
        &tz,
    )?;
    assert_eq!(w_drill.summary.verified_tokens, Some(500));

    // Monthly check: 2026-11 = 300, 2026-12 = 300, 2026-01 = 75; page offset=1 reaches 2025-12 = 50
    let month_page = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(month_page.buckets.len(), 12);
    assert!(month_page.has_older);
    let by_month = |k: &str| month_page.buckets.iter().find(|b| b.key == k).unwrap().verified_tokens;
    assert_eq!(by_month("2026-01"), Some(75));
    assert_eq!(by_month("2026-11"), Some(300));
    assert_eq!(by_month("2026-12"), Some(300));

    let older_months = history::query_with_tz(&store, Granularity::Month, 1, None, None, None, today, &tz)?;
    assert!(older_months.has_newer);
    assert!(!older_months.has_older);
    assert_eq!(
        older_months.buckets.iter().find(|b| b.key == "2025-12").unwrap().verified_tokens,
        Some(50)
    );
    let _ = tz.timestamp_opt(0, 0);
    Ok(())
}

#[test]
fn history_partial_backfill_merges_fields_and_preserves_original_date() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("antigravity", "installed")])?;
    store.select(vec!["antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-backfill/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    let transcript = logs.join("transcript.jsonl");

    // Nov 29 reports input=300, output=20 (no cache_read_tokens)
    let mut f = fs::File::create(&transcript)?;
    writeln!(
        f,
        "{}",
        json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 300,
            "output_tokens": 20
        })
    )?;
    f.flush()?;

    let mut mon = Monitor::default();
    mon.update(&store, "antigravity", &brain)?;

    // Dec 1 backfills ONLY cache_read_tokens=100 for the same step
    writeln!(
        f,
        "{}",
        json!({
            "step_index": 5,
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-12-01T10:00:00+08:00",
            "cache_read_tokens": 100
        })
    )?;
    f.flush()?;
    mon.update(&store, "antigravity", &brain)?;

    let count: i64 = store
        .connect()?
        .query_row("SELECT COUNT(*) FROM usage WHERE provider='antigravity'", [], |r| r.get(0))?;
    assert_eq!(count, 1);

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
    let day_page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    assert_eq!(
        day_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens,
        Some(420)
    );
    assert_eq!(
        day_page.buckets.iter().find(|b| b.key == "2026-12-01").unwrap().verified_tokens,
        None
    );

    let week_page = history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(
        week_page.buckets.iter().find(|b| b.key == "2026-11-23..2026-11-29").unwrap().verified_tokens,
        Some(420)
    );
    assert_eq!(
        week_page.buckets.iter().find(|b| b.key == "2026-11-30..2026-12-06").unwrap().verified_tokens,
        None
    );

    let month_page = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(
        month_page.buckets.iter().find(|b| b.key == "2026-11").unwrap().verified_tokens,
        Some(420)
    );
    assert_eq!(
        month_page.buckets.iter().find(|b| b.key == "2026-12").unwrap().verified_tokens,
        None
    );
    Ok(())
}

#[test]
fn history_distinguishes_empty_zero_all_missing_legacy_and_disabled_provider() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || {
        vec![agent("codex", "installed"), agent("antigravity", "installed")]
    })?;
    store.select(vec!["codex".into(), "antigravity".into()])?;

    let brain = dir.path().join("brain");
    let logs = brain.join("conv-states/.system_generated/logs");
    fs::create_dir_all(&logs)?;
    // Day 2026-11-21: empty (no record)
    // Day 2026-11-22: explicit zero (0, 0, 0)
    // Day 2026-11-23: DONE step with all 3 token fields missing
    // Day 2026-11-24: legacy-only row inserted directly (simulating deleted source log)
    // Day 2026-11-25: Codex 100 + Antigravity 200 -> All = 300
    fs::write(
        logs.join("transcript.jsonl"),
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-22T10:00:00+08:00", "input_tokens": 0, "cache_read_tokens": 0, "output_tokens": 0}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-23T10:00:00+08:00"}),
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-25T10:00:00+08:00", "input_tokens": 150, "cache_read_tokens": 30, "output_tokens": 20}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "antigravity", &brain)?;

    let codex_dir = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_dir)?;
    fs::write(
        codex_dir.join("rollout.jsonl"),
        format!(
            "{}\n",
            json!({
                "type": "token_usage_record",
                "timestamp": "2026-11-25T11:00:00+08:00",
                "payload": {"session_id": "s1", "response_id": "r1", "usage": {"total_tokens": 100}}
            })
        ),
    )?;
    Monitor::default().update(&store, "codex", &codex_dir)?;

    // Insert a legacy-only Antigravity row on 2026-11-24
    let legacy_ts = chrono::DateTime::parse_from_rfc3339("2026-11-24T12:00:00+08:00")?.timestamp();
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('antigravity','deleted-conv','1',?1,777,NULL,1,1)",
        [legacy_ts],
    )?;

    let today = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
    let bucket = |k: &str| page.buckets.iter().find(|b| b.key == k).unwrap();

    // 1. Empty
    let b21 = bucket("2026-11-21");
    assert_eq!((b21.verified_tokens, b21.incomplete_steps, b21.legacy_steps), (None, 0, 0));
    // 2. Explicit zero
    let b22 = bucket("2026-11-22");
    assert_eq!((b22.verified_tokens, b22.verified_steps, b22.incomplete_steps), (Some(0), 1, 0));
    // 3. All 3 fields missing -> preserved as incomplete with incomplete_known = None
    let b23 = bucket("2026-11-23");
    assert_eq!(b23.verified_tokens, None);
    assert_eq!(b23.incomplete_steps, 1);
    assert_eq!(b23.incomplete_known, None);
    // 4. Legacy-only -> separated from verified_tokens, legacy_tokens = Some(777)
    let b24 = bucket("2026-11-24");
    assert_eq!(b24.verified_tokens, None);
    assert_eq!(b24.legacy_steps, 1);
    assert_eq!(b24.legacy_tokens, Some(777));
    // 5. Codex 100 + Antigravity 200 -> All = 300
    let b25 = bucket("2026-11-25");
    assert_eq!(b25.verified_tokens, Some(300));
    assert_eq!(b25.providers.len(), 2);

    // Disable Antigravity: its saved 200 must still be queryable without re-enabling collection!
    store.select(vec!["codex".into()])?;
    let ag_only = history::query_with_tz(&store, Granularity::Day, 0, Some("antigravity"), None, None, today, &tz)?;
    assert_eq!(
        ag_only.buckets.iter().find(|b| b.key == "2026-11-25").unwrap().verified_tokens,
        Some(200)
    );
    assert!(
        !ag_only
            .available_providers
            .iter()
            .find(|p| p.id == "antigravity")
            .unwrap()
            .enabled
    );
    assert!(store.enabled_agent("antigravity").is_err());
    Ok(())
}

#[test]
fn history_dst_23h_and_25h_days_do_not_assume_86400_seconds() -> anyhow::Result<()> {
    use chrono::{FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

    // Minimal US-like DST timezone:
    // - Spring forward: 2026-03-08 02:00 EST (-5) -> 03:00 EDT (-4) (23-hour day)
    // - Fall back:      2026-11-01 02:00 EDT (-4) -> 01:00 EST (-5) (25-hour day)
    #[derive(Clone, Copy)]
    struct MockDstTz;
    impl MockDstTz {
        fn is_dst_utc(utc_ts: i64) -> bool {
            // 2026-03-08 07:00:00Z .. 2026-11-01 06:00:00Z
            let start = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap().and_hms_opt(7, 0, 0).unwrap().and_utc().timestamp();
            let end = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap().and_hms_opt(6, 0, 0).unwrap().and_utc().timestamp();
            utc_ts >= start && utc_ts < end
        }
    }
    impl TimeZone for MockDstTz {
        type Offset = FixedOffset;
        fn from_offset(_offset: &Self::Offset) -> Self { MockDstTz }
        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let est = FixedOffset::west_opt(5 * 3600).unwrap();
            let edt = FixedOffset::west_opt(4 * 3600).unwrap();
            let est_utc = local.and_utc().timestamp() + 5 * 3600;
            let edt_utc = local.and_utc().timestamp() + 4 * 3600;
            let valid_est = !Self::is_dst_utc(est_utc);
            let valid_edt = Self::is_dst_utc(edt_utc);
            match (valid_edt, valid_est) {
                (true, false) => LocalResult::Single(edt),
                (false, true) => LocalResult::Single(est),
                (true, true) => LocalResult::Ambiguous(edt, est),
                (false, false) => LocalResult::None,
            }
        }
        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            if Self::is_dst_utc(utc.and_utc().timestamp()) {
                FixedOffset::west_opt(4 * 3600).unwrap()
            } else {
                FixedOffset::west_opt(5 * 3600).unwrap()
            }
        }
    }

    // Verify 23-hour spring-forward day and 25-hour fall-back day boundaries!
    let s1 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), &MockDstTz)?;
    let s2 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(), &MockDstTz)?;
    assert_eq!(s2 - s1, 23 * 3600);

    let f1 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), &MockDstTz)?;
    let f2 = history::local_day_start_sec(NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(), &MockDstTz)?;
    assert_eq!(f2 - f1, 25 * 3600);

    // Event at 23:30 EST on the 25-hour day (24.5 hours after 00:00 EDT!) must STILL land in 2026-11-01!
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;
    let late_25h_ts = f1 + 24 * 3600 + 1800; // 23:30 EST on 2026-11-01
    store.connect()?.execute(
        "INSERT INTO usage(provider,session,response,at,tokens,at_ns,complete,legacy)
         VALUES('codex','s1','dst1',?1,456,?2,1,0)",
        rusqlite::params![late_25h_ts, late_25h_ts * 1_000_000_000],
    )?;

    let page = history::query_with_tz(
        &store,
        Granularity::Day,
        0,
        None,
        None,
        None,
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
        &MockDstTz,
    )?;
    assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-01").unwrap().verified_tokens, Some(456));
    assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-02").unwrap().verified_tokens, None);
    Ok(())
}

#[test]
fn antigravity_out_of_order_field_merge_and_restart_persistence() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();

    // Sequence 1: Nov 27 complete 300 -> Nov 29 complete 500 -> replay Nov 28 complete 400.
    // Final: 1 step, 500 tokens attributed to Nov 27; replaying again yields identical state.
    {
        let dir = tempdir()?;
        let db_path = dir.path().join("seq1.sqlite");
        let store = Store::open(&db_path)?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;

        let brain = dir.path().join("brain");
        let logs = brain.join("conv-seq1/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let transcript = logs.join("transcript.jsonl");
        let mut f = fs::File::create(&transcript)?;

        let e27 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-27T10:00:00+08:00",
            "input_tokens": 200, "cache_read_tokens": 50, "output_tokens": 50
        });
        let e29 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-29T10:00:00+08:00",
            "input_tokens": 350, "cache_read_tokens": 100, "output_tokens": 50
        });
        let e28 = json!({
            "step_index": 7, "type": "PLANNER_RESPONSE", "status": "DONE",
            "created_at": "2026-11-28T10:00:00+08:00",
            "input_tokens": 250, "cache_read_tokens": 100, "output_tokens": 50
        });

        writeln!(f, "{e27}")?;
        writeln!(f, "{e29}")?;
        writeln!(f, "{e28}")?;
        f.flush()?;

        let mut mon = Monitor::default();
        mon.update(&store, "antigravity", &brain)?;

        // Replay Nov 28 again
        writeln!(f, "{e28}")?;
        f.flush()?;
        mon.update(&store, "antigravity", &brain)?;

        let count: i64 = store.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);

        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-27").unwrap().verified_tokens, Some(500));
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap().verified_tokens, None);
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens, None);
    }

    // Sequence 2: Dec 1 complete 420 -> restart -> Nov 29 earlier partial record.
    // Final: keeps complete 420, attributed to Nov 29; Dec 1 no longer has this step.
    {
        let dir = tempdir()?;
        let db_path = dir.path().join("seq2.sqlite");
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-seq2/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        let transcript = logs.join("transcript.jsonl");

        {
            let store = Store::open(&db_path)?;
            store.settings(false, || vec![agent("antigravity", "installed")])?;
            store.select(vec!["antigravity".into()])?;
            fs::write(
                &transcript,
                format!(
                    "{}\n",
                    json!({
                        "step_index": 9, "type": "PLANNER_RESPONSE", "status": "DONE",
                        "created_at": "2026-12-01T10:00:00+08:00",
                        "input_tokens": 300, "cache_read_tokens": 100, "output_tokens": 20
                    })
                ),
            )?;
            Monitor::default().update(&store, "antigravity", &brain)?;
        }

        // Restart Store & Monitor, then append earlier Nov 29 partial record
        let store = Store::open(&db_path)?;
        let mut f = fs::OpenOptions::new().append(true).open(&transcript)?;
        writeln!(
            f,
            "{}",
            json!({
                "step_index": 9, "type": "PLANNER_RESPONSE", "status": "DONE",
                "created_at": "2026-11-29T10:00:00+08:00",
                "input_tokens": 280, "output_tokens": 15
            })
        )?;
        f.flush()?;
        Monitor::default().update(&store, "antigravity", &brain)?;

        let count: i64 = store.connect()?.query_row(
            "SELECT COUNT(*) FROM usage WHERE provider='antigravity'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1);

        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let day_page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
        assert_eq!(day_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens, Some(420));
        assert_eq!(day_page.buckets.iter().find(|b| b.key == "2026-12-01").unwrap().verified_tokens, None);

        let month_page = history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
        assert_eq!(month_page.buckets.iter().find(|b| b.key == "2026-11").unwrap().verified_tokens, Some(420));
        assert_eq!(month_page.buckets.iter().find(|b| b.key == "2026-12").unwrap().verified_tokens, None);
    }

    // Explicit 0 is a valid newer value and must not be overwritten by later missing field.
    {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("zero.sqlite"))?;
        store.settings(false, || vec![agent("antigravity", "installed")])?;
        store.select(vec!["antigravity".into()])?;
        let brain = dir.path().join("brain");
        let logs = brain.join("conv-zero/.system_generated/logs");
        fs::create_dir_all(&logs)?;
        fs::write(
            logs.join("transcript.jsonl"),
            [
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-30T09:00:00+08:00", "cache_read_tokens": 0}),
                json!({"step_index": 3, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T09:00:00+08:00", "input_tokens": 310}),
            ]
            .map(|v| v.to_string())
            .join("\n")
                + "\n",
        )?;
        Monitor::default().update(&store, "antigravity", &brain)?;
        let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let page = history::query_with_tz(&store, Granularity::Day, 0, None, None, None, today, &tz)?;
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap().verified_tokens, Some(330));
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-11-30").unwrap().verified_tokens, None);
        assert_eq!(page.buckets.iter().find(|b| b.key == "2026-12-01").unwrap().verified_tokens, None);
    }

    Ok(())
}

#[test]
fn v2_upgrade_repairs_antigravity_and_resumes_after_interruption() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let db_v2 = dir.path().join("from_v2.sqlite");
    let db_fresh = dir.path().join("fresh_v3.sqlite");
    let brain = dir.path().join("brain");
    let codex_root = dir.path().join("codex-sessions");
    fs::create_dir_all(&codex_root)?;

    let logs1 = brain.join("conv-main/.system_generated/logs");
    let logs2 = brain.join("conv-second/.system_generated/logs");
    fs::create_dir_all(&logs1)?;
    fs::create_dir_all(&logs2)?;
    let t1 = logs1.join("transcript.jsonl");
    let t2 = logs2.join("transcript.jsonl");

    // conv-main: Step A (DONE, all 3 fields missing) + Step B (Nov 29 in=300, out=20; Dec 1 cache_read=100)
    fs::write(
        &t1,
        [
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T09:00:00+08:00"}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-29T10:00:00+08:00", "input_tokens": 300, "output_tokens": 20}),
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-12-01T10:00:00+08:00", "cache_read_tokens": 100}),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    fs::write(
        &t2,
        format!(
            "{}\n",
            json!({"step_index": 1, "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-11-30T11:00:00+08:00", "input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 25})
        ),
    )?;

    // Build a v2 database matching 7d76bf1's schema & EOF state (Step A skipped, Step B overwritten with Dec 1 partial 100).
    {
        let con = rusqlite::Connection::open(&db_v2)?;
        con.execute_batch(
            "PRAGMA user_version=2;
             CREATE TABLE settings(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE last_quota(id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL);
             CREATE TABLE usage(provider TEXT, session TEXT, response TEXT, at INTEGER NOT NULL, tokens INTEGER NOT NULL, at_ns INTEGER, complete INTEGER NOT NULL DEFAULT 1, input_tokens INTEGER, cache_read_tokens INTEGER, output_tokens INTEGER, legacy INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider,session,response));
             CREATE INDEX usage_date ON usage(provider,at);
             CREATE TABLE cursors(provider TEXT, path TEXT, fingerprint TEXT, pos INTEGER NOT NULL, PRIMARY KEY(provider,path));",
        )?;
        let settings = json!({
            "agents": [
                {"id": "codex", "name": "OpenAI Codex", "status": "installed", "binary": null},
                {"id": "antigravity", "name": "Antigravity", "status": "installed", "binary": null}
            ],
            "enabled": ["codex", "antigravity"],
            "confirmed": true
        });
        con.execute("INSERT INTO settings VALUES(1, ?1)", [settings.to_string()])?;
        let dec1 = chrono::DateTime::parse_from_rfc3339("2026-12-01T10:00:00+08:00")?;
        let nov28 = chrono::DateTime::parse_from_rfc3339("2026-11-28T10:00:00+08:00")?;
        let nov29 = chrono::DateTime::parse_from_rfc3339("2026-11-29T08:00:00+08:00")?;
        // 7d76bf1 skipped Step A, left Step B as Dec 1 partial 100, and had a deleted-log row (777) + Codex row (5000).
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-main','2',?1,100,?2,0,NULL,100,NULL,0)",
            rusqlite::params![dec1.timestamp(), dec1.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('antigravity','conv-deleted','1',?1,777,?2,1,700,50,27,0)",
            rusqlite::params![nov28.timestamp(), nov28.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO usage VALUES('codex','s1','r1',?1,5000,?2,1,NULL,NULL,NULL,0)",
            rusqlite::params![nov29.timestamp(), nov29.timestamp_nanos_opt()],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('codex', '/fake/codex.jsonl', 'codex-fp', 160)",
            [],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, 'old-fp', ?2)",
            rusqlite::params![t1.to_string_lossy(), t1.metadata()?.len() as i64],
        )?;
        con.execute(
            "INSERT INTO cursors VALUES('antigravity', ?1, 'old-fp2', ?2)",
            rusqlite::params![t2.to_string_lossy(), t2.metadata()?.len() as i64],
        )?;
    }

    // Open with new Store (v3 migration runs) and simulate interruption: hide conv-second during first repair pass,
    // wait — simulate real mid-repair failure by making conv-second temporarily unreadable on unix (or checking pending state).
    let store = Store::open(&db_v2)?;
    assert!(store.antigravity_repair_pending()?);

    // Simulate interruption: first repair pass only sees conv-main (rename conv-second away temporarily and force pending back),
    // verify conv-main gets EOF cursor & repaired rows, then restore conv-second while repair is still pending and resume!
    let t2_backup = dir.path().join("conv-second-backup");
    fs::rename(brain.join("conv-second"), &t2_backup)?;
    Monitor::default().update(&store, "antigravity", &brain)?;
    // Simulate crash right before mark_antigravity_repaired committed while conv-main was already committed at EOF:
    store.connect()?.execute(
        "INSERT OR REPLACE INTO meta(key,value) VALUES('antigravity_repair_version','2')",
        [],
    )?;
    fs::rename(&t2_backup, brain.join("conv-second"))?;

    // Reopen Store & Monitor: resumes repair, skips already-repaired conv-main, repairs conv-second, marks v3 done!
    let store = Store::open(&db_v2)?;
    assert!(store.antigravity_repair_pending()?);
    Monitor::default().update(&store, "antigravity", &brain)?;
    assert!(!store.antigravity_repair_pending()?);

    // Build a fresh v3 DB from the exact same on-disk transcripts for field-by-field comparison.
    let fresh = Store::open(&db_fresh)?;
    fresh.settings(false, || vec![agent("antigravity", "installed")])?;
    fresh.select(vec!["antigravity".into()])?;
    Monitor::default().update(&fresh, "antigravity", &brain)?;

    type RowDump = (String, String, i64, Option<i64>, i64, i64, Option<i64>, Option<i64>, Option<i64>, i64);
    let dump = |s: &Store| -> anyhow::Result<Vec<RowDump>> {
        let con = s.connect()?;
        let mut stmt = con.prepare(
            "SELECT session, response, at, at_ns, tokens, complete, input_tokens, cache_read_tokens, output_tokens, legacy
             FROM usage WHERE provider='antigravity' AND session != 'conv-deleted' ORDER BY session, response",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    };
    assert_eq!(dump(&store)?, dump(&fresh)?);

    // Reopen twice more: zero duplication, repair stays done, Codex untouched, conv-deleted preserved as legacy.
    for _ in 0..2 {
        let s = Store::open(&db_v2)?;
        assert!(!s.antigravity_repair_pending()?);
        Monitor::default().update(&s, "antigravity", &brain)?;
        assert_eq!(dump(&s)?, dump(&fresh)?);
    }

    let today = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
    let day_page = history::query_with_tz(&store, Granularity::Day, 0, Some("antigravity"), None, None, today, &tz)?;
    let b28 = day_page.buckets.iter().find(|b| b.key == "2026-11-28").unwrap();
    assert_eq!((b28.verified_tokens, b28.legacy_steps, b28.legacy_tokens), (None, 1, Some(777)));
    let b29 = day_page.buckets.iter().find(|b| b.key == "2026-11-29").unwrap();
    assert_eq!((b29.verified_tokens, b29.verified_steps, b29.incomplete_steps, b29.incomplete_known.clone()), (Some(420), 1, 1, None));
    let b30 = day_page.buckets.iter().find(|b| b.key == "2026-11-30").unwrap();
    assert_eq!(b30.verified_tokens, Some(175));
    let b01 = day_page.buckets.iter().find(|b| b.key == "2026-12-01").unwrap();
    assert_eq!((b01.verified_tokens, b01.incomplete_steps), (None, 0));

    let codex_fp: String = store.connect()?.query_row("SELECT fingerprint FROM cursors WHERE provider='codex'", [], |r| r.get(0))?;
    assert_eq!(codex_fp, "codex-fp");
    Ok(())
}

#[test]
fn history_year_granularity_monthly_drilldown_and_current_end_date_capping() -> anyhow::Result<()> {
    use chrono::{FixedOffset, NaiveDate};
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let dir = tempdir()?;
    let store = Store::open(&dir.path().join("monitor.sqlite"))?;
    store.settings(false, || vec![agent("codex", "installed")])?;
    store.select(vec!["codex".into()])?;

    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions)?;
    let write_ev = |id: &str, rfc: &str, tokens: i64| {
        json!({
            "type": "token_usage_record",
            "timestamp": rfc,
            "payload": {"session_id": "s1", "response_id": id, "usage": {"total_tokens": tokens}}
        })
    };
    fs::write(
        sessions.join("yearly.jsonl"),
        [
            write_ev("y1", "2025-12-31T23:59:59+08:00", 50),
            write_ev("y2", "2026-01-01T00:00:00+08:00", 75),
            write_ev("y3", "2026-11-25T12:00:00+08:00", 100),
            write_ev("y4", "2026-11-26T12:00:00+08:00", 200),
        ]
        .map(|v| v.to_string())
        .join("\n")
            + "\n",
    )?;
    Monitor::default().update(&store, "codex", &sessions)?;

    let today = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();

    // Week: current week end_date & range_end must cap at today (2026-11-26), not Sunday (2026-11-29).
    let week_page =
        history::query_with_tz(&store, Granularity::Week, 0, None, None, None, today, &tz)?;
    assert_eq!(week_page.range_end, "2026-11-26");
    let w_curr = week_page
        .buckets
        .iter()
        .find(|b| b.key == "2026-11-23..2026-11-29")
        .unwrap();
    assert!(w_curr.is_current);
    assert_eq!(w_curr.end_date, "2026-11-26");
    assert_eq!(w_curr.verified_tokens, Some(300));

    // Month: current month end_date & range_end must cap at today (2026-11-26), not month-end (2026-11-30).
    let month_page =
        history::query_with_tz(&store, Granularity::Month, 0, None, None, None, today, &tz)?;
    assert_eq!(month_page.range_end, "2026-11-26");
    let m_curr = month_page.buckets.iter().find(|b| b.key == "2026-11").unwrap();
    assert!(m_curr.is_current);
    assert_eq!(m_curr.end_date, "2026-11-26");
    assert_eq!(m_curr.verified_tokens, Some(300));

    // Year: returns 5 calendar years (2022..2026), range_end == "2026-11-26".
    let year_page =
        history::query_with_tz(&store, Granularity::Year, 0, None, None, None, today, &tz)?;
    assert_eq!(year_page.buckets.len(), 5);
    assert_eq!(year_page.range_start, "2022-01-01");
    assert_eq!(year_page.range_end, "2026-11-26");
    let y2025 = year_page.buckets.iter().find(|b| b.key == "2025").unwrap();
    assert!(!y2025.is_current);
    assert_eq!(y2025.end_date, "2025-12-31");
    assert_eq!(y2025.verified_tokens, Some(50));
    let y2026 = year_page.buckets.iter().find(|b| b.key == "2026").unwrap();
    assert!(y2026.is_current);
    assert_eq!(y2026.end_date, "2026-11-26");
    assert_eq!(y2026.verified_tokens, Some(375));

    // Year monthly drill-down (2026-01-01 .. 2026-11-26): 11 months (2026-01 .. 2026-11), not 2026-12.
    let y_drill = history::query_with_tz(
        &store,
        Granularity::Month,
        0,
        None,
        Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
        Some(NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()),
        today,
        &tz,
    )?;
    assert_eq!(y_drill.buckets.len(), 11);
    assert_eq!(
        y_drill.buckets.iter().find(|b| b.key == "2026-01").unwrap().verified_tokens,
        Some(75)
    );
    assert_eq!(
        y_drill.buckets.iter().find(|b| b.key == "2026-11").unwrap().verified_tokens,
        Some(300)
    );
    assert_eq!(y_drill.summary.verified_tokens, Some(375));
    Ok(())
}
