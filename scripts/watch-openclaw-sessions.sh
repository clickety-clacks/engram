#!/bin/bash
set -euo pipefail

WATCH_DIR="$HOME/shared-workspace/shared/openclaw-sessions"
GEMINI_WATCH_DIR="$HOME/.gemini/tmp"
CLAUDE_WATCH_DIR="$HOME/.claude/projects"
CODEX_WATCH_DIR="$HOME/.codex"
LOG_FILE="$HOME/.engram/openclaw-sessions-watch.log"
DEBOUNCE_SECS="${DEBOUNCE_SECS:-5}"
INGEST_TIMEOUT="${INGEST_TIMEOUT:-120}"

run_ingest() {
  local workdir="$1"
  local target="$2"
  local label="$3"
  cd "$workdir" 2>/dev/null || { echo "[$(date)] ingest skipped ($label): workdir unavailable: $workdir" >> "$LOG_FILE"; return 0; }
  "$HOME/.local/bin/engram" ingest "$target" >> "$LOG_FILE" 2>&1 &
  local INGEST_PID=$!
  local ELAPSED=0
  while kill -0 $INGEST_PID 2>/dev/null; do
    sleep 5; ELAPSED=$((ELAPSED+5))
    if [ "$ELAPSED" -ge "$INGEST_TIMEOUT" ]; then
      kill $INGEST_PID 2>/dev/null || true
      echo "[$(date)] ingest timed out after ${INGEST_TIMEOUT}s ($label): $target" >> "$LOG_FILE"
      return 0
    fi
  done
  wait $INGEST_PID 2>/dev/null \
    && echo "[$(date)] ingest ok ($label): $target" >> "$LOG_FILE" \
    || echo "[$(date)] ingest failed ($label): $target" >> "$LOG_FILE"
}

# Ingest specific OpenClaw file names (cwd must be WATCH_DIR)
ingest_file() {
  local fname="$1"
  run_ingest "$WATCH_DIR" "$fname" "openclaw"
}

# Ingest specific Gemini session files (cwd set to HOME, target is absolute path)
ingest_gemini_file() {
  local abs_path="$1"
  run_ingest "$HOME" "$abs_path" "gemini"
}

start_openclaw_watch() {
  /opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed \
    --exclude "\.engram" \
    "$WATCH_DIR" \
  | while IFS= read -r -d "" changed_path; do
      fname=$(basename "$changed_path")
      case "$fname" in
        *.jsonl)
          echo "[$(date)] openclaw change: $fname" >> "$LOG_FILE"
          sleep "$DEBOUNCE_SECS"
          ingest_file "$fname"
          ;;
      esac
    done
}

start_gemini_watch() {
  if [ ! -d "$GEMINI_WATCH_DIR" ]; then
    echo "[$(date)] gemini watch skipped: $GEMINI_WATCH_DIR missing" >> "$LOG_FILE"
    return 0
  fi

  /opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed \
    "$GEMINI_WATCH_DIR" \
  | while IFS= read -r -d "" changed_path; do
      case "$changed_path" in
        */chats/session-*.json)
          echo "[$(date)] gemini change: $changed_path" >> "$LOG_FILE"
          sleep "$DEBOUNCE_SECS"
          ingest_gemini_file "$changed_path"
          ;;
      esac
    done
}


start_claude_watch() {
  if [ ! -d "$CLAUDE_WATCH_DIR" ]; then
    echo "$(date) claude watch skipped: $CLAUDE_WATCH_DIR missing" >> "$LOG_FILE"
    return 0
  fi

  /opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed     "$CLAUDE_WATCH_DIR"   | while IFS= read -r -d "" changed_path; do
      case "$changed_path" in
        *.jsonl)
          echo "$(date) claude change: $changed_path" >> "$LOG_FILE"
          sleep "$DEBOUNCE_SECS"
          run_ingest "$HOME" "$changed_path" "claude"
          ;;
      esac
    done
}

start_codex_watch() {
  if [ ! -d "$CODEX_WATCH_DIR" ]; then
    echo "[$(date)] codex watch skipped: $CODEX_WATCH_DIR missing" >> "$LOG_FILE"
    return 0
  fi

  /opt/homebrew/bin/fswatch -0 -r --event Created --event Updated --event Renamed     "$CODEX_WATCH_DIR"   | while IFS= read -r -d "" changed_path; do
      case "$changed_path" in
        *.jsonl)
          echo "[$(date)] codex change: $changed_path" >> "$LOG_FILE"
          sleep "$DEBOUNCE_SECS"
          run_ingest "$HOME" "$changed_path" "codex"
          ;;
      esac
    done
}
WATCH_PIDS=()
cleanup() {
  for pid in "${WATCH_PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait "${WATCH_PIDS[@]:-}" 2>/dev/null || true
}

trap cleanup INT TERM EXIT

echo "[$(date)] watcher start (fswatch) openclaw_dir=$WATCH_DIR gemini_dir=$GEMINI_WATCH_DIR debounce=${DEBOUNCE_SECS}s timeout=${INGEST_TIMEOUT}s" >> "$LOG_FILE"

start_openclaw_watch &
WATCH_PIDS+=($!)

if [ -d "$GEMINI_WATCH_DIR" ]; then
  start_gemini_watch &
  WATCH_PIDS+=($!)
else
  echo "[$(date)] gemini watch skipped: $GEMINI_WATCH_DIR missing" >> "$LOG_FILE"
fi

if [ -d "$CLAUDE_WATCH_DIR" ]; then
  start_claude_watch &
  WATCH_PIDS+=($!)
else
  echo "$(date) claude watch skipped: $CLAUDE_WATCH_DIR missing" >> "$LOG_FILE"
fi

if [ -d "$CODEX_WATCH_DIR" ]; then
  start_codex_watch &
  WATCH_PIDS+=($!)
else
  echo "[$(date)] codex watch skipped: $CODEX_WATCH_DIR missing" >> "$LOG_FILE"
fi

wait "${WATCH_PIDS[@]}"
