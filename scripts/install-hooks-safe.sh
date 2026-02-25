#!/usr/bin/env bash
set -euo pipefail

mkdir -p .githooks

for h in pre-commit pre-push post-merge; do
  if [ -f ".githooks/$h" ]; then
    echo "[engram] preserve existing .githooks/$h"
  else
    cp "scripts/hooks/$h" ".githooks/$h"
    echo "[engram] installed .githooks/$h"
  fi
done

chmod +x .githooks/pre-commit .githooks/pre-push .githooks/post-merge

git config core.hooksPath .githooks
echo "[engram] core.hooksPath set to .githooks"
