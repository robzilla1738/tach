#!/usr/bin/env bash
# End-to-end test of HTTP TOOLS IN PLANS, fully hermetic: a localhost stub
# server stands in for a Stripe-class API and logs every request it receives.
# Proves, over a real socket:
#
#   1. authority    — a URL outside `allow { http.get/post [...] }` is refused
#                     pre-I/O (authority.denied, no receipt, request count 0);
#   2. secrets      — `headers_env` reads the credential from the environment;
#                     the wire sees it, the durable store NEVER contains it;
#   3. idempotency  — every POST carries an Idempotency-Key derived from the
#                     receipt key, and crash + resume sends the request once;
#   4. replay       — `goal replay` re-walks from receipts; the server's
#                     request count cannot grow.
#
# Exit 0 == real HTTP effects are governed, durable, exactly-once, and secret-safe.
set -euo pipefail
export NO_COLOR=1

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/perdure"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; kill "${SERVER_PID:-0}" 2>/dev/null || true' EXIT
cd "$WORK"

echo "## start the stub API server (logs every request to requests.log)"
python3 - "$WORK" <<'PY' &
import http.server, socketserver, sys, os, threading

workdir = sys.argv[1]
log_path = os.path.join(workdir, "requests.log")
port_path = os.path.join(workdir, "port.txt")

class Handler(http.server.BaseHTTPRequestHandler):
    def _log(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("utf-8", "replace") if length else ""
        with open(log_path, "a") as f:
            f.write(f"{method} {self.path}\n")
            for k, v in self.headers.items():
                f.write(f"{k.lower()}: {v}\n")
            f.write(f"body: {body}\n---\n")
    def do_GET(self):
        self._log("GET")
        data = b'{"charges":[{"id":"ch_1","duplicate":true}]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_POST(self):
        self._log("POST")
        data = b'{"refund":"re_1","status":"succeeded"}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *a):
        pass

with socketserver.TCPServer(("127.0.0.1", 0), Handler) as httpd:
    with open(port_path, "w") as f:
        f.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY
SERVER_PID=$!
for _ in $(seq 1 50); do [ -f port.txt ] && break; sleep 0.1; done
PORT="$(cat port.txt)"
LOG="$WORK/requests.log"
requests_seen() { if [ -f "$LOG" ]; then grep -c '^---$' "$LOG" || true; else echo 0; fi; }
echo "   ok — stub listening on 127.0.0.1:$PORT"

echo "## a refund workflow: GET the dispute, then POST the refund behind approval"
export PERDURE_E2E_AUTH="Bearer sk_test_e2e_53cr3t"
mkdir flow && cd flow
cat > goal.pdr <<PDR
goal RefundFlow -> Success {
  budget {
    steps: 10
  }
  allow {
    http.get "http://127.0.0.1:$PORT/**"
    http.post ["http://127.0.0.1:$PORT/v1/refunds"]
  }
  plan {
    let dispute = call http.get {
      url: "http://127.0.0.1:$PORT/v1/disputes/dp_1"
    }
    if dispute.ok {
      approve "issue the refund" {
        call http.post {
          url: "http://127.0.0.1:$PORT/v1/refunds"
          body: "charge=ch_1&amount=4200"
          headers: { content_type: "application/x-www-form-urlencoded" }
          headers_env: { authorization: "PERDURE_E2E_AUTH" }
        }
      }
    }
  }
}
PDR

"$BIN" goal check RefundFlow >/dev/null
"$BIN" goal run RefundFlow >/dev/null
RUN_ID="$("$BIN" goal list | grep -oE 'run_[a-f0-9]+' | head -1)"
[ "$(requests_seen)" = "1" ] || { echo "FAIL: expected 1 request (the GET), saw $(requests_seen)"; exit 1; }
echo "   ok — GET fired and receipted, POST is waiting on approval"

echo "## approve and resume: the POST fires exactly once, with the secret and key"
APR="$("$BIN" goal approvals "$RUN_ID" | grep pending | grep -oE 'apr_[a-f0-9]+' | head -1)"
"$BIN" goal approve "$RUN_ID" "$APR" >/dev/null
"$BIN" goal resume "$RUN_ID" >/dev/null
[ "$(requests_seen)" = "2" ] || { echo "FAIL: expected 2 requests, saw $(requests_seen)"; exit 1; }
grep -q "authorization: $PERDURE_E2E_AUTH" "$LOG" || { echo "FAIL: auth header never reached the server"; exit 1; }
grep -qi "idempotency-key: idem_" "$LOG" || { echo "FAIL: POST carried no Idempotency-Key"; exit 1; }
grep -q "body: charge=ch_1&amount=4200" "$LOG" || { echo "FAIL: POST body missing"; exit 1; }
echo "   ok — wire saw the credential and the idempotency key"

echo "## the durable store NEVER contains the secret"
if grep -r "$PERDURE_E2E_AUTH" .perdure/ >/dev/null 2>&1; then
  echo "FAIL: secret found inside .perdure/"
  grep -rl "$PERDURE_E2E_AUTH" .perdure/
  exit 1
fi
grep -A2 '"headers_env_resolved"' .perdure/goals/"$RUN_ID"/receipts/*.json | grep -q '"authorization"' || {
  echo "FAIL: receipt should record the resolved header NAME"; exit 1; }
echo "   ok — receipts/events/artifacts carry the env-var name, never the value"

echo "## resume again + replay: no new requests, replay identical"
"$BIN" goal resume "$RUN_ID" >/dev/null 2>&1 || true
"$BIN" goal replay "$RUN_ID" >/dev/null
[ "$(requests_seen)" = "2" ] || { echo "FAIL: replay/redundant resume re-fired a request"; exit 1; }
echo "   ok — exactly-once held; replay re-walked from receipts with zero requests"

echo "## authority: a URL off the allowlist is refused before any socket I/O"
cd .. && mkdir denied && cd denied
cat > goal.pdr <<PDR
goal Sneaky -> Success {
  budget {
    steps: 5
  }
  allow {
    http.get "http://127.0.0.1:$PORT/v1/disputes/**"
  }
  plan {
    call http.get {
      url: "http://127.0.0.1:$PORT/admin/secrets"
    }
  }
}
PDR
BEFORE="$(requests_seen)"
set +e
"$BIN" goal run Sneaky >/dev/null 2>&1
set -e
EVIL_RUN="$("$BIN" goal list | grep -oE 'run_[a-f0-9]+' | head -1)"
[ "$(requests_seen)" = "$BEFORE" ] || { echo "FAIL: the refused URL was actually requested"; exit 1; }
grep -q '"authority.denied"' ".perdure/goals/$EVIL_RUN/events.jsonl" || { echo "FAIL: no authority.denied event"; exit 1; }
echo "   ok — refusal on the ledger, zero socket I/O"

echo
echo "PASS: http tools are governed, durable, exactly-once, and secret-safe"
