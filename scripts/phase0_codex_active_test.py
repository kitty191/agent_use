#!/usr/bin/env python3
"""
Phase 0b Codex Active Query & Live Capture Verification Script (macOS / Cross-platform).

Tests two things with zero model requests sent by this script:
1. Active query via `codex app-server` stdio JSON-RPC:
   - Handshake: `initialize` -> `initialized`
   - Calls `account/read` (read-only, refreshToken=false)
   - Calls `account/rateLimits/read`
   - Outputs sanitized account (masked email + hashed IDs), actual window durations,
     used percentages, and reset times (UTC + local timezone) for side-by-side comparison
     with the official Codex client UI.
   - If any step fails, records the exact failing step and error, and falls back to the
     latest local JSONL `rate_limits` snapshot.
2. Live incremental log capture (`--watch`):
   - Tails `~/.codex/sessions/**/*.jsonl` using byte offsets while the user runs one
     normal request in another terminal.
   - Measures capture latency from event `timestamp` to detection.
   - Verifies deduplication across `token_usage_record` + `event_msg:token_count` so a
     single request is counted exactly once.

Safety guarantees:
- Pure Python 3 stdlib, never reads `~/.codex/auth.json` or any credential/cookie file.
- Never sends `thread/start`, `turn/start`, or any model inference request.
- Masks emails (`k***@example.com`), hashes IDs, strips prompts/replies/paths.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

HOME = Path.home()
EMAIL_RE = re.compile(r"^([^@\s]+)@([^@\s]+)$")


def short_hash(value: Any) -> str:
    digest = hashlib.sha256(str(value).encode("utf-8", errors="ignore")).hexdigest()
    return f"hash:{digest[:10]}"


def mask_email(value: str) -> str:
    m = EMAIL_RE.match(value.strip())
    if not m:
        return "<REDACTED>"
    local, domain = m.group(1), m.group(2)
    prefix = local[0] if local else "*"
    return f"{prefix}***@{domain}"


def fmt_unix_ts(ts: Any) -> dict[str, Any] | None:
    if not isinstance(ts, (int, float)):
        return None
    dt_utc = datetime.fromtimestamp(ts, tz=timezone.utc)
    dt_local = dt_utc.astimezone()
    return {
        "unix_seconds": int(ts),
        "utc": dt_utc.isoformat(),
        "local": dt_local.isoformat(timespec="seconds"),
    }


def parse_iso_utc(ts: str) -> datetime | None:
    try:
        if ts.endswith("Z"):
            ts = ts[:-1] + "+00:00"
        return datetime.fromisoformat(ts).astimezone(timezone.utc)
    except Exception:  # noqa: BLE001
        return None


def classify_window_label(minutes: Any) -> str:
    if not isinstance(minutes, (int, float)):
        return "unknown_window"
    mins = int(minutes)
    if mins == 300:
        return "5h_window (300m)"
    if mins == 10080:
        return "7d_weekly_window (10080m)"
    if mins % 1440 == 0:
        return f"{mins // 1440}d_window ({mins}m)"
    if mins % 60 == 0:
        return f"{mins // 60}h_window ({mins}m)"
    return f"{mins}m_window"


def sanitize_account(raw: Any) -> Any:
    """Sanitize `account/read` response: mask emails, hash IDs, drop tokens/secrets."""
    if not isinstance(raw, dict):
        return raw
    out: dict[str, Any] = {}
    for k, v in raw.items():
        kl = k.lower()
        if any(s in kl for s in ("token", "secret", "key", "cookie", "password", "jwt")):
            continue
        if isinstance(v, dict):
            out[k] = sanitize_account(v)
        elif isinstance(v, list):
            out[k] = [sanitize_account(i) if isinstance(i, dict) else "<REDACTED_ITEM>" for i in v]
        elif isinstance(v, str):
            if "email" in kl or EMAIL_RE.match(v):
                out[k] = mask_email(v)
            elif any(s in kl for s in ("id", "sub", "subject", "org", "workspace", "user")):
                out[k] = short_hash(v)
            elif kl in {"type", "plantype", "plan_type", "plan", "authmode", "auth_mode", "status", "tier"}:
                out[k] = v
            else:
                out[k] = "<REDACTED_STR>"
        elif isinstance(v, (int, float, bool)) or v is None:
            out[k] = v
    return out


def normalize_slot(slot: Any) -> dict[str, Any] | None:
    if not isinstance(slot, dict):
        return None
    mins = slot.get("windowDurationMins", slot.get("window_minutes"))
    used = slot.get("usedPercent", slot.get("used_percent"))
    resets = slot.get("resetsAt", slot.get("resets_at"))
    return {
        "window_minutes": mins,
        "window_label": classify_window_label(mins),
        "used_percent": used,
        "resets_at": fmt_unix_ts(resets),
    }


def normalize_rate_limit_bucket(bucket: Any) -> dict[str, Any] | None:
    if not isinstance(bucket, dict):
        return None
    credits_raw = bucket.get("credits")
    credits_clean = None
    if isinstance(credits_raw, dict):
        credits_clean = {
            "has_credits": credits_raw.get("hasCredits", credits_raw.get("has_credits")),
            "unlimited": credits_raw.get("unlimited"),
            "balance": credits_raw.get("balance"),
        }
    return {
        "limit_id": bucket.get("limitId", bucket.get("limit_id")),
        "limit_name": bucket.get("limitName", bucket.get("limit_name")),
        "plan_type": bucket.get("planType", bucket.get("plan_type")),
        "primary": normalize_slot(bucket.get("primary")),
        "secondary": normalize_slot(bucket.get("secondary")),
        "credits": credits_clean,
    }


def sanitize_rate_limits_response(raw: Any) -> dict[str, Any]:
    if not isinstance(raw, dict):
        return {"raw_type": type(raw).__name__}
    buckets: dict[str, Any] = {}
    by_id = raw.get("rateLimitsByLimitId")
    if isinstance(by_id, dict):
        for bid, bval in by_id.items():
            buckets[str(bid)] = normalize_rate_limit_bucket(bval)
    legacy = normalize_rate_limit_bucket(raw.get("rateLimits"))
    return {
        "has_rateLimitsByLimitId": isinstance(by_id, dict),
        "buckets_by_limit_id": buckets,
        "legacy_rate_limits": legacy,
        "ordinary_usage_allowed": raw.get("ordinaryUsageAllowed"),
    }


class AppServerRpcClient:
    def __init__(self, cmd: list[str]) -> None:
        self.cmd = cmd
        self.proc: subprocess.Popen[str] | None = None
        self._stdout_q: queue.Queue[str | None] = queue.Queue()
        self._stderr_lines: list[str] = []
        self._notifications: list[str] = []
        self._next_id = 1

    def start(self) -> None:
        self.proc = subprocess.Popen(
            self.cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self) -> None:
        assert self.proc and self.proc.stdout
        try:
            for line in self.proc.stdout:
                self._stdout_q.put(line)
        finally:
            self._stdout_q.put(None)

    def _read_stderr(self) -> None:
        assert self.proc and self.proc.stderr
        for line in self.proc.stderr:
            if len(self._stderr_lines) < 30:
                self._stderr_lines.append(line.strip()[:200])

    def send(self, payload: dict[str, Any]) -> None:
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()

    def request(self, method: str, params: dict[str, Any], timeout: float = 10.0) -> tuple[dict[str, Any] | None, str | None]:
        req_id = self._next_id
        self._next_id += 1
        self.send({"id": req_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None, f"timeout after {timeout:.1f}s waiting for {method}"
            try:
                line = self._stdout_q.get(timeout=remaining)
            except queue.Empty:
                return None, f"timeout after {timeout:.1f}s waiting for {method}"
            if line is None:
                rc = self.proc.poll() if self.proc else None
                return None, f"app-server closed stdout unexpectedly (exit_code={rc})"
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(msg, dict):
                continue
            if msg.get("id") == req_id:
                if "error" in msg and msg["error"] is not None:
                    return None, f"rpc_error: {json.dumps(msg['error'], ensure_ascii=False)}"
                return msg.get("result"), None
            if "method" in msg:
                self._notifications.append(str(msg["method"]))

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self.send({"method": method, "params": params})

    def close(self) -> None:
        if not self.proc:
            return
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.terminate()
            self.proc.wait(timeout=2)
        except Exception:  # noqa: BLE001
            try:
                self.proc.kill()
            except OSError:
                pass


def run_active_query(codex_bin: str) -> dict[str, Any]:
    checked_at = datetime.now(timezone.utc).astimezone().isoformat(timespec="seconds")
    result: dict[str, Any] = {
        "checked_at_local": checked_at,
        "codex_binary_found": bool(codex_bin),
        "steps": {},
        "account": None,
        "rate_limits": None,
        "fallback_used": False,
    }
    if not codex_bin:
        result["steps"]["spawn"] = {"ok": False, "error": "codex binary not found in PATH"}
        result["fallback_used"] = True
        return result

    cmd_candidates = [
        [codex_bin, "-c", "analytics.enabled=false", "app-server", "--listen", "stdio://"],
        [codex_bin, "app-server"],
    ]

    last_err = None
    for idx, cmd in enumerate(cmd_candidates):
        client = AppServerRpcClient(cmd)
        try:
            t0 = time.monotonic()
            client.start()
            init_res, init_err = client.request(
                "initialize",
                {
                    "clientInfo": {
                        "name": "agent_use_phase0_probe",
                        "title": "Agent Use Phase 0 Probe",
                        "version": "0.1.0",
                    },
                    "capabilities": {},
                },
                timeout=10.0,
            )
            if init_err:
                last_err = f"candidate[{idx}] initialize failed: {init_err}"
                result["steps"][f"spawn_candidate_{idx}"] = {
                    "ok": False,
                    "error": init_err,
                    "stderr_tail": client._stderr_lines[-5:],
                }
                client.close()
                continue

            result["steps"]["initialize"] = {
                "ok": True,
                "candidate_index": idx,
                "latency_ms": int((time.monotonic() - t0) * 1000),
                "server_keys": sorted(init_res.keys()) if isinstance(init_res, dict) else [],
            }
            client.notify("initialized", {})

            # 1) account/read (read-only: refreshToken=false, fallback to {} if param rejected)
            t1 = time.monotonic()
            acc_res, acc_err = client.request("account/read", {"refreshToken": False}, timeout=8.0)
            if acc_err and "rpc_error" in acc_err:
                acc_res, acc_err = client.request("account/read", {}, timeout=8.0)
            if acc_err:
                result["steps"]["account/read"] = {"ok": False, "error": acc_err}
            else:
                result["steps"]["account/read"] = {
                    "ok": True,
                    "latency_ms": int((time.monotonic() - t1) * 1000),
                }
                result["account"] = sanitize_account(acc_res)

            # 2) account/rateLimits/read
            t2 = time.monotonic()
            rl_res, rl_err = client.request("account/rateLimits/read", {}, timeout=10.0)
            if rl_err:
                result["steps"]["account/rateLimits/read"] = {"ok": False, "error": rl_err}
                result["fallback_used"] = True
            else:
                result["steps"]["account/rateLimits/read"] = {
                    "ok": True,
                    "latency_ms": int((time.monotonic() - t2) * 1000),
                }
                result["rate_limits"] = sanitize_rate_limits_response(rl_res)

            result["notifications_seen"] = sorted(set(client._notifications))
            client.close()
            return result
        except Exception as exc:  # noqa: BLE001
            last_err = f"candidate[{idx}] exception: {type(exc).__name__}: {exc}"
            client.close()

    result["steps"]["spawn"] = {"ok": False, "error": last_err or "all candidates failed"}
    result["fallback_used"] = True
    return result


def read_latest_log_rate_limit_snapshot(sessions_dir: Path) -> dict[str, Any]:
    if not sessions_dir.exists():
        return {"exists": False}
    latest_time = None
    snapshot = {"exists": True, "rate_limits": None}
    for fpath in sessions_dir.rglob("*.jsonl"):
        try:
            with fpath.open("r", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    if '"rate_limits"' not in line:
                        continue
                    try:
                        obj = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if not isinstance(obj, dict) or obj.get("type") != "event_msg":
                        continue
                    payload = obj.get("payload")
                    if not isinstance(payload, dict) or payload.get("type") != "token_count":
                        continue
                    limits = payload.get("rate_limits")
                    timestamp = obj.get("timestamp")
                    source_time = parse_iso_utc(timestamp) if isinstance(timestamp, str) else None
                    if not isinstance(limits, dict) or source_time is None:
                        continue
                    if latest_time is None or source_time > latest_time:
                        latest_time = source_time
                        snapshot = {
                            "exists": True,
                            "source_timestamp": timestamp,
                            "rate_limits": normalize_rate_limit_bucket(limits),
                        }
        except OSError:
            continue
    return snapshot


def watch_single_request(sessions_dir: Path, timeout_sec: int, codex_bin: str) -> dict[str, Any]:
    """Tail `~/.codex/sessions/**/*.jsonl` incrementally until one new request completes."""
    cursors: dict[Path, int] = {}
    turn_models: dict[str, str] = {}
    seen_dedup_keys: set[tuple[str, str]] = set()

    if sessions_dir.exists():
        for fpath in sessions_dir.rglob("*.jsonl"):
            try:
                cursors[fpath] = fpath.stat().st_size
            except OSError:
                continue

    print(
        f"[watch] Listening to {sessions_dir} (timeout={timeout_sec}s). "
        "Please complete ONE normal Codex request in another terminal...",
        file=sys.stderr,
        flush=True,
    )

    deadline = time.monotonic() + timeout_sec
    raw_token_usage_records = 0
    raw_token_count_events = 0
    duplicates_dropped = 0
    captured_records: list[dict[str, Any]] = []
    latest_rate_limits: dict[str, Any] | None = None
    last_capture_wall: float | None = None
    task_complete_wall: float | None = None

    try:
        while time.monotonic() < deadline:
            if sessions_dir.exists():
                for fpath in sessions_dir.rglob("*.jsonl"):
                    try:
                        size = fpath.stat().st_size
                    except OSError:
                        continue
                    prev = cursors.get(fpath, 0)
                    if size < prev:
                        prev = 0
                    if size == prev:
                        continue
                    try:
                        with fpath.open("r", encoding="utf-8", errors="ignore") as fh:
                            fh.seek(prev)
                            chunk = fh.read()
                            new_pos = fh.tell()
                    except OSError:
                        continue
                    cursors[fpath] = new_pos
                    detected_utc = datetime.now(timezone.utc)

                    for line in chunk.splitlines():
                        line = line.strip()
                        if not line:
                            continue
                        try:
                            obj = json.loads(line)
                        except json.JSONDecodeError:
                            continue
                        if not isinstance(obj, dict):
                            continue
                        etype = obj.get("type")
                        payload = obj.get("payload") if isinstance(obj.get("payload"), dict) else {}

                        if etype == "turn_context":
                            tid = payload.get("turn_id")
                            mdl = payload.get("model")
                            if tid and mdl:
                                turn_models[str(tid)] = str(mdl)

                        elif etype == "token_usage_record":
                            raw_token_usage_records += 1
                            sid = str(payload.get("session_id", "unknown"))
                            rid = str(payload.get("response_id", f"line:{detected_utc.timestamp()}"))
                            tid = str(payload.get("turn_id", ""))
                            dedup_key = (sid, rid)
                            is_duplicate = dedup_key in seen_dedup_keys
                            seen_dedup_keys.add(dedup_key)

                            src_ts_str = obj.get("timestamp")
                            src_dt = parse_iso_utc(src_ts_str) if isinstance(src_ts_str, str) else None
                            latency_ms = (
                                int((detected_utc - src_dt).total_seconds() * 1000)
                                if src_dt
                                else None
                            )
                            usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
                            if is_duplicate:
                                duplicates_dropped += 1
                            else:
                                captured_records.append(
                                    {
                                        "session_id_hash": short_hash(sid),
                                        "turn_id_hash": short_hash(tid) if tid else None,
                                        "response_id_hash": short_hash(rid),
                                        "model": turn_models.get(tid),
                                        "source_timestamp": src_ts_str,
                                        "detected_at_utc": detected_utc.isoformat(),
                                        "capture_latency_ms": latency_ms,
                                        "usage": {
                                            "input_tokens": usage.get("input_tokens"),
                                            "cached_input_tokens": usage.get("cached_input_tokens"),
                                            "output_tokens": usage.get("output_tokens"),
                                            "reasoning_output_tokens": usage.get("reasoning_output_tokens"),
                                            "total_tokens": usage.get("total_tokens"),
                                        },
                                    }
                                )
                                last_capture_wall = time.monotonic()

                        elif etype == "event_msg" and payload.get("type") == "token_count":
                            raw_token_count_events += 1
                            if isinstance(payload.get("rate_limits"), dict):
                                latest_rate_limits = {
                                    "source_timestamp": obj.get("timestamp"),
                                    "rate_limits": normalize_rate_limit_bucket(payload["rate_limits"]),
                                }

                        elif etype == "event_msg" and payload.get("type") == "task_complete":
                            if last_capture_wall is not None:
                                task_complete_wall = time.monotonic()

            now_m = time.monotonic()
            if task_complete_wall is not None and (now_m - task_complete_wall) >= 0.5:
                break
            if last_capture_wall is not None and (now_m - last_capture_wall) >= 3.0:
                break
            time.sleep(0.1)
    except KeyboardInterrupt:
        pass

    post_active = run_active_query(codex_bin) if captured_records else None
    return {
        "captured_new_request": len(captured_records) > 0,
        "raw_token_usage_records_seen": raw_token_usage_records,
        "raw_token_count_events_seen": raw_token_count_events,
        "duplicates_dropped": duplicates_dropped,
        "dedup_counted_records": len(captured_records),
        "single_counted_verified": (
            len(captured_records) > 0
            and len(captured_records) == (raw_token_usage_records - duplicates_dropped)
        ),
        "records": captured_records,
        "log_rate_limits_after_request": latest_rate_limits,
        "active_rpc_after_request": post_active,
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Phase 0b Codex app-server active query & live capture verifier"
    )
    parser.add_argument(
        "--watch",
        action="store_true",
        help="Also watch ~/.codex/sessions for one live request from another terminal",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=180,
        help="Watch timeout in seconds (default: 180)",
    )
    args = parser.parse_args()

    codex_bin = shutil.which("codex") or ""
    sessions_dir = HOME / ".codex" / "sessions"

    report: dict[str, Any] = {
        "active_query": run_active_query(codex_bin),
        "latest_log_snapshot_fallback": read_latest_log_rate_limit_snapshot(sessions_dir),
    }
    if args.watch:
        report["live_capture"] = watch_single_request(sessions_dir, args.timeout, codex_bin)

    print(json.dumps(report, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
