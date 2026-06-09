#!/usr/bin/env bash
# End-to-end smoke test: scaffold a broken project, prove `perdure check` is red,
# run the repair loop, and prove the project ends green with passing tests.
# Exit code 0 == everything works. Safe for headless / CI / cloud-agent use.
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

echo "## perdure check  (expect failure: 3 planted bugs)"
if "$BIN" check >/dev/null 2>&1; then
  echo "FAIL: expected check to report errors on the fresh demo"
  exit 1
fi
echo "   ok — check is red as expected"

echo "## perdure fix"
"$BIN" fix

echo "## perdure check  (expect success)"
"$BIN" check

echo "## perdure test   (expect all green)"
"$BIN" test

echo "## perdure replay (expect exact reproduction)"
"$BIN" replay >/dev/null

echo "## multi-file imports: a project split across files checks, tests, and auto-repairs"
cd .. && "$BIN" new multi --clean >/dev/null && cd multi
cat > src/util.pdr <<'PDR'
fn double(x: Int) -> Int {
  return x * 2
}
PDR
cat > src/main.pdr <<'PDR'
import log
import "./util.pdr"

fn greet(name: String) -> String {
  return name
}

fn main() -> Int effects [log.write] {
  log.info("multi-file demo")
  return double(21)
}
PDR
"$BIN" check
"$BIN" test
echo "   ok — cross-file call resolves through the file import"

echo "## a missing file import is a repairable diagnostic (fix adds it)"
cat > src/main.pdr <<'PDR'
import log

fn greet(name: String) -> String {
  return name
}

fn main() -> Int effects [log.write] {
  log.info("multi-file demo")
  return double(21)
}
PDR
if "$BIN" check >/dev/null 2>&1; then
  echo "FAIL: expected E0473 symbol_not_imported"
  exit 1
fi
"$BIN" fix >/dev/null
grep -q 'import "./util.pdr"' src/main.pdr || { echo "FAIL: fix did not insert the file import"; exit 1; }
"$BIN" check
echo "   ok — fix inserted import \"./util.pdr\" and the project is green"

echo
echo "ALL GOOD — red → green, reproduced."
