#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

revision=$(git rev-parse --verify HEAD)
candidate=9f8afc65d0b365444446a473c04389adf80bd4b3
test -z "$(git status --porcelain)"
git merge-base --is-ancestor "$candidate" "$revision"

export SOURCE_DATE_EPOCH=1789853328
export TZ=UTC
export LC_ALL=C
export T1772_BUILD_REVISION="$revision"

cargo build --locked --release \
  --bin engram \
  --bin t1772_p0_runner \
  --bin t1772_p0_controller

target=$(rustc -vV | awk '/^host:/ {print $2}')
printf 'target=%s\n' "$target"
rustc -vV
cargo -V
git show -s --format=fuller "$revision"
git diff-tree --no-commit-id --name-status -r "$revision"

if command -v sha256sum >/dev/null 2>&1; then
  sha256sum target/release/engram target/release/t1772_p0_runner target/release/t1772_p0_controller
else
  shasum -a 256 target/release/engram target/release/t1772_p0_runner target/release/t1772_p0_controller
fi
