#!/usr/bin/env bash
# End-to-end test of the action layer: run a built-in business goal that pauses for
# human approval, approve it, crash mid-flight right after the refund is receipted,
# resume to completion, and prove the refund happened EXACTLY ONCE with a durable
# receipt and a deterministic replay. Exit 0 == the approval/refund/receipt story
# works. Hermetic: the "tools" are offline fakes — no network, no clock, no services.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/tach"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

status_of() {
  "$BIN" goal inspect "$1" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["state"]["status"])'
}

echo "## tach goal run ResolveDuplicateCharge  (expect a pause at the refund gate)"
"$BIN" goal run ResolveDuplicateCharge >/dev/null
RUN_ID="$("$BIN" goal list | awk '/run_/ {print $1; exit}')"
echo "## run id: $RUN_ID"

if [ "$(status_of "$RUN_ID")" != "awaiting_approval" ]; then
  echo "FAIL: expected awaiting_approval, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — paused awaiting approval"

APR="$("$BIN" goal approvals "$RUN_ID" | grep -oE 'apr_[a-f0-9]+' | head -1)"
echo "## tach goal approve $RUN_ID $APR"
"$BIN" goal approve "$RUN_ID" "$APR" >/dev/null

echo "## tach goal resume $RUN_ID --crash-after step:4  (expect a durable crash)"
set +e
"$BIN" goal resume "$RUN_ID" --crash-after step:4 >/dev/null
CODE=$?
set -e
if [ "$CODE" -ne 99 ]; then
  echo "FAIL: expected crash exit 99, got $CODE"
  exit 1
fi
echo "   ok — crashed after the refund, state durable"

echo "## tach goal resume $RUN_ID  (expect completion)"
"$BIN" goal resume "$RUN_ID" >/dev/null
if [ "$(status_of "$RUN_ID")" != "completed" ]; then
  echo "FAIL: expected completed, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — completed"

echo "## assert fake.stripe.refund was called EXACTLY ONCE across the whole run"
CALLS="$("$BIN" goal inspect "$RUN_ID" --json | python3 -c '
import sys, json
evs = json.load(sys.stdin)["events"]
print(sum(1 for e in evs
          if e["kind"] == "tool.called"
          and e["payload"].get("tool") == "fake.stripe.refund"))')"
if [ "$CALLS" -ne 1 ]; then
  echo "FAIL: expected 1 refund tool.called, found $CALLS"
  exit 1
fi
echo "   ok — refund invoked exactly once (no duplicate across the crash)"

echo "## assert exactly one fake.stripe.refund receipt exists"
REFUNDS="$("$BIN" goal receipts "$RUN_ID" | grep -c 'fake.stripe.refund')"
if [ "$REFUNDS" -ne 1 ]; then
  echo "FAIL: expected 1 refund receipt, found $REFUNDS"
  exit 1
fi
echo "   ok — exactly one durable refund receipt"

echo "## tach goal replay $RUN_ID  (expect exact reproduction)"
"$BIN" goal replay "$RUN_ID"

echo
echo "ALL GOOD — paused, approved, crashed, resumed, refunded exactly once, reproduced."
