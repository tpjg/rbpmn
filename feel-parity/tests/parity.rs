//! The guarantee: every expression in our FEEL subset evaluates *identically*
//! under dsntk, so a v1 condition keeps its meaning when dsntk lands
//! (design brief, "Condition grammar" and "Post-v1: decisions").
//!
//! Two outcomes are allowed and are not failures:
//!   - we refuse what dsntk accepts — a strict subset may reject more
//!     (ordering ops require a number literal; that is deliberate);
//!   - dsntk answers null where we answer false — identical after the root
//!     collapse, which is the only thing a sequence flow observes.
//!
//! Everything else is a divergence and fails the run.

use rbpmn_feel_parity::{Answer, dsntk, rbpmn};
use serde_json::{Value, json};

fn documents() -> Vec<Value> {
    vec![
        json!({}),
        json!({ "a": 1 }),
        json!({ "a": 2 }),
        json!({ "a": -1 }),
        json!({ "a": 1.5 }),
        json!({ "a": "one" }),
        json!({ "a": "" }),
        json!({ "a": true }),
        json!({ "a": false }),
        json!({ "a": null }),
        json!({ "b": true }),
        json!({ "b": false }),
        json!({ "x": { "y": 1 } }),
        json!({ "x": { "y": "one" } }),
        json!({ "x": { "y": null } }),
        json!({ "a": 1, "b": true, "x": { "y": 2 } }),
    ]
}

const PATHS: [&str; 4] = ["a", "b", "x.y", "zz"];
const OPS: [&str; 6] = ["=", "!=", "<", "<=", ">", ">="];
const LITERALS: [&str; 9] = [
    "1", "2", "-1", "1.5", "\"one\"", "\"\"", "true", "false", "null",
];

/// One comparison. Returns a description when the two diverge.
fn diverges(src: &str, vars: &Value) -> Option<String> {
    let (ours, theirs) = (rbpmn(src, vars), dsntk(src, vars));
    match (ours.taken(), theirs.taken()) {
        // A strict subset may reject more, never evaluate differently.
        (None, _) => None,
        // ...but it must never accept what the reference cannot evaluate.
        (Some(o), None) => Some(format!(
            "{src:<28} on {vars:<36} ours={o:<5} dsntk REFUSED ({theirs:?})"
        )),
        (Some(o), Some(t)) if o != t => Some(format!(
            "{src:<28} on {vars:<36} ours={o:<5} dsntk={t} (raw {theirs:?})"
        )),
        _ => None,
    }
}

fn assert_no_divergence(label: &str, cases: &[(String, Value)]) {
    let checked = cases.len();
    let mut found = Vec::new();
    for (src, vars) in cases {
        if let Some(d) = diverges(src, vars) {
            found.push(d);
        }
    }
    assert!(checked > 0, "{label}: generated no cases");
    if !found.is_empty() {
        let shown: Vec<&String> = found.iter().take(25).collect();
        panic!(
            "{label}: {} of {checked} comparisons diverge from dsntk:\n  {}\n{}",
            found.len(),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  "),
            if found.len() > shown.len() {
                format!("  ... and {} more", found.len() - shown.len())
            } else {
                String::new()
            }
        );
    }
    println!("{label}: {checked} comparisons, no divergence");
}

#[test]
fn atoms_evaluate_identically() {
    let mut cases = Vec::new();
    for p in PATHS {
        for o in OPS {
            for l in LITERALS {
                for d in documents() {
                    cases.push((format!("{p} {o} {l}"), d));
                }
            }
        }
    }
    assert_no_divergence("atoms", &cases);
}

#[test]
fn connectives_are_kleene() {
    let atoms = [
        "a = 1",
        "a != 1",
        "a = null",
        "a != null",
        "b = true",
        "zz = 1",
        "zz != 1",
        "x.y > 1",
    ];
    let mut cases = Vec::new();
    for l in atoms {
        for r in atoms {
            for c in ["and", "or"] {
                for d in documents() {
                    cases.push((format!("{l} {c} {r}"), d.clone()));
                    cases.push((format!("({l}) {c} ({r})"), d));
                }
            }
        }
    }
    assert_no_divergence("connectives", &cases);
}

/// The null rules, stated as the two independent rules they are. These are the
/// cases that were wrong before this crate existed.
#[test]
fn null_rules_match_the_reference() {
    let cases: Vec<(&str, Value)> = vec![
        // Rule A — against the `null` literal: a boolean, both directions.
        ("a = null", json!({})),
        ("a = null", json!({ "a": null })),
        ("a = null", json!({ "a": 1 })),
        ("a != null", json!({})),
        ("a != null", json!({ "a": 1 })),
        // Rule B — a missing/null value against a non-null literal is null,
        // including under `!=`. A missing variable must not satisfy a
        // negative condition.
        ("a = 1", json!({})),
        ("a != 1", json!({})),
        ("a = 1", json!({ "a": null })),
        ("a != 1", json!({ "a": null })),
        ("a != \"one\"", json!({})),
        ("a != true", json!({})),
        // Rule B applies to every other type mismatch identically.
        ("a = 1", json!({ "a": "one" })),
        ("a != 1", json!({ "a": "one" })),
        ("a != 1", json!({ "a": true })),
    ];
    let cases: Vec<(String, Value)> = cases.into_iter().map(|(s, v)| (s.to_string(), v)).collect();
    assert_no_divergence("null rules", &cases);
}

/// `==` is accepted on input and normalized to FEEL's `=`; the normalized form
/// must evaluate identically under the reference.
#[test]
fn double_equals_normalizes_to_feel_equals() {
    let docs = documents();
    let mut checked = 0;
    for p in PATHS {
        for l in LITERALS {
            for d in &docs {
                let ours = rbpmn(&format!("{p} == {l}"), d);
                let theirs = dsntk(&format!("{p} = {l}"), d);
                if let (Some(o), Some(t)) = (ours.taken(), theirs.taken()) {
                    checked += 1;
                    assert_eq!(o, t, "`{p} == {l}` on {d}: ours={o} dsntk={t}");
                }
            }
        }
    }
    println!("`==` normalization: {checked} comparisons, no divergence");
}

/// Our extra strictness is real and intentional: ordering operators require a
/// number literal. Guard it so the subset does not silently widen.
#[test]
fn ordering_against_non_numbers_is_refused_by_us_only() {
    let vars = json!({ "a": 1 });
    for op in ["<", "<=", ">", ">="] {
        for lit in ["\"one\"", "true", "null"] {
            let src = format!("a {op} {lit}");
            assert!(
                matches!(rbpmn(&src, &vars), Answer::Refused(_)),
                "`{src}` should be refused by the subset"
            );
            assert!(
                !matches!(dsntk(&src, &vars), Answer::Refused(_)),
                "`{src}` is expected to be valid FEEL — if dsntk now refuses it, \
                 our strictness is no longer a strict subset but a difference"
            );
        }
    }
}
