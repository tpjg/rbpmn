//! RFC 7386 JSON Merge Patch — how handler completions write variables.
//! Merge patch, not replacement: concurrent parallel branches must not
//! clobber each other's writes. Implemented by the letter of the RFC,
//! including its footguns (a non-object patch replaces the whole document;
//! null deletes a member) — documented, not reinterpreted.

use serde_json::{Map, Value};

pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(entries) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let obj = target.as_object_mut().unwrap();
            for (key, value) in entries {
                if value.is_null() {
                    obj.remove(key);
                } else {
                    merge_patch(obj.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn apply(target: Value, patch: Value) -> Value {
        let mut t = target;
        merge_patch(&mut t, &patch);
        t
    }

    #[test]
    fn rfc7386_appendix_cases() {
        assert_eq!(apply(json!({"a":"b"}), json!({"a":"c"})), json!({"a":"c"}));
        assert_eq!(
            apply(json!({"a":"b"}), json!({"b":"c"})),
            json!({"a":"b","b":"c"})
        );
        assert_eq!(apply(json!({"a":"b"}), json!({"a":null})), json!({}));
        assert_eq!(
            apply(json!({"a":"b","b":"c"}), json!({"a":null})),
            json!({"b":"c"})
        );
        assert_eq!(
            apply(json!({"a":["b"]}), json!({"a":"c"})),
            json!({"a":"c"})
        );
        assert_eq!(
            apply(json!({"a":"c"}), json!({"a":["b"]})),
            json!({"a":["b"]})
        );
        assert_eq!(
            apply(json!({"a":{"b":"c"}}), json!({"a":{"b":"d","c":null}})),
            json!({"a":{"b":"d"}})
        );
        assert_eq!(
            apply(json!({"a":[{"b":"c"}]}), json!({"a":[1]})),
            json!({"a":[1]})
        );
        assert_eq!(
            apply(json!(["a", "b"]), json!(["c", "d"])),
            json!(["c", "d"])
        );
        assert_eq!(apply(json!({"a":"b"}), json!(["c"])), json!(["c"]));
        assert_eq!(apply(json!({"a":"foo"}), json!(null)), json!(null));
        assert_eq!(apply(json!({"a":"foo"}), json!("bar")), json!("bar"));
        assert_eq!(
            apply(json!({"e":null}), json!({"a":1})),
            json!({"e":null,"a":1})
        );
        assert_eq!(
            apply(json!([1, 2]), json!({"a":"b","c":null})),
            json!({"a":"b"})
        );
        assert_eq!(
            apply(json!({}), json!({"a":{"bb":{"ccc":null}}})),
            json!({"a":{"bb":{}}})
        );
    }
}
