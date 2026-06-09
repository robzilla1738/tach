use crate::ast::{FnDecl, Item, Module, TestDecl};
use crate::diagnostics::Diagnostic;
use crate::parser::parse;
use crate::source::SourceFile;
use crate::types::TypeRegistry;
use std::collections::{HashMap, HashSet};

/// One parsed source file plus its derived facts.
pub struct Unit {
    pub source: SourceFile,
    pub module: Module,
    pub imports: HashSet<String>,
}

/// A whole Perdure program: every source file in a project, parsed.
///
/// The checker walks units one at a time (diagnostics are per-file), while the
/// interpreter sees the merged set of functions and types.
pub struct Program {
    pub units: Vec<Unit>,
}

impl Program {
    pub fn parse_sources(sources: Vec<SourceFile>) -> (Program, Vec<Diagnostic>) {
        let mut units = Vec::new();
        let mut diags = Vec::new();
        for src in sources {
            let (module, ds) = parse(&src.path, &src.text);
            diags.extend(ds);
            let imports = module
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Import(im) if im.file.is_none() => Some(im.module.clone()),
                    _ => None,
                })
                .collect();
            units.push(Unit {
                source: src,
                module,
                imports,
            });
        }
        (Program { units }, diags)
    }

    /// All functions across the program, keyed by name (last definition wins).
    pub fn functions(&self) -> HashMap<String, &FnDecl> {
        let mut m = HashMap::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Fn(f) = it {
                    m.insert(f.name.clone(), f);
                }
            }
        }
        m
    }

    pub fn type_registry(&self) -> TypeRegistry {
        let mut r = TypeRegistry::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Type(t) = it {
                    r.add_decl(t);
                }
            }
        }
        r
    }

    pub fn tests(&self) -> Vec<&TestDecl> {
        let mut v = Vec::new();
        for u in &self.units {
            for it in &u.module.items {
                if let Item::Test(t) = it {
                    v.push(t);
                }
            }
        }
        v
    }
}

/// Normalize a file import (`import "./billing.pdr"`) against the importing
/// file's workspace-relative path. `Err` carries the human reason: absolute
/// paths and paths that climb out of the workspace root are refused — a file
/// import can only ever name a file the workspace already contains.
pub fn normalize_import(importer: &str, raw: &str) -> Result<String, String> {
    let raw_n = raw.replace('\\', "/");
    if raw_n.starts_with('/') || raw_n.chars().nth(1) == Some(':') {
        return Err("is absolute — file imports are workspace-relative".into());
    }
    let mut stack: Vec<String> = match importer.rsplit_once('/') {
        Some((dir, _)) => dir.split('/').map(|s| s.to_string()).collect(),
        None => Vec::new(),
    };
    for seg in raw_n.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err("climbs out of the workspace root".into());
                }
            }
            s => stack.push(s.to_string()),
        }
    }
    Ok(stack.join("/"))
}

/// The canonical relative spelling of an import from `from` to `to` (both
/// workspace-relative paths): `./sibling.pdr` or `../src/lib.pdr`. Used by the
/// E0473 preferred patch so `fix` inserts exactly what a human would write.
pub fn relative_import_path(from: &str, to: &str) -> String {
    let fdir: Vec<&str> = match from.rsplit_once('/') {
        Some((d, _)) => d.split('/').collect(),
        None => Vec::new(),
    };
    let tsegs: Vec<&str> = to.split('/').collect();
    let (tdir, tfile) = tsegs.split_at(tsegs.len() - 1);
    let common = fdir
        .iter()
        .zip(tdir.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = fdir.len() - common;
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
    parts.extend(tdir[common..].iter().map(|s| s.to_string()));
    parts.push(tfile[0].to_string());
    if ups == 0 {
        format!("./{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod import_path_tests {
    use super::*;

    #[test]
    fn normalization_resolves_dots_and_refuses_escapes() {
        assert_eq!(
            normalize_import("tests/auth_test.pdr", "../src/auth.pdr").unwrap(),
            "src/auth.pdr"
        );
        assert_eq!(
            normalize_import("src/main.pdr", "./billing.pdr").unwrap(),
            "src/billing.pdr"
        );
        assert_eq!(
            normalize_import("main.pdr", "./lib.pdr").unwrap(),
            "lib.pdr"
        );
        assert!(normalize_import("main.pdr", "../outside.pdr").is_err());
        assert!(normalize_import("src/a.pdr", "/etc/x.pdr").is_err());
    }

    #[test]
    fn relative_spelling_round_trips() {
        assert_eq!(
            relative_import_path("tests/auth_test.pdr", "src/auth.pdr"),
            "../src/auth.pdr"
        );
        assert_eq!(
            relative_import_path("src/main.pdr", "src/billing.pdr"),
            "./billing.pdr"
        );
        assert_eq!(relative_import_path("main.pdr", "lib.pdr"), "./lib.pdr");
        // The spelling normalizes back to the target.
        assert_eq!(
            normalize_import(
                "tests/auth_test.pdr",
                &relative_import_path("tests/auth_test.pdr", "src/auth.pdr")
            )
            .unwrap(),
            "src/auth.pdr"
        );
    }
}
