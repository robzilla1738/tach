//! The durable goal store: everything a long-horizon run leaves on disk.
//!
//! Layout under a repository root:
//!
//! ```text
//! .tach/goals/<run_id>/
//!   goal.json              the resolved GoalSpec + the base source snapshot
//!   state.json             the current RunState (status, step, metrics)
//!   events.jsonl           append-only history
//!   checkpoints/<step>.json a workspace snapshot taken after each step
//! ```
//!
//! Nothing here touches the repository's real source files; the runtime works on
//! an in-memory workspace and only writes verified results back when a run
//! completes green. That separation is what makes a crash safe: the store is the
//! durable state, and the working tree is never left half-edited.

use crate::goal::GoalSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The persisted goal record: its contract plus the exact source it started from,
/// so a run is fully reproducible without depending on the live working tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalRecord {
    pub spec: GoalSpec,
    pub strategy: String,
    pub base_files: BTreeMap<String, String>,
}

/// The mutable head of a run. Overwritten after every step (the events log is the
/// immutable history; this is the latest summary).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunState {
    pub run_id: String,
    pub goal: String,
    /// running | completed | failed | cancelled | budget_exhausted
    pub status: String,
    /// Steps completed so far (== the highest checkpoint written).
    pub step: u64,
    pub consecutive_rejections: u64,
    pub patches_applied: usize,
    pub patches_rejected: usize,
    pub tests_run: usize,
    pub regressions: usize,
    pub diff_chars: usize,
    pub final_errors: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
}

/// A workspace snapshot taken after a step, enough to resume from exactly here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub step: u64,
    pub status: String,
    pub files: BTreeMap<String, String>,
    pub consecutive_rejections: u64,
    pub patches_applied: usize,
    pub patches_rejected: usize,
    pub tests_run: usize,
    pub regressions: usize,
    pub diff_chars: usize,
}

pub fn goals_root(repo: &Path) -> PathBuf {
    repo.join(".tach").join("goals")
}

pub fn run_dir(repo: &Path, run_id: &str) -> PathBuf {
    goals_root(repo).join(run_id)
}

fn checkpoints_dir(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("checkpoints")
}

pub fn events_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("events.jsonl")
}

fn goal_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("goal.json")
}

fn state_path(repo: &Path, run_id: &str) -> PathBuf {
    run_dir(repo, run_id).join("state.json")
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    fs::write(path, json)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn save_goal(repo: &Path, run_id: &str, record: &GoalRecord) -> io::Result<()> {
    write_json(&goal_path(repo, run_id), record)
}

pub fn load_goal(repo: &Path, run_id: &str) -> io::Result<GoalRecord> {
    read_json(&goal_path(repo, run_id))
}

pub fn save_state(repo: &Path, state: &RunState) -> io::Result<()> {
    write_json(&state_path(repo, &state.run_id), state)
}

pub fn load_state(repo: &Path, run_id: &str) -> io::Result<RunState> {
    read_json(&state_path(repo, run_id))
}

pub fn save_checkpoint(repo: &Path, run_id: &str, cp: &Checkpoint) -> io::Result<()> {
    write_json(
        &checkpoints_dir(repo, run_id).join(format!("{}.json", cp.step)),
        cp,
    )
}

/// Load the highest-numbered checkpoint, i.e. the most recent durable state.
pub fn load_latest_checkpoint(repo: &Path, run_id: &str) -> io::Result<Checkpoint> {
    let dir = checkpoints_dir(repo, run_id);
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if let Ok(n) = stem.parse::<u64>() {
                if best.as_ref().is_none_or(|(b, _)| n > *b) {
                    best = Some((n, path));
                }
            }
        }
    }
    match best {
        Some((_, path)) => read_json(&path),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no checkpoints recorded for this run",
        )),
    }
}

/// Every run id present in the store, sorted.
pub fn list_runs(repo: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(goals_root(repo)) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// A deterministic, offline run id derived from the goal name and the exact base
/// source it starts from. No clock, no randomness — the same goal over the same
/// code always gets the same id, which is what makes replay and resume
/// addressable without bookkeeping.
pub fn derive_run_id(goal: &str, base: &BTreeMap<String, String>) -> String {
    // FNV-1a over a stable serialization of (goal, files).
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    mix(goal.as_bytes());
    mix(&[0]);
    for (path, text) in base {
        mix(path.as_bytes());
        mix(&[0]);
        mix(text.as_bytes());
        mix(&[0]);
    }
    format!("run_{:016x}", h)
}
