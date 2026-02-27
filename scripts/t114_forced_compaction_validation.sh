#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ROOT="${ROOT}/scratch/t114-forced-compaction"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="${OUT_ROOT}/${RUN_ID}"
REPO_DIR="${RUN_DIR}/repo"
SRC_DIR="${RUN_DIR}/sources"
RESULTS_DIR="${RUN_DIR}/results"
HOME_SANDBOX="${RUN_DIR}/home"
BIN="${ROOT}/target/release/engram"

mkdir -p "${REPO_DIR}/src" "${SRC_DIR}" "${RESULTS_DIR}" "${HOME_SANDBOX}"

if [[ ! -x "${BIN}" ]]; then
  cargo build --release --manifest-path "${ROOT}/Cargo.toml" >/dev/null
fi

run_with_capture() {
  local name="$1"
  shift
  local stdout_file="${RESULTS_DIR}/${name}.stdout"
  local stderr_file="${RESULTS_DIR}/${name}.stderr"
  local exit_file="${RESULTS_DIR}/${name}.exit"

  set +e
  (
    cd "${REPO_DIR}"
    HOME="${HOME_SANDBOX}" "${BIN}" "$@" >"${stdout_file}" 2>"${stderr_file}"
  )
  local rc=$?
  set -e

  printf '%s\n' "${rc}" > "${exit_file}"
  if [[ "${rc}" -ne 0 ]]; then
    echo "command ${name} failed with exit=${rc}" >&2
    echo "stderr (${stderr_file}):" >&2
    sed -n '1,120p' "${stderr_file}" >&2 || true
    return "${rc}"
  fi
}

cat > "${REPO_DIR}/src/engine.rs" <<'EOF'
pub fn helper() {}
pub fn continuation_probe() -> &'static str { "T114_FORCED_COMPACTION_PROBE" }
pub fn tail() {}
EOF

SPAN_TEXT='pub fn continuation_probe() -> &'"'"'static str { "T114_FORCED_COMPACTION_PROBE" }'
SPAN_SHA="$(printf '%s' "${SPAN_TEXT}" | shasum -a 256 | awk '{print $1}')"

cat > "${SRC_DIR}/oc_t114_1.jsonl" <<EOF
{"type":"session","id":"oc-t114-1","timestamp":"2026-02-27T10:00:00Z"}
{"type":"message","id":"m1","timestamp":"2026-02-27T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"T114_MARKER_RUN_START session=1"}]}}
{"type":"message","id":"m2","timestamp":"2026-02-27T10:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Working on continuation probe before forced /compact boundary B1."},{"type":"toolCall","id":"call_s1_w","name":"Apply","arguments":{"file":"src/engine.rs","before_hash":"seed-s1","after_hash":"${SPAN_SHA}"}}]}}
{"type":"message","id":"m3","timestamp":"2026-02-27T10:00:03Z","message":{"role":"toolResult","toolCallId":"call_s1_w","toolName":"Apply","content":[{"type":"text","text":"updated src/engine.rs at probe line"}],"isError":false}}
{"type":"message","id":"m4","timestamp":"2026-02-27T10:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"FORCED_COMPACT_BOUNDARY_B1 summary: preserved probe intent and state."}]}}
EOF

cat > "${SRC_DIR}/oc_t114_2.jsonl" <<EOF
{"type":"session","id":"oc-t114-2","timestamp":"2026-02-27T10:05:00Z"}
{"type":"message","id":"m1","timestamp":"2026-02-27T10:05:01Z","message":{"role":"user","content":[{"type":"text","text":"T114_MARKER_RESUME_AFTER_B1 predecessor=oc-t114-1"}]}}
{"type":"message","id":"m2","timestamp":"2026-02-27T10:05:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Compaction carry-forward from B1: preserve T114_FORCED_COMPACTION_PROBE semantics."},{"type":"toolCall","id":"call_s2_w","name":"Apply","arguments":{"file":"src/engine.rs","before_hash":"seed-s2","after_hash":"${SPAN_SHA}"}}]}}
{"type":"message","id":"m3","timestamp":"2026-02-27T10:05:03Z","message":{"role":"toolResult","toolCallId":"call_s2_w","toolName":"Apply","content":[{"type":"text","text":"updated src/engine.rs after B1"}],"isError":false}}
{"type":"message","id":"m4","timestamp":"2026-02-27T10:05:04Z","message":{"role":"assistant","content":[{"type":"text","text":"FORCED_COMPACT_BOUNDARY_B2 summary: carry-forward validated."}]}}
EOF

cat > "${SRC_DIR}/oc_t114_3.jsonl" <<EOF
{"type":"session","id":"oc-t114-3","timestamp":"2026-02-27T10:10:00Z"}
{"type":"message","id":"m1","timestamp":"2026-02-27T10:10:01Z","message":{"role":"user","content":[{"type":"text","text":"T114_MARKER_RESUME_AFTER_B2 predecessor=oc-t114-2"}]}}
{"type":"message","id":"m2","timestamp":"2026-02-27T10:10:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Compaction carry-forward from B2: finalizing probe edits."},{"type":"toolCall","id":"call_s3_w","name":"Apply","arguments":{"file":"src/engine.rs","before_hash":"seed-s3","after_hash":"${SPAN_SHA}"}}]}}
{"type":"message","id":"m3","timestamp":"2026-02-27T10:10:03Z","message":{"role":"toolResult","toolCallId":"call_s3_w","toolName":"Apply","content":[{"type":"text","text":"updated src/engine.rs after B2"}],"isError":false}}
EOF

