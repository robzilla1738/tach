#!/usr/bin/env bash
# End-to-end test of the CODING-AGENT GUARD harness against a real (non-toy) repo,
# driven entirely through the `perdure` binary — the path an external coding agent
# (Claude Code, Codex, …) actually takes. `tests/coding_e2e.rs` covers the in-process
# API; this proves the CLI dispatch, JSON stdout, exit codes, and on-disk ledger.
#
# It proves, against the binary:
#   - `perdure init --existing` adopts a real repo (Perdurefile/PERDURE_AGENT.md/.perdureignore),
#     detects the test command, scaffolds no toy source, and never touches AGENTS.md;
#     PERDURE_AGENT.md states commit is ledger-only (not git).
#   - `perdure guard begin` opens a coding session with a durable baseline.
#   - `perdure guard context --json` is a usable operating contract.
#   - an in-scope fix verifies, mints a shell.run receipt with captured artifacts,
#     and `perdure guard commit` finalizes Perdure's ledger only — git is untouched.
#   - a crash right after the receipt is durable reuses it on resume (command runs once).
#   - an OUT-OF-SCOPE edit under a dot-directory (`.github/workflows/ci.yml`) — the
#     blind spot a guardrail must catch — is detected and blocks the commit.
#   - `perdure goal replay` is consistent, and `--rerun` reproduces the verdict.
#
# Hermetic + toolchain-free: the allowlisted command is a trivial committed `check.sh`
# (greps for a fix marker), not a real `cargo test`, so CI needs no project toolchain.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

jget() { python3 -c 'import sys,json; print(json.load(sys.stdin)'"$1"')'; }

# ---------------------------------------------------------------------------
echo "## build an existing repo fixture (Cargo.toml so detection picks cargo test)"
cat > Cargo.toml <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
EOF
mkdir -p src
echo '// broken: no fix marker yet' > src/lib.rs
echo '# Fixture' > README.md
echo 'user-owned — perdure must never touch this' > AGENTS.md
# The trivial "test command": passes iff the agent wrote the FIX marker.
echo 'grep -q FIXMARKER src/lib.rs && exit 0 || exit 1' > check.sh

# ---------------------------------------------------------------------------
echo "## perdure init --existing  (adopt the repo; detect; write 3 files; leave source + AGENTS.md alone)"
"$BIN" init --existing >/dev/null
for f in Perdurefile PERDURE_AGENT.md .perdureignore; do
  [ -f "$f" ] || { echo "FAIL: init --existing did not write $f"; exit 1; }
done
[ -f src/main.pdr ] && { echo "FAIL: init scaffolded toy source"; exit 1; }
grep -q 'cargo test' Perdurefile || { echo "FAIL: Perdurefile did not detect 'cargo test'"; exit 1; }
grep -q 'command("cargo test").passes' Perdurefile || { echo "FAIL: Perdurefile missing command(...).passes require"; exit 1; }
[ "$(cat AGENTS.md)" = 'user-owned — perdure must never touch this' ] || { echo "FAIL: AGENTS.md was modified"; exit 1; }
grep -qi 'ledger' PERDURE_AGENT.md || { echo "FAIL: PERDURE_AGENT.md does not explain ledger-only commit"; exit 1; }
echo "   ok — adopted; detected cargo test; AGENTS.md + source untouched"

echo "## point the goal at the trivial check.sh command (keeps CI toolchain-free)"
cat > Perdurefile <<'EOF'
goal FixFailingTests -> Success {
  budget {
    steps: 40
  }
  allow {
    fs.write ["src/**", "tests/**"]
    shell.run ["sh check.sh"]
  }
  require {
    command("sh check.sh").passes
    no_out_of_scope_writes
  }
}
EOF
"$BIN" check Perdurefile >/dev/null 2>&1 || true   # Perdurefile isn't a .pdr module; begin re-validates it

# ---------------------------------------------------------------------------
echo "## perdure guard begin  (open a coding session over the broken tree)"
RUN_ID="$("$BIN" guard begin FixFailingTests --json | jget '["run_id"]')"
echo "## run id: $RUN_ID"
[ -n "$RUN_ID" ] || { echo "FAIL: no run id from guard begin"; exit 1; }
KIND="$(python3 -c 'import json;print(json.load(open(".perdure/goals/'"$RUN_ID"'/state.json"))["kind"])')"
[ "$KIND" = coding ] || { echo "FAIL: state.json kind=$KIND, expected coding"; exit 1; }
[ -f ".perdure/goals/$RUN_ID/baseline.json" ] || { echo "FAIL: no baseline.json"; exit 1; }
echo "   ok — coding session open with a durable baseline"

echo "## perdure guard context --json  (the operating contract is usable)"
CTX="$("$BIN" guard context --json)"
echo "$CTX" | jget '["allowed_commands"]' | grep -q 'sh check.sh' || { echo "FAIL: context missing allowed command"; exit 1; }
echo "$CTX" | jget '["allowed_files"]' | grep -q 'src/' || { echo "FAIL: context missing allowed files"; exit 1; }
echo "$CTX" | jget '["done_condition"]' | grep -qi 'verified' || { echo "FAIL: context missing done_condition"; exit 1; }
echo "   ok — context advertises allowed files/commands + done condition"

