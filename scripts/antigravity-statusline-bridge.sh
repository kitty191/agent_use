#!/usr/bin/env sh
# Bridge Antigravity CLI statusLine JSON stdin to Agent Use without leaving
# temporary files behind or disrupting your existing CLI status bar.
#
# Usage in ~/.gemini/antigravity-cli/settings.json:
# 1) Keep built-in CLI status line:
#    {
#      "statusLine": {
#        "type": "command",
#        "command": "'/Applications/Agent Use.app/Contents/Resources/antigravity-statusline-bridge.sh'",
#        "stack_with_default": true
#      }
#    }
# 2) Preserve an existing custom statusLine script:
#    {
#      "statusLine": {
#        "type": "command",
#        "command": "'/Applications/Agent Use.app/Contents/Resources/antigravity-statusline-bridge.sh' ~/.gemini/antigravity-cli/statusline.sh"
#      }
#    }

set -u

DEFAULT_DIR="$HOME/Library/Application Support/dev.agentuse.desktop"
if [ -n "${AGENT_USE_ANTIGRAVITY_STATUSLINE_PATH:-}" ]; then
  TARGET="$AGENT_USE_ANTIGRAVITY_STATUSLINE_PATH"
elif [ -n "${AGENT_USE_DATA_DIR:-}" ]; then
  TARGET="${AGENT_USE_DATA_DIR%/}/antigravity_statusline.json"
else
  TARGET="$DEFAULT_DIR/antigravity_statusline.json"
fi

TMP_IN=""
TMP_OUT=""
cleanup() {
  [ -n "$TMP_IN" ] && rm -f "$TMP_IN" 2>/dev/null || true
  [ -n "$TMP_OUT" ] && rm -f "$TMP_OUT" 2>/dev/null || true
}
trap cleanup EXIT INT TERM HUP

TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"
TMP_IN="$(mktemp "${TMP_BASE}/agy_statusline_in.XXXXXX" 2>/dev/null)" || TMP_IN=""
if [ -z "$TMP_IN" ]; then
  # Cannot stage temp file; still forward stdin directly to upstream command if configured.
  if [ "$#" -gt 0 ]; then
    "$@"
    exit $?
  elif [ -n "${AGENT_USE_UPSTREAM_STATUSLINE:-}" ]; then
    sh -c "$AGENT_USE_UPSTREAM_STATUSLINE"
    exit $?
  fi
  exit 0
fi

cat > "$TMP_IN" 2>/dev/null || true

# Best-effort atomic snapshot write; failures never block or alter the upstream status line.
if [ -s "$TMP_IN" ]; then
  TARGET_DIR="$(dirname "$TARGET")"
  if mkdir -p "$TARGET_DIR" 2>/dev/null; then
    if TMP_OUT="$(mktemp "$TARGET_DIR/.antigravity_statusline.XXXXXX" 2>/dev/null)"; then
      if cp "$TMP_IN" "$TMP_OUT" 2>/dev/null && mv -f "$TMP_OUT" "$TARGET" 2>/dev/null; then
        TMP_OUT=""
      else
        rm -f "$TMP_OUT" 2>/dev/null || true
        TMP_OUT=""
      fi
    fi
  fi
fi

rc=0
if [ "$#" -gt 0 ]; then
  "$@" < "$TMP_IN"
  rc=$?
elif [ -n "${AGENT_USE_UPSTREAM_STATUSLINE:-}" ]; then
  sh -c "$AGENT_USE_UPSTREAM_STATUSLINE" < "$TMP_IN"
  rc=$?
fi

exit "$rc"
