//! End-to-end test for the coding-agent harness, driven through the real CLI
//! (`tach::cli::run`) against a throwaway copy of `fixtures/existing-rust`.
//!
//! It proves the P0 contract: `init --existing` adopts a real repo without
//! scaffolding source; a guard session snapshots the tree, runs the project's
//! real command (a cheap script here, so CI needs no toolchain), captures a
//! receipt and an artifact, and certifies a `verified` bit; an out-of-scope edit
//! is detected and blocks commit; a crash mid-verify reuses its receipt on
//! resume; and the run replays from its durable history.
//!
//! One test, run serially in its own binary, because it changes the process cwd
//! (the CLI resolves paths from it). State is asserted by reading the store back.

use std::fs;
use std::path::{Path, PathBuf};

use tach::event::{self, kind};
use tach::{adopt, cli, guard, runtime, store};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/existing-rust")
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

fn run(args: &[&str]) -> i32 {
    cli::run(args.iter().map(|s| s.to_string()).collect())
}

fn read(p: &Path, rel: &str) -> String {
    fs::read_to_string(p.join(rel)).unwrap()
}

fn write(p: &Path, rel: &str, text: &str) {
    let full = p.join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, text).unwrap();
}

/// The execution Tachfile: identical in shape to what `init` generates, but with
/// the cheap `sh check.sh` command so the e2e exercises real process execution
/// without a Rust toolchain.
fn execution_tachfile() -> String {
    adopt::coding_goal_source(
        "FixFailingTests",
        "sh check.sh",
        &["src/**".to_string(), "tests/**".to_string()],
    )
}

#[test]
fn coding_harness_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("tach_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    copy_dir(&fixture_root(), &tmp);

    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // Run the body and always restore the cwd, even on panic.
    let result = std::panic::catch_unwind(|| body(&tmp));
    std::env::set_current_dir(&original_cwd).unwrap();
    let _ = fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn body(tmp: &Path) {
    // (2 — detection) The adopt layer recognizes a Rust repo's test command.
    assert_eq!(adopt::detect(tmp).unwrap().command, "cargo test");

    // (1 — init --existing) Adopt the repo. A pre-existing AGENTS.md is sacred.
    write(tmp, "AGENTS.md", "user owned — do not touch");
    assert_eq!(run(&["init", "--existing"]), 0);
    assert!(tmp.join("Tachfile").exists());
    assert!(tmp.join("TACH_AGENT.md").exists());
    assert!(tmp.join(".tachignore").exists());
    assert!(
        !tmp.join("src/main.tach").exists(),
        "no toy source scaffolded"
    );
    assert_eq!(
        read(tmp, "AGENTS.md"),
        "user owned — do not touch",
        "AGENTS.md must never be clobbered"
    );
    let generated = read(tmp, "Tachfile");
    assert!(
        generated.contains("command(\"cargo test\").passes"),
        "generated goal must require the detected command: {generated}"
    );
    assert!(generated.contains("shell.run"));

    // Swap in the cheap execution command for the rest of the flow.
    write(tmp, "Tachfile", &execution_tachfile());

    // (3 — begin) Open a guard session over the working tree.
    assert_eq!(run(&["guard", "begin", "FixFailingTests"]), 0);
    let id = store::active_guard(tmp).expect("active session recorded");
    let state = store::load_state(tmp, &id).unwrap();
    assert_eq!(state.kind, "coding");
    assert!(
        store::load_baseline(tmp, &id).is_ok(),
        "baseline snapshotted"
    );

    // The repo starts broken → verify fails (exit 1), no commit possible.
    assert_eq!(run(&["guard", "verify"]), 1, "broken tree must not verify");

    // (4 — in-scope edit + verify) The agent fixes an allowed file.
    write(
        tmp,
        "src/lib.rs",
        "// FIXED\npub fn answer() -> i32 { 42 }\n",
    );
    assert_eq!(run(&["guard", "verify"]), 0, "fixed tree must verify");
    let st = store::load_state(tmp, &id).unwrap();
    assert!(st.verified);
    let receipts = store::list_receipts(tmp, &id);
    let passing = receipts
        .iter()
        .find(|r| r.output.get("exit_code").and_then(|v| v.as_i64()) == Some(0))
        .expect("a passing shell.run receipt");
    let artifact = passing
        .output
        .get("stdout_artifact")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(tmp.join(artifact).exists(), "stdout artifact captured");

    // (5 — out-of-scope edit) An edit outside src/**,tests/** is rejected.
    write(tmp, "README.md", "out-of-scope edit\n");
    let d = guard::diff(tmp, &id).unwrap();
    assert_eq!(d.out_of_scope, vec!["README.md"]);
    assert!(d.rejected);
    assert_ne!(
        run(&["guard", "commit"]),
        0,
        "commit must refuse out-of-scope"
    );
    let events = event::read_all(&store::events_path(tmp, &id)).unwrap();
    assert!(
        events.iter().any(|e| e.kind == kind::SCOPE_VIOLATION),
        "scope violation recorded in history"
    );

    // Remove the out-of-scope file, re-verify, and commit for real.
    fs::remove_file(tmp.join("README.md")).unwrap();
    assert_eq!(run(&["guard", "verify"]), 0);
    assert_eq!(run(&["guard", "commit"]), 0, "clean verified tree commits");
    assert_eq!(store::load_state(tmp, &id).unwrap().status, "completed");

    // (7 — replay) The committed run replays from its receipts (no re-exec), and
    // a --rerun reproduces the verdict by actually re-running the command.
    let rr = runtime::replay_run(tmp, &id, false).unwrap();
    assert!(rr.identical, "recorded run is self-consistent");
    let rr2 = runtime::replay_run(tmp, &id, true).unwrap();
    assert!(rr2.identical, "re-run reproduces the verdict");

    // (6 — crash/resume) A fresh session: crash right after the receipt is durable
    // but before the verified bit is saved; resuming verify reuses the receipt and
    // never re-runs the command.
    assert_eq!(run(&["guard", "begin", "FixFailingTests"]), 0);
    let id2 = store::active_guard(tmp).expect("second session");
    assert_ne!(id2, id, "a fresh run id, not a reuse of the committed one");
    guard::verify_inner(tmp, &id2, false, Some(guard::GuardCrash::AfterReceipt(1))).unwrap();
    assert!(!store::load_state(tmp, &id2).unwrap().verified);
    assert_eq!(store::list_receipts(tmp, &id2).len(), 1);

    assert_eq!(run(&["guard", "verify"]), 0, "resume verify passes");
    assert_eq!(
        store::list_receipts(tmp, &id2).len(),
        1,
        "no duplicate receipt on resume"
    );
    let ev2 = event::read_all(&store::events_path(tmp, &id2)).unwrap();
    assert_eq!(
        ev2.iter()
            .filter(|e| e.kind == kind::SHELL_EXECUTED)
            .count(),
        1,
        "command executed exactly once across crash+resume"
    );
    assert!(ev2.iter().any(|e| e.kind == kind::RECEIPT_REUSED));
}
