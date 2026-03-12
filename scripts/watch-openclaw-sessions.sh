#!/bin/bash
set -euo pipefail

WATCH_DIR="$HOME/shared-workspace/shared/openclaw-sessions"
LOG_FILE="$HOME/.engram/openclaw-sessions-watch.log"
DEBOUNCE_SECS="${DEBOUNCE_SECS:-5}"
INGEST_TIMEOUT="${INGEST_TIMEOUT:-120}"

run_ingest() {
  if ! [ -d "$WATCH_DIR" ]; then
    echo "[$(date)] ingest skipped: watch dir unavailable" >> "$LOG_FILE"
    return 0
  fi
  cd "$WATCH_DIR"
  "$HOME/.local/bin/engram" ingest >> "$LOG_FILE" 2>&1 &
  INGEST_PID=$!
  # wait with manual timeout
  ELAPSED=0
  while kill -0 $INGEST_PID 2>/dev/null; do
    sleep 5
    ELAPSED=$((ELAPSED+5))
    if [ "$ELAPSED" -ge "$INGEST_TIMEOUT" ]; then
      kill $INGEST_PID 2>/dev/null || true
      echo "[$(date)] ingest timed out after ${INGEST_TIMEOUT}s" >> "$LOG_FILE"
      return 0
    fi
  done
  wait $INGEST_PID 2>/dev/null \
    && echo "[$(date)] ingest ok" >> "$LOG_FILE" \
    || echo "[$(date)] ingest failed" >> "$LOG_FILE"
}

echo "[$(date)] watcher start (fswatch) dir=$WATCH_DIR debounce=${DEBOUNCE_SECS}s timeout=${INGEST_TIMEOUT}s" >> "$LOG_FILE"

run_ingest

/opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed \
  --exclude "\.engram" \
  "$WATCH_DIR" \
| while IFS= read -r -d "" _path; do
    echo "[$(date)] change: $_path" >> "$LOG_FILE"
    sleep "$DEBOUNCE_SECS"
    run_ingest
  done
