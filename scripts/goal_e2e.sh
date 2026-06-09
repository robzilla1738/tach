#!/usr/bin/env bash
# End-to-end test of the durable goal runtime: scaffold a broken project, run its
# goal until a simulated crash, prove the working tree was NOT touched, resume to
# green, and prove the run replays deterministically. Exit 0 == the whole
# crash/resume/replay story works. Hermetic: no network, no clock, no services.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

echo "## perdure new demo"
"$BIN" new demo >/dev/null
cd demo

echo "## perdure goal run FixFailingTests --crash-after step:2  (expect a durable crash)"
set +e
"$BIN" goal run FixFailingTests --crash-after step:2
CODE=$?
set -e
if [ "$CODE" -ne 99 ]; then
  echo "FAIL: expected crash exit code 99, got $CODE"
  exit 1
fi
echo "   ok — crashed mid-run as expected (exit 99)"

RUN_ID="$("$BIN" goal list | awk '/run_/ {print $1; exit}')"
echo "## run id: $RUN_ID"

echo "## perdure check  (expect STILL RED — a crash must not leave a half-edited tree)"
if "$BIN" check >/dev/null 2>&1; then
  echo "FAIL: working tree went green after a crash — it should be untouched"
  exit 1
fi
echo "   ok — working tree untouched by the crash"

echo "## perdure goal resume $RUN_ID  (expect completion, no repeated work)"
"$BIN" goal resume "$RUN_ID"

echo "## perdure check  (expect GREEN — verified result written back)"
"$BIN" check

echo "## perdure test   (expect all green)"
"$BIN" test

echo "## assert exactly 3 patches were applied across the whole run (no duplication)"
APPLIED="$("$BIN" goal inspect "$RUN_ID" --json | grep -c '"kind": "patch.applied"')"
if [ "$APPLIED" -ne 3 ]; then
  echo "FAIL: expected 3 patch.applied events, found $APPLIED"
  exit 1
fi
echo "   ok — 3 patches applied, none repeated across the crash boundary"

echo "## perdure goal replay $RUN_ID  (expect exact reproduction)"
"$BIN" goal replay "$RUN_ID"

echo
echo "ALL GOOD — crashed, resumed without repeating work, reproduced."