# ---------------------------------------------------------------------------
echo "## agent makes the in-scope fix, then CRASH right after the receipt is durable"
echo '// FIXMARKER applied' > src/lib.rs
set +e
"$BIN" guard verify --crash-after receipt:1 >/dev/null
CODE=$?
set -e
[ "$CODE" -eq 99 ] || { echo "FAIL: expected crash exit 99, got $CODE"; exit 1; }
RCPTS="$(ls -1 ".perdure/goals/$RUN_ID/receipts/"*.json 2>/dev/null | wc -l | tr -d ' ')"
[ "$RCPTS" -eq 1 ] || { echo "FAIL: expected 1 durable receipt after crash, found $RCPTS"; exit 1; }
VERIFIED="$("$BIN" guard status --json | jget '["verified"]')"
[ "$VERIFIED" = False ] || { echo "FAIL: verified should not be saved at the crash, got $VERIFIED"; exit 1; }
echo "   ok — crashed with the receipt durable but verified unsaved"

echo "## perdure guard verify  (resume: the receipt is REUSED, the command does not re-run)"
"$BIN" guard verify >/dev/null
VERIFIED="$("$BIN" guard status --json | jget '["verified"]')"
[ "$VERIFIED" = True ] || { echo "FAIL: resume verify did not reach verified=true"; exit 1; }
RCPTS="$(ls -1 ".perdure/goals/$RUN_ID/receipts/"*.json 2>/dev/null | wc -l | tr -d ' ')"
[ "$RCPTS" -eq 1 ] || { echo "FAIL: a second receipt was minted on resume ($RCPTS)"; exit 1; }
EXEC="$(grep -c '"shell.executed"' ".perdure/goals/$RUN_ID/events.jsonl" || true)"
[ "$EXEC" -eq 1 ] || { echo "FAIL: command executed $EXEC times across crash+resume (want exactly 1)"; exit 1; }
grep -q '"receipt.reused"' ".perdure/goals/$RUN_ID/events.jsonl" || { echo "FAIL: no receipt.reused event on resume"; exit 1; }
# The receipt records a real captured subprocess: exit 0 + an on-disk stdout artifact.
RC="$(ls -1 ".perdure/goals/$RUN_ID/receipts/"*.json | head -1)"
EXIT0="$(python3 -c 'import json;print(json.load(open("'"$RC"'"))["output"]["exit_code"])')"
[ "$EXIT0" = 0 ] || { echo "FAIL: receipt exit_code=$EXIT0, expected 0"; exit 1; }
ls -1 ".perdure/goals/$RUN_ID/artifacts/"*.stdout >/dev/null 2>&1 || { echo "FAIL: no stdout artifact captured"; exit 1; }
echo "   ok — command ran exactly once; receipt reused; artifacts captured"

echo "## perdure guard finalize  (preferred spelling; finalize Perdure's ledger ONLY — git untouched)"
"$BIN" guard finalize >/dev/null
STATUS="$(python3 -c 'import json;print(json.load(open(".perdure/goals/'"$RUN_ID"'/state.json"))["status"])')"
[ "$STATUS" = completed ] || { echo "FAIL: status=$STATUS, expected completed"; exit 1; }
[ ! -d .git ] || { echo "FAIL: commit created a .git directory — it must not touch git"; exit 1; }
grep -q '"guard.committed"' ".perdure/goals/$RUN_ID/events.jsonl" || { echo "FAIL: no guard.committed event"; exit 1; }
echo "   ok — run committed into the ledger; no git repo created"

echo "## perdure goal replay $RUN_ID  (consistent from receipts) + --rerun (reproduces)"
"$BIN" goal replay "$RUN_ID" >/dev/null
"$BIN" goal replay "$RUN_ID" --rerun >/dev/null
echo "   ok — replay consistent and reproducible"

# ---------------------------------------------------------------------------
echo "## NEW SESSION: an out-of-scope edit under .github/ must be caught (the blind-spot fix)"
RUN2="$("$BIN" guard begin FixFailingTests --json | jget '["run_id"]')"
mkdir -p .github/workflows
echo 'on: push' > .github/workflows/ci.yml      # dot-directory: must NOT be invisible
echo '# edited out of scope' >> README.md         # plain out-of-scope file
DIFF="$("$BIN" guard diff --json)"
echo "$DIFF" | jget '["out_of_scope"]' | grep -q '.github/workflows/ci.yml' || { echo "FAIL: .github CI edit not flagged out-of-scope"; exit 1; }
echo "$DIFF" | jget '["out_of_scope"]' | grep -q 'README.md' || { echo "FAIL: README edit not flagged out-of-scope"; exit 1; }
echo "$DIFF" | jget '["rejected"]' | grep -q True || { echo "FAIL: diff not marked rejected"; exit 1; }
set +e
"$BIN" guard verify >/dev/null; VCODE=$?
"$BIN" guard commit >/dev/null; CCODE=$?
set -e
[ "$VCODE" -ne 0 ] || { echo "FAIL: verify passed despite out-of-scope edits"; exit 1; }
[ "$CCODE" -ne 0 ] || { echo "FAIL: commit succeeded despite out-of-scope edits"; exit 1; }
grep -q '"scope.violation"' ".perdure/goals/$RUN2/events.jsonl" || { echo "FAIL: no scope.violation recorded"; exit 1; }
"$BIN" guard abort >/dev/null
echo "   ok — .github/ CI edit detected, verify + commit refused, violation recorded"

echo
echo "ALL GOOD — a real repo was adopted, an in-scope fix verified with a real captured"
echo "           receipt, crashed and reused the receipt, committed to the ledger (git"
echo "           untouched), replayed — and an out-of-scope .github/ edit was rejected."
