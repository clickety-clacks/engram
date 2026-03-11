#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "[engram] running real harness discovery/import validation"
cargo test --test real_harness_validation -- --nocapture
echo "[engram] real harness discovery/import validation: PASS"
