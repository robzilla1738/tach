#!/usr/bin/env bash
# End-to-end test of USER-AUTHORED plan goals: a plan goal a user writes in their
# own workspace (no built-in Rust catalog entry) must run, pause for approval,
# survive a mid-loop crash with exactly-once refunds, replay deterministically —
# and, critically, RESUME OFF THE SOURCE SNAPSHOT taken at run start, so editing
# the live goal.pdr after the run begins cannot change the in-flight run.
#
# Flow: scaffold `perdure new demo --goal chargebacks` (writes a ReconcileLocalDemo
# goal.pdr), check it, run to the first gate, approve, crash right after refund #1,
# then EDIT the live goal.pdr, resume, and prove the run ignored the edit; approve
# the second gate, finish, replay. Finally prove ReconcileLocalDemo is not built-in.
# Hermetic: the "tools" are offline fakes — no network, no clock, no services.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

status_of() {
  "$BIN" goal inspect "$1" --json | python3 -c 'import sys,json; print(json.load(sys.stdin)["state"]["status"])'
}
pending_apr() {
  "$BIN" goal approvals "$1" | grep pending | grep -oE 'apr_[a-f0-9]+' | head -1
}
refund_receipts() {
  "$BIN" goal receipts "$1" | grep -c 'fake.stripe.refund' || true
}

echo "## perdure new demo --goal chargebacks  (scaffold a workspace-authored plan goal)"
"$BIN" new demo --goal chargebacks >/dev/null
cd demo
if ! grep -q 'goal ReconcileLocalDemo' goal.pdr; then
  echo "FAIL: scaffold did not write a ReconcileLocalDemo goal"
  exit 1
fi
echo "   ok — goal.pdr written"

echo "## perdure fmt --check goal.pdr  (the scaffolded plan goal is already canonical)"
"$BIN" fmt --check goal.pdr >/dev/null
echo "   ok — fmt is a no-op (formatter renders the whole plan block)"

echo "## perdure goal check ReconcileLocalDemo  (static plan validation passes)"
"$BIN" goal check ReconcileLocalDemo >/dev/null
echo "   ok — plan checks out"

echo "## perdure goal run ReconcileLocalDemo  (expect a pause at the first duplicate's gate)"
"$BIN" goal run ReconcileLocalDemo >/dev/null
RUN_ID="$("$BIN" goal list | awk '/run_/ {print $1; exit}')"
echo "## run id: $RUN_ID"
if [ "$(status_of "$RUN_ID")" != "awaiting_approval" ]; then
  echo "FAIL: expected awaiting_approval, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — paused at the first refund gate"

APR1="$(pending_apr "$RUN_ID")"
echo "## approve the first refund ($APR1), then resume --crash-after step:1 (crash right after the refund)"
"$BIN" goal approve "$RUN_ID" "$APR1" >/dev/null
set +e
"$BIN" goal resume "$RUN_ID" --crash-after step:1 >/dev/null
CODE=$?
set -e
if [ "$CODE" -ne 99 ]; then
  echo "FAIL: expected crash exit 99, got $CODE"
  exit 1
fi
if [ "$(refund_receipts "$RUN_ID")" -ne 1 ]; then
  echo "FAIL: expected 1 durable refund receipt after the crash, found $(refund_receipts "$RUN_ID")"
  exit 1
fi
echo "   ok — crashed mid-loop with the first refund durable"

echo "## EDIT the live goal.pdr (change the customer + email), then resume — the run must IGNORE the edit"
python3 - <<'PY'
p = "goal.pdr"
s = open(p).read()
s = s.replace('cus_42', 'cus_EDITED').replace('billing@acme.test', 'edited@acme.test')
open(p, 'w').write(s)
PY
if ! grep -q 'cus_EDITED' goal.pdr; then
  echo "FAIL: the live edit did not take"
  exit 1
fi
"$BIN" goal resume "$RUN_ID" >/dev/null
if [ "$(status_of "$RUN_ID")" != "awaiting_approval" ]; then
  echo "FAIL: expected awaiting_approval at the second gate, got $(status_of "$RUN_ID")"
  exit 1
fi
# The smoking gun: if resume had re-read the live file, the run would now carry
# cus_EDITED (a different list_disputes input → different charges → new refunds).
# It must carry only the snapshot's cus_42.
if "$BIN" goal inspect "$RUN_ID" --json | grep -q 'cus_EDITED'; then
  echo "FAIL: the run picked up the live edit — it did not resume off the snapshot"
  exit 1
fi
if ! "$BIN" goal inspect "$RUN_ID" --json | grep -q 'cus_42'; then
  echo "FAIL: the run lost the original snapshot input"
  exit 1
fi
echo "   ok — resumed off the frozen snapshot; the live edit was ignored"

APR2="$(pending_apr "$RUN_ID")"
echo "## approve the second refund ($APR2), resume to completion"
"$BIN" goal approve "$RUN_ID" "$APR2" >/dev/null
"$BIN" goal resume "$RUN_ID" >/dev/null
if [ "$(status_of "$RUN_ID")" != "completed" ]; then
  echo "FAIL: expected completed, got $(status_of "$RUN_ID")"
  exit 1
fi
echo "   ok — completed"

echo "## assert exactly-once: two refund tool.called, two refund receipts, six receipts total"
CALLS="$("$BIN" goal inspect "$RUN_ID" --json | python3 -c '
import sys, json
evs = json.load(sys.stdin)["events"]
print(sum(1 for e in evs
          if e["kind"] == "tool.called"
          and e["payload"].get("tool") == "fake.stripe.refund"))')"
if [ "$CALLS" -ne 2 ]; then
  echo "FAIL: expected 2 refund tool.called (one per duplicate), found $CALLS"
  exit 1
fi
if [ "$(refund_receipts "$RUN_ID")" -ne 2 ]; then
  echo "FAIL: expected 2 refund receipts, found $(refund_receipts "$RUN_ID")"
  exit 1
fi
TOTAL="$("$BIN" goal receipts "$RUN_ID" | grep -c 'rcpt_')"
if [ "$TOTAL" -ne 6 ]; then
  echo "FAIL: expected 6 receipts (list + refund×2 + email×2 + comment), found $TOTAL"
  exit 1
fi
echo "   ok — each duplicate refunded exactly once across the crash AND the live edit"

echo "## perdure goal replay $RUN_ID  (expect exact reproduction)"
"$BIN" goal replay "$RUN_ID"

echo "## prove ReconcileLocalDemo is NOT a built-in (it only runs from the workspace)"
EMPTY="$(mktemp -d)"
set +e
(cd "$EMPTY" && "$BIN" goal run ReconcileLocalDemo >/dev/null 2>&1)
NOBUILTIN=$?
set -e
rm -rf "$EMPTY"
if [ "$NOBUILTIN" -eq 0 ]; then
  echo "FAIL: ReconcileLocalDemo ran without its workspace — it must not be a built-in"
  exit 1
fi
echo "   ok — outside the workspace there is no such goal"

echo
echo "ALL GOOD — a user-authored plan goal ran, paused, crashed, resumed off its snapshot"
echo "           (ignoring a live edit), refunded each duplicate exactly once, and replayed."
