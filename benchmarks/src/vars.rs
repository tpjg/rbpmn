//! Seeded variable documents.
//!
//! Determinism is a reproducibility requirement, not a convenience: the
//! result file records the seed and the run id, and the same pair rebuilds
//! the same documents — which is also what lets the correlator know the keys
//! it must deliver without reading them back out of the database.
//!
//! The RNG is the one the fixture corpus's model generator uses
//! (`crates/rbpmn-core/tests/modelgen`): tiny, self-contained, and already
//! trusted to make random runs reproducible in this repo.

use crate::scenario::{FieldSpec, FieldValue};
use serde_json::{Map, Value};

/// The same LCG `modelgen::Rng` uses. Copied rather than shared because that
/// module is a test-only file in another crate; the constants are the
/// contract, and they are three lines.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(2) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() as usize) % n
        }
    }
}

/// Build instance `index`'s variable document.
///
/// The RNG is re-seeded per instance from (seed, index) rather than drawn
/// from one stream, so instance 7's document is the same whether the batch
/// was generated in one process or three — the modes are separately
/// invocable, and a document that depended on arrival order would not
/// survive that.
pub fn document(fields: &[FieldSpec], seed: u64, run_id: &str, index: u32) -> Value {
    let mut rng = Rng::new(seed ^ (u64::from(index) << 17));
    let mut root = Map::new();
    for field in fields {
        let value = match &field.value {
            FieldValue::Pick { values } => {
                if values.is_empty() {
                    Value::Null
                } else {
                    values[rng.below(values.len())].clone()
                }
            }
            FieldValue::PickBool => Value::Bool(rng.below(2) == 1),
            FieldValue::Unique { prefix } => Value::String(unique(prefix, run_id, index)),
            FieldValue::Constant { value } => value.clone(),
            FieldValue::Filler { bytes } => Value::String("x".repeat(*bytes)),
        };
        insert_path(&mut root, &field.path, value);
    }
    Value::Object(root)
}

/// The correlation key for instance `index` — the same string
/// [`document`] wrote, derived rather than remembered.
pub fn unique(prefix: &str, run_id: &str, index: u32) -> String {
    format!("{prefix}{run_id}-{index}")
}

/// Insert at a FEEL qualified name, creating intermediate objects. A segment
/// that collides with a non-object value overwrites it — scenarios are
/// authored data, and two fields claiming the same path is an authoring
/// mistake the result file makes visible by carrying the document.
fn insert_path(root: &mut Map<String, Value>, path: &str, value: Value) {
    let mut segments = path.split('.').peekable();
    let mut current = root;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            current.insert(segment.to_string(), value);
            return;
        }
        let entry = current
            .entry(segment.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        current = entry.as_object_mut().expect("just made it an object");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::FieldSpec;

    fn field(path: &str, value: FieldValue) -> FieldSpec {
        FieldSpec {
            path: path.to_string(),
            value,
        }
    }

    #[test]
    fn qualified_names_become_nested_objects() {
        let fields = vec![
            field(
                "order.id",
                FieldValue::Unique {
                    prefix: "order-".into(),
                },
            ),
            field(
                "order.priority",
                FieldValue::Constant {
                    value: serde_json::json!("high"),
                },
            ),
        ];
        let doc = document(&fields, 42, "abc", 7);
        assert_eq!(doc["order"]["id"], serde_json::json!("order-abc-7"));
        assert_eq!(doc["order"]["priority"], serde_json::json!("high"));
    }

    #[test]
    fn the_same_index_reproduces_the_same_document() {
        let fields = vec![field(
            "region",
            FieldValue::Pick {
                values: vec![
                    serde_json::json!("emea"),
                    serde_json::json!("amer"),
                    serde_json::json!("apac"),
                ],
            },
        )];
        for index in 0..64 {
            assert_eq!(
                document(&fields, 7, "run", index),
                document(&fields, 7, "run", index),
                "instance {index} must not depend on arrival order"
            );
        }
        // And a different seed must actually move it, or the axis is fake.
        let a: Vec<Value> = (0..64).map(|i| document(&fields, 7, "run", i)).collect();
        let b: Vec<Value> = (0..64).map(|i| document(&fields, 8, "run", i)).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn a_filler_field_is_the_size_it_claims() {
        let fields = vec![field("blob", FieldValue::Filler { bytes: 1024 })];
        let doc = document(&fields, 1, "run", 0);
        assert_eq!(doc["blob"].as_str().expect("a string").len(), 1024);
    }
}
