//! Real-filesystem snapshots for the coding harness.
//!
//! A guard session needs a baseline of the working tree taken at `tach guard
//! begin`, so that later it can tell exactly which files the external agent
//! touched and check each against the goal's `fs.write` scope. We capture that
//! baseline as a *manifest*: a map of repo-relative path -> content hash. Hashes,
//! not contents, so the baseline of a 50k-file repo is still small — and the diff
//! is a cheap map comparison.
//!
//! The walk deliberately mirrors `project::collect`'s exclusions (never descend
//! into `.git`, `.tach`, `target`, `node_modules`, or any dotdir) so Tach's own
//! `.tach/` state — which churns every `verify` — never shows up as a "change".

use crate::patch::glob_match;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// A snapshot of a tree: repo-relative (forward-slashed) path -> content hash.
pub type Manifest = BTreeMap<String, String>;

/// The user-supplied ignore set, loaded from `.tachignore`. Built-in directory
/// exclusions (dotdirs, `target`, `node_modules`) are handled by the walk itself
/// and are not represented here.
#[derive(Default, Debug)]
pub struct Ignore {
    /// Each `.tachignore` line is expanded to two globs — the entry itself and
    /// `entry/**` — so `target` ignores both the dir and everything beneath it.
    globs: Vec<String>,
}

impl Ignore {
    /// Load `<repo>/.tachignore`, ignoring blank lines and `#` comments. A missing
    /// file yields an empty ignore set.
    pub fn load(repo: &Path) -> Self {
        let mut globs = Vec::new();
        if let Ok(text) = fs::read_to_string(repo.join(".tachignore")) {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                let base = l.trim_end_matches('/');
                globs.push(base.to_string());
                globs.push(format!("{base}/**"));
            }
        }
        Ignore { globs }
    }

    /// Does any `.tachignore` pattern match this repo-relative path?
    pub fn matches(&self, rel: &str) -> bool {
        self.globs.iter().any(|g| glob_match(g, rel))
    }
}

/// A directory name the walk never descends into, regardless of `.tachignore`.
/// Dotdirs (`.git`, `.tach`, `.venv`, …) plus the usual heavy build/dep roots.
fn is_builtin_ignored_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules")
}

/// FNV-1a (64-bit) hex digest of a file's bytes — the same mixer the store uses
/// for its deterministic ids, so the whole system speaks one hash.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Walk the working tree under `repo` into a manifest, honoring the ignore set.
pub fn snapshot(repo: &Path, ignore: &Ignore) -> io::Result<Manifest> {
    let mut m = Manifest::new();
    walk(repo, repo, ignore, &mut m)?;
    Ok(m)
}

fn walk(base: &Path, dir: &Path, ignore: &Ignore, m: &mut Manifest) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // Never follow symlinks: they can form cycles and can point outside the repo.
        if ft.is_symlink() {
            continue;
        }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if ft.is_dir() {
            if is_builtin_ignored_dir(&name) || ignore.matches(&rel) {
                continue;
            }
            walk(base, &p, ignore, m)?;
        } else if ft.is_file() {
            if ignore.matches(&rel) {
                continue;
            }
            let bytes = fs::read(&p)?;
            m.insert(rel, content_hash(&bytes));
        }
    }
    Ok(())
}

/// The classified difference between a baseline manifest and a current one.
#[derive(Default, Clone, Debug, Serialize)]
pub struct Diff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Diff {
    /// Every path that changed in any way, sorted. This is the set the scope gate
    /// classifies against the goal's `fs.write` authority.
    pub fn changed(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .added
            .iter()
            .chain(&self.modified)
            .chain(&self.deleted)
            .cloned()
            .collect();
        all.sort();
        all.dedup();
        all
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Diff a baseline against the current head manifest. Deterministic and sorted.
pub fn diff(base: &Manifest, head: &Manifest) -> Diff {
    let mut d = Diff::default();
    for (path, hash) in head {
        match base.get(path) {
            None => d.added.push(path.clone()),
            Some(h) if h != hash => d.modified.push(path.clone()),
            _ => {}
        }
    }
    for path in base.keys() {
        if !head.contains_key(path) {
            d.deleted.push(path.clone());
        }
    }
    d.added.sort();
    d.modified.sort();
    d.deleted.sort();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tach_snap_{}_{}_{}",
                std::process::id(),
                tag,
                n
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn snapshot_skips_builtin_dirs_and_tracks_files() {
        let d = TempDir::new("walk");
        d.write("src/lib.rs", "fn a() {}");
        d.write("README.md", "hi");
        d.write("target/debug/junk", "x"); // must be skipped
        d.write(".git/config", "x"); // must be skipped
        d.write(".tach/goals/run/state.json", "x"); // must be skipped
        let m = snapshot(d.path(), &Ignore::default()).unwrap();
        assert!(m.contains_key("src/lib.rs"));
        assert!(m.contains_key("README.md"));
        assert!(!m.keys().any(|k| k.starts_with("target/")));
        assert!(!m.keys().any(|k| k.starts_with(".git/")));
        assert!(!m.keys().any(|k| k.starts_with(".tach/")));
    }

    #[test]
    fn tachignore_excludes_matches() {
        let d = TempDir::new("ignore");
        d.write(".tachignore", "# comment\nbuild\n*.log\n");
        d.write("src/lib.rs", "x");
        d.write("build/out.o", "x");
        d.write("debug.log", "x");
        let ig = Ignore::load(d.path());
        let m = snapshot(d.path(), &ig).unwrap();
        assert!(m.contains_key("src/lib.rs"));
        assert!(!m.contains_key("build/out.o"), "build/ dir ignored");
        assert!(!m.contains_key("debug.log"), "*.log ignored");
    }

    #[test]
    fn diff_classifies_changes() {
        let d = TempDir::new("diff");
        d.write("a.txt", "1");
        d.write("b.txt", "1");
        let base = snapshot(d.path(), &Ignore::default()).unwrap();
        d.write("b.txt", "2"); // modify
        d.write("c.txt", "3"); // add
        fs::remove_file(d.path().join("a.txt")).unwrap(); // delete
        let head = snapshot(d.path(), &Ignore::default()).unwrap();
        let diff = diff(&base, &head);
        assert_eq!(diff.added, vec!["c.txt"]);
        assert_eq!(diff.modified, vec!["b.txt"]);
        assert_eq!(diff.deleted, vec!["a.txt"]);
        assert_eq!(diff.changed(), vec!["a.txt", "b.txt", "c.txt"]);
    }
}
