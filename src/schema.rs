//! Versioned JSON Schemas for every machine-facing output Tach produces.
//!
//! These are the contract an integrator codes against. They are embedded into the
//! binary at build time (so `tach schema <name>` works hermetically, with no
//! files to ship) and are kept honest by a golden test that parses each one and
//! checks that a representative output validates against the Rust types that are
//! the actual source of truth.

/// One published schema: a short name and its JSON Schema text.
pub struct Schema {
    pub name: &'static str,
    pub title: &'static str,
    pub json: &'static str,
}

/// Every schema Tach publishes, in stable order.
pub const SCHEMAS: &[Schema] = &[
    Schema {
        name: "diagnostic",
        title: "compiler diagnostic (`tach check --json`)",
        json: include_str!("../schemas/diagnostic.schema.json"),
    },
    Schema {
        name: "patch",
        title: "typed patch (fixture coder input)",
        json: include_str!("../schemas/patch.schema.json"),
    },
    Schema {
        name: "event",
        title: "goal run event (events.jsonl)",
        json: include_str!("../schemas/event.schema.json"),
    },
    Schema {
        name: "goal",
        title: "resolved goal spec (goal.json)",
        json: include_str!("../schemas/goal.schema.json"),
    },
    Schema {
        name: "run",
        title: "goal run state (state.json)",
        json: include_str!("../schemas/run.schema.json"),
    },
    Schema {
        name: "approval",
        title: "action approval gate (approvals/<id>.json)",
        json: include_str!("../schemas/approval.schema.json"),
    },
    Schema {
        name: "receipt",
        title: "effect receipt (receipts/<id>.json)",
        json: include_str!("../schemas/receipt.schema.json"),
    },
    Schema {
        name: "bench",
        title: "suite bench output (`tach bench --suite --json`)",
        json: include_str!("../schemas/bench.schema.json"),
    },
    Schema {
        name: "test",
        title: "test report (`tach test --json`)",
        json: include_str!("../schemas/test.schema.json"),
    },
];

/// Look up a schema by name.
pub fn get(name: &str) -> Option<&'static Schema> {
    SCHEMAS.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_valid_json_with_an_id() {
        for s in SCHEMAS {
            let v: serde_json::Value =
                serde_json::from_str(s.json).unwrap_or_else(|e| panic!("{}: {}", s.name, e));
            assert!(v.get("$schema").is_some(), "{} missing $schema", s.name);
            assert!(v.get("$id").is_some(), "{} missing $id", s.name);
            assert_eq!(
                v.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "{} should be an object schema",
                s.name
            );
        }
    }

    #[test]
    fn lookup_finds_published_schemas() {
        assert!(get("event").is_some());
        assert!(get("goal").is_some());
        assert!(get("nope").is_none());
    }
}