run_with_capture init init
cat > "${REPO_DIR}/.engram/config.yml" <<EOF
sources:
  - path: ${SRC_DIR}/*.jsonl
    adapter: openclaw
exclude: []
EOF
run_with_capture ingest ingest
run_with_capture explain explain src/engine.rs:2-2

cp "${RESULTS_DIR}/init.stdout" "${RESULTS_DIR}/init.json"
cp "${RESULTS_DIR}/ingest.stdout" "${RESULTS_DIR}/ingest.json"
cp "${RESULTS_DIR}/explain.stdout" "${RESULTS_DIR}/explain.json"

jq -r '.sessions[].tape_id' "${RESULTS_DIR}/explain.json" > "${RESULTS_DIR}/explain_tape_ids.txt"
> "${RESULTS_DIR}/observed_session_order.txt"
while IFS= read -r tape_id; do
  sid="$(
    cd "${REPO_DIR}" && HOME="${HOME_SANDBOX}" "${BIN}" show "${tape_id}" --raw \
      | head -n1 \
      | jq -r '.source.session_id // empty'
  )"
  if [[ -n "${sid}" ]]; then
    printf '%s\n' "${sid}" >> "${RESULTS_DIR}/observed_session_order.txt"
  fi
done < "${RESULTS_DIR}/explain_tape_ids.txt"

jq -r '.sessions | sort_by(.latest_touch_timestamp) | .[].tape_id' "${RESULTS_DIR}/explain.json" \
  > "${RESULTS_DIR}/chronological_tape_ids.txt"
> "${RESULTS_DIR}/chronological_session_ids.txt"
while IFS= read -r tape_id; do
  sid="$(
    cd "${REPO_DIR}" && HOME="${HOME_SANDBOX}" "${BIN}" show "${tape_id}" --raw \
      | head -n1 \
      | jq -r '.source.session_id // empty'
  )"
  if [[ -n "${sid}" ]]; then
    printf '%s\n' "${sid}" >> "${RESULTS_DIR}/chronological_session_ids.txt"
  fi
done < "${RESULTS_DIR}/chronological_tape_ids.txt"

cat > "${RESULTS_DIR}/ground_truth_edges.txt" <<'EOF'
oc-t114-1->oc-t114-2
oc-t114-2->oc-t114-3
EOF

awk '
  NR==1 {prev=$0; next}
  {print prev "->" $0; prev=$0}
' "${RESULTS_DIR}/chronological_session_ids.txt" > "${RESULTS_DIR}/predicted_edges.txt"

tp=0
fp=0
fn=0

while IFS= read -r edge; do
  if [[ -z "${edge}" ]]; then
    continue
  fi
  if grep -Fxq "${edge}" "${RESULTS_DIR}/ground_truth_edges.txt"; then
    tp=$((tp + 1))
  else
    fp=$((fp + 1))
  fi
done < "${RESULTS_DIR}/predicted_edges.txt"

while IFS= read -r edge; do
  if [[ -z "${edge}" ]]; then
    continue
  fi
  if ! grep -Fxq "${edge}" "${RESULTS_DIR}/predicted_edges.txt"; then
    fn=$((fn + 1))
  fi
done < "${RESULTS_DIR}/ground_truth_edges.txt"

pred_total=$((tp + fp))
truth_total=$((tp + fn))

precision="0.000"
recall="0.000"
f1="0.000"
if [[ ${pred_total} -gt 0 ]]; then
  precision="$(awk -v a="${tp}" -v b="${pred_total}" 'BEGIN {printf "%.3f", a / b}')"
fi
if [[ ${truth_total} -gt 0 ]]; then
  recall="$(awk -v a="${tp}" -v b="${truth_total}" 'BEGIN {printf "%.3f", a / b}')"
fi
if [[ "${precision}" != "0.000" || "${recall}" != "0.000" ]]; then
  f1="$(awk -v p="${precision}" -v r="${recall}" 'BEGIN {printf "%.3f", (2*p*r)/(p+r)}')"
fi

cat > "${RESULTS_DIR}/metrics.json" <<EOF
{
  "tp": ${tp},
  "fp": ${fp},
  "fn": ${fn},
  "precision": ${precision},
  "recall": ${recall},
  "f1": ${f1},
  "ground_truth_edge_count": 2,
  "predicted_edge_count": ${pred_total},
  "run_dir": "${RUN_DIR}",
  "probe_anchor_sha256": "${SPAN_SHA}"
}
EOF

cat > "${RESULTS_DIR}/summary.txt" <<EOF
run_dir=${RUN_DIR}
probe_anchor_sha256=${SPAN_SHA}
init_exit=$(cat "${RESULTS_DIR}/init.exit")
ingest_exit=$(cat "${RESULTS_DIR}/ingest.exit")
explain_exit=$(cat "${RESULTS_DIR}/explain.exit")
ground_truth_edges=$(tr '\n' ',' < "${RESULTS_DIR}/ground_truth_edges.txt" | sed 's/,$//')
predicted_edges=$(tr '\n' ',' < "${RESULTS_DIR}/predicted_edges.txt" | sed 's/,$//')
precision=${precision}
recall=${recall}
f1=${f1}
EOF

echo "T114 forced-compaction validation artifacts written to:"
echo "  ${RUN_DIR}"
echo "Key outputs:"
echo "  ${RESULTS_DIR}/ingest.json"
echo "  ${RESULTS_DIR}/explain.json"
echo "  ${RESULTS_DIR}/metrics.json"
echo "  ${RESULTS_DIR}/ingest.stderr"
echo "  ${RESULTS_DIR}/explain.stderr"
cat "${RESULTS_DIR}/summary.txt"
