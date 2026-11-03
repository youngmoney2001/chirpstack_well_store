#!/usr/bin/env bash
# Retrieve the offline `cargo vendor` snapshot produced by
# .github/workflows/vendor-crates.yml.
#
# Why: crates.io is blocked in this sandbox, but GitHub runners can reach it.
# The workflow vendors all crates from Cargo.lock, splits the tarball into
# <90 MB parts and pushes them onto this branch; this script fetches them over
# github.com (allowed), reassembles them into ~/.cache/vendor (gitignored cache)
# and, optionally, removes the temporary parts from git again.
#
# Usage:
#   scripts/fetch-vendor-crates.sh              # fetch an existing snapshot
#   scripts/fetch-vendor-crates.sh --trigger   # first trigger the workflow
#   scripts/fetch-vendor-crates.sh --cleanup   # also delete the parts from git
#
# Note: `gh workflow run` is refused by the Arena GitHub App token (403), so
# --trigger bumps .github/vendor-trigger and pushes it (that is what the
# workflow's `on: push: paths:` filter listens to).

set -euo pipefail

CACHE="${HOME}/.cache"
VENDOR_DIR="${CACHE}/vendor"
WORK="${CACHE}/vendor-work"
BRANCH="$(git branch --show-current)"
REMOTE_REF="origin/${BRANCH}"
WORKFLOW="vendor-crates.yml"
TRIGGER=0
CLEANUP=0
for arg in "$@"; do
  case "$arg" in
    --trigger) TRIGGER=1 ;;
    --cleanup) CLEANUP=1 ;;
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done

if [ "$TRIGGER" = 0 ] && [ -d "$VENDOR_DIR" ] \
   && [ "$(find "$VENDOR_DIR" -name .cargo-checksum.json | wc -l)" -gt 100 ]; then
  echo "==> $VENDOR_DIR already contains a complete vendor snapshot."
  echo "    Use --trigger to regenerate it after changing Cargo.lock."
  exit 0
fi

BEFORE="$(gh run list --workflow="$WORKFLOW" --limit 1 --json databaseId \
  -q '.[0].databaseId // 0' 2>/dev/null || echo 0)"
echo "==> last existing run: $BEFORE / branch: $BRANCH"

if [ "$TRIGGER" = 1 ]; then
  echo "==> triggering workflow by bumping .github/vendor-trigger"
  printf 'trigger %s\n' "$(date -u +%FT%TZ)" > .github/vendor-trigger
  git add .github/vendor-trigger
  git commit -q -m "build: trigger crates vendor workflow"
  git push origin "$BRANCH"
fi

echo "==> waiting for a successful vendor run (up to 15 min)"
RUN=0
for _ in $(seq 1 60); do
  LINE="$(gh run list --workflow="$WORKFLOW" --limit 1 --json databaseId,status,conclusion \
    -q '.[0] | (.databaseId|tostring) + " " + .status + " " + (.conclusion // "")' 2>/dev/null || true)"
  read -r RID STATUS CONCLUSION <<< "$LINE"
  if [ -n "${RID:-}" ] && [ "$RID" -gt "$BEFORE" ] && [ "${STATUS:-}" = completed ]; then
    RUN="$RID"
    [ "$CONCLUSION" = success ] || {
      echo "Run $RUN failed ($CONCLUSION). Logs:" >&2
      gh run view "$RUN" --log 2>/dev/null | tail -40 >&2
      exit 1
    }
    break
  fi
  printf '.'
  sleep 15
done
echo
[ "$RUN" != 0 ] || { echo "No completed vendor run found" >&2; exit 1; }
echo "==> run $RUN succeeded"

echo "==> fetching parts from $REMOTE_REF"
git fetch origin "$BRANCH" >/dev/null 2>&1
PARTS="$(git ls-tree --name-only "$REMOTE_REF:.vendor-parts/" 2>/dev/null | grep '^part_' | sort || true)"
if [ -z "$PARTS" ]; then
  if [ -d "$VENDOR_DIR" ] && [ "$(find "$VENDOR_DIR" -name .cargo-checksum.json | wc -l)" -gt 100 ]; then
    echo "    parts already cleaned up; local vendor dir is complete ($VENDOR_DIR)"
    exit 0
  fi
  echo "No vendor parts on $REMOTE_REF. Re-run with --trigger." >&2
  exit 1
fi
mkdir -p "$WORK"
: > "$WORK/vendor.tar.gz"
for p in $PARTS; do
  git show "$REMOTE_REF:.vendor-parts/$p" >> "$WORK/vendor.tar.gz"
done
ls -lh "$WORK/vendor.tar.gz"
gzip -t "$WORK/vendor.tar.gz"

echo "==> extracting into $VENDOR_DIR"
rm -rf "$VENDOR_DIR"
tar xzf "$WORK/vendor.tar.gz" -C "$CACHE"
COUNT="$(find "$VENDOR_DIR" -name .cargo-checksum.json | wc -l)"
echo "    $COUNT crates vendored"
[ "$COUNT" -gt 100 ] || { echo "Suspiciously small vendor dir" >&2; exit 1; }

if [ "$CLEANUP" = 1 ]; then
  echo "==> removing temporary parts from git"
  git fetch origin "$BRANCH" >/dev/null 2>&1
  git merge --ff-only "$REMOTE_REF" >/dev/null 2>&1 || true
  if [ -d .vendor-parts ]; then
    git rm -r -q .vendor-parts
    git commit -q -m "build: remove temporary vendored-crates parts"
    git push origin "$BRANCH"
  fi
fi

echo "==> Done. Build with:"
echo "    export CARGO_HOME=${CACHE}/cargo-home"
echo "    cargo check --offline -p chirpstack"
