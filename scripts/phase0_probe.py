#!/usr/bin/env python3
"""
Phase 0 Read-Only Sanitized Probe for macOS (Codex / Claude Code / Antigravity).

Safety guarantees:
- Pure Python 3 stdlib, 100% read-only, zero network calls.
- Never reads credential/auth/cookie files or process command-line arguments.
- Never outputs prompts, assistant replies, tool inputs/outputs, file paths, or emails.
- Only extracts structural keys, numeric token/rate-limit fields, timestamps, and hashed IDs.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import plistlib
import shutil
import subprocess
from pathlib import Path
from typing import Any

HOME = Path.home()

SAFE_SCALAR_KEYS = {
    "type",
    "subtype",
    "role",
    "model",
    "stop_reason",
    "service_tier",
    "plan_type",
    "timestamp",
    "created_at",
    "resets_at",
    "resets_in_seconds",
    "window_minutes",
    "used_percent",
    "remaining_percent",
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "ephemeral_5m_input_tokens",
    "ephemeral_1h_input_tokens",
    "has_credits",
    "unlimited",
    "balance",
    "limit_id",
    "limit_name",
    "isSidechain",
    "version",
}

HASH_ID_KEYS = {
    "id",
    "uuid",
    "parentUuid",
    "sessionId",
    "session_id",
    "requestId",
    "request_id",
    "turn_id",
    "response_id",
}

SAFE_CONTAINER_KEYS = {
    "payload",
    "info",
    "total_token_usage",
    "last_token_usage",
    "rate_limits",
    "primary",
    "secondary",
    "credits",
    "message",
    "usage",
    "cache_creation",
    "metadata",
}


def short_hash(value: Any) -> str:
    digest = hashlib.sha256(str(value).encode("utf-8", errors="ignore")).hexdigest()
    return f"hash:{digest[:10]}"


def sanitize_obj(obj: Any, depth: int = 0) -> Any:
    if depth > 6:
        return "<MAX_DEPTH>"
    if isinstance(obj, dict):
        out: dict[str, Any] = {}
        for k, v in obj.items():
            if k in HASH_ID_KEYS and v is not None:
                out[k] = short_hash(v)
            elif k in SAFE_SCALAR_KEYS:
                if isinstance(v, (int, float, bool)) or v is None:
                    out[k] = v
                elif isinstance(v, str) and len(v) <= 80 and "@" not in v and "/" not in v:
                    out[k] = v
                else:
                    out[k] = "<REDACTED_STR>"
            elif k in SAFE_CONTAINER_KEYS and isinstance(v, dict):
                cleaned = sanitize_obj(v, depth + 1)
                if cleaned:
                    out[k] = cleaned
        return out
    return None


def run_safe_cmd(args: list[str], timeout: int = 5) -> dict[str, Any]:
    binary = shutil.which(args[0])
    if not binary:
        return {"installed": False}
    try:
        proc = subprocess.run(
            args,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        stdout = (proc.stdout or "").strip()
        stderr = (proc.stderr or "").strip()
        return {
            "installed": True,
            "exit_code": proc.returncode,
            "stdout_preview": "\n".join(stdout.splitlines()[:40]),
            "stderr_preview": "\n".join(stderr.splitlines()[:10]),
        }
    except Exception as exc:  # noqa: BLE001
        return {"installed": True, "error": type(exc).__name__}


def inspect_jsonl_dir(base_dir: Path, max_files: int = 2, max_events_per_file: int = 25) -> dict[str, Any]:
    if not base_dir.exists():
        return {"exists": False}
    files = sorted(
        base_dir.rglob("*.jsonl"),
        key=lambda p: p.stat().st_mtime if p.exists() else 0,
        reverse=True,
    )
    total_size = sum(p.stat().st_size for p in files if p.exists())
    samples = []
    for idx, fpath in enumerate(files[:max_files]):
        event_types: dict[str, int] = {}
        head_events: list[dict[str, Any]] = []
        tail_events: list[dict[str, Any]] = []
        head_limit = 5
        tail_limit = max(1, max_events_per_file - head_limit)
        line_count = 0
        try:
            with fpath.open("r", encoding="utf-8", errors="ignore") as fh:
                for line in fh:
                    line_count += 1
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        raw = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if not isinstance(raw, dict):
                        continue
                    top_type = str(raw.get("type", "unknown"))
                    sub_type = ""
                    if isinstance(raw.get("payload"), dict) and "type" in raw["payload"]:
                        sub_type = f":{raw['payload']['type']}"
                    key = f"{top_type}{sub_type}"
                    event_types[key] = event_types.get(key, 0) + 1

                    # Keep usage / rate-limit / assistant / token events only
                    is_interesting = (
                        "token" in key.lower()
                        or "usage" in key.lower()
                        or "rate" in key.lower()
                        or top_type in {"assistant", "turn_context", "session_meta", "event_msg"}
                        or (isinstance(raw.get("message"), dict) and "usage" in raw["message"])
                    )
                    if is_interesting:
                        cleaned = sanitize_obj(raw)
                        if cleaned:
                            cleaned["_line"] = line_count
                            if len(head_events) < head_limit:
                                head_events.append(cleaned)
                            else:
                                tail_events.append(cleaned)
                                if len(tail_events) > tail_limit:
                                    tail_events.pop(0)
        except OSError as exc:
            samples.append({"file_index": idx, "error": type(exc).__name__})
            continue
        samples.append(
            {
                "file_index": idx,
                "size_bytes": fpath.stat().st_size,
                "line_count": line_count,
                "event_type_counts": event_types,
                "sanitized_sample_events": head_events + tail_events,
            }
        )
    return {
        "exists": True,
        "jsonl_file_count": len(files),
        "total_size_bytes": total_size,
        "latest_samples": samples,
    }


def probe_mac_app_version(app_path: Path) -> dict[str, Any]:
    plist_path = app_path / "Contents" / "Info.plist"
    if not plist_path.exists():
        return {"installed": False}
    try:
        with plist_path.open("rb") as fh:
            data = plistlib.load(fh)
        return {
            "installed": True,
            "short_version": data.get("CFBundleShortVersionString"),
            "bundle_version": data.get("CFBundleVersion"),
            "bundle_id": data.get("CFBundleIdentifier"),
        }
    except Exception as exc:  # noqa: BLE001
        return {"installed": True, "error": type(exc).__name__}


def probe_running_process_names(keywords: tuple[str, ...]) -> list[str]:
    """Reads process executable names ONLY (never command-line arguments)."""
    try:
        proc = subprocess.run(
            ["ps", "-axo", "comm="],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        matched = sorted(
            {
                Path(line.strip()).name
                for line in (proc.stdout or "").splitlines()
                if any(kw in line.lower() for kw in keywords)
            }
        )
        return matched
    except Exception:  # noqa: BLE001
        return []


def list_shallow_entries(path: Path, limit: int = 30) -> dict[str, Any]:
    if not path.exists():
        return {"exists": False}
    try:
        entries = sorted(p.name for p in path.iterdir())[:limit]
        return {"exists": True, "top_level_entries": entries}
    except OSError as exc:
        return {"exists": True, "error": type(exc).__name__}


def main() -> None:
    report = {
        "system": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "mac_ver": platform.mac_ver()[0],
            "python": platform.python_version(),
        },
        "codex": {
            "version": run_safe_cmd(["codex", "--version"]),
            "help": run_safe_cmd(["codex", "--help"]),
            "root_dir": list_shallow_entries(HOME / ".codex"),
            "sessions": inspect_jsonl_dir(HOME / ".codex" / "sessions"),
        },
        "claude_code": {
            "version": run_safe_cmd(["claude", "--version"]),
            "help": run_safe_cmd(["claude", "--help"]),
            "root_dir": list_shallow_entries(HOME / ".claude"),
            "projects": inspect_jsonl_dir(HOME / ".claude" / "projects"),
        },
        "antigravity": {
            "agy_version": run_safe_cmd(["agy", "--version"]),
            "agy_help": run_safe_cmd(["agy", "--help"]),
            "antigravity_cli_version": run_safe_cmd(["antigravity", "--version"]),
            "app_bundle": probe_mac_app_version(Path("/Applications/Antigravity.app")),
            "gemini_dir": list_shallow_entries(HOME / ".gemini"),
            "app_support_dir": list_shallow_entries(
                HOME / "Library" / "Application Support" / "Antigravity"
            ),
            "running_process_names": probe_running_process_names(
                ("antigravity", "language_server", "agy")
            ),
        },
    }
    print(json.dumps(report, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
