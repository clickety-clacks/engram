#!/bin/bash
set -euo pipefail

WATCH_DIR="$HOME/shared-workspace/shared/openclaw-sessions"
LOG_FILE="$HOME/.engram/openclaw-sessions-watch.log"
DEBOUNCE_SECS="${DEBOUNCE_SECS:-5}"
INGEST_TIMEOUT="${INGEST_TIMEOUT:-120}"

# Ingest specific files by name (cwd must be WATCH_DIR)
ingest_file() {
  local fname="$1"
  cd "$WATCH_DIR" 2>/dev/null || { echo "[$(date)] ingest skipped: watch dir unavailable" >> "$LOG_FILE"; return 0; }
  "$HOME/.local/bin/engram" ingest "$fname" >> "$LOG_FILE" 2>&1 &
  local INGEST_PID=$!
  local ELAPSED=0
  while kill -0 $INGEST_PID 2>/dev/null; do
    sleep 5; ELAPSED=$((ELAPSED+5))
    if [ "$ELAPSED" -ge "$INGEST_TIMEOUT" ]; then
      kill $INGEST_PID 2>/dev/null || true
      echo "[$(date)] ingest timed out after ${INGEST_TIMEOUT}s: $fname" >> "$LOG_FILE"
      return 0
    fi
  done
  wait $INGEST_PID 2>/dev/null \
    && echo "[$(date)] ingest ok: $fname" >> "$LOG_FILE" \
    || echo "[$(date)] ingest failed: $fname" >> "$LOG_FILE"
}

echo "[$(date)] watcher start (fswatch) dir=$WATCH_DIR debounce=${DEBOUNCE_SECS}s timeout=${INGEST_TIMEOUT}s" >> "$LOG_FILE"

/opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed \
  --exclude "\.engram" \
  "$WATCH_DIR" \
| while IFS= read -r -d "" changed_path; do
    fname=$(basename "$changed_path")
    case "$fname" in
      *.jsonl)
        echo "[$(date)] change: $fname" >> "$LOG_FILE"
        sleep "$DEBOUNCE_SECS"
        ingest_file "$fname"
        ;;
    esac
  done
