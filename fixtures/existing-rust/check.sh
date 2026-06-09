# The fixture's "test command", run by the guard via `sh check.sh`. It passes
# (exit 0) only once the agent has written the FIXED marker into src/lib.rs.
# Cheap and toolchain-free so the e2e exercises real process execution in CI.
grep -q FIXED src/lib.rs && exit 0 || exit 1
