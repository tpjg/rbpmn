//! DMN fixture corpus runner — the same contract as the BPMN corpus
//! (`rbpmn-model/tests/fixtures.rs`), so a diagnostic means the same thing on
//! both sides of a deployment.
//!
//! Every `tests/fixtures/**/*.dmn` declares its expected diagnostics in a
//! leading XML comment:
//!
//! ```xml
//! <!-- expect-diagnostics:
//!   error feel-deterministic @ Discount
//! -->
//! ```
//!
//! The runner asserts exactly that set (severity, rule, element — messages
//! are free to evolve, ids are not). Fixtures under `reject/` must produce at
//! least one error; fixtures under `accept/` none.

use rbpmn_model::has_errors;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Expected {
    severity: String,
    rule: String,
    element: String,
}

fn parse_expectations(xml: &str, path: &Path) -> Vec<Expected> {
    let Some(start) = xml.find("expect-diagnostics:") else {
        panic!(
            "{} has no `expect-diagnostics:` comment — every fixture must declare its expected diagnostics",
            path.display()
        );
    };
    let rest = &xml[start + "expect-diagnostics:".len()..];
    let end = rest.find("-->").unwrap_or_else(|| {
        panic!(
            "{}: unterminated expect-diagnostics comment",
            path.display()
        )
    });
    rest[..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            match parts.as_slice() {
                [severity, rule, "@", element] => Expected {
                    severity: severity.to_string(),
                    rule: rule.to_string(),
                    element: element.to_string(),
                },
                _ => panic!(
                    "{}: bad expectation line '{line}' (format: `<severity> <rule> @ <element>`)",
                    path.display()
                ),
            }
        })
        .collect()
}

fn fixture_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "dmn"))
        .collect();
    files.sort();
    files
}

#[test]
fn fixture_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut failures = String::new();
    let mut total = 0;

    for dir in ["accept", "reject"] {
        for path in fixture_files(&root.join(dir)) {
            total += 1;
            let xml = fs::read_to_string(&path).unwrap();
            let mut expected = parse_expectations(&xml, &path);
            expected.sort();

            let check = rbpmn_dmn::check(&[xml]);
            let mut actual: Vec<Expected> = check
                .diagnostics
                .iter()
                .map(|d| Expected {
                    severity: d.severity.to_string(),
                    rule: d.rule.clone(),
                    element: d.element.clone(),
                })
                .collect();
            actual.sort();

            if actual != expected {
                writeln!(failures, "{}: diagnostics mismatch", path.display()).unwrap();
                writeln!(failures, "  expected:").unwrap();
                for e in &expected {
                    writeln!(failures, "    {} {} @ {}", e.severity, e.rule, e.element).unwrap();
                }
                writeln!(failures, "  actual:").unwrap();
                for d in &check.diagnostics {
                    writeln!(failures, "    {d}").unwrap();
                }
            }

            let errors = has_errors(&check.diagnostics);
            if dir == "accept" {
                if errors {
                    writeln!(
                        failures,
                        "{}: accept fixture produced errors",
                        path.display()
                    )
                    .unwrap();
                }
                // An accepted artifact must also be *usable*: if it compiled,
                // it exposes something a business-rule task can bind to.
                // Without this an empty document would sail through.
                if !errors && check.invocables.is_empty() {
                    writeln!(
                        failures,
                        "{}: accepted but exposes no invocable — nothing could bind to it",
                        path.display()
                    )
                    .unwrap();
                }
            }
            if dir == "reject" && !errors {
                writeln!(
                    failures,
                    "{}: reject fixture produced no errors — it would deploy",
                    path.display()
                )
                .unwrap();
            }
        }
    }

    assert!(
        total >= 15,
        "fixture corpus unexpectedly small: {total} files"
    );
    assert!(failures.is_empty(), "\n{failures}");
}

/// Every requirement an accepted fixture declares must carry diagram
/// interchange, and this is not a cosmetic rule.
///
/// dmn-js imports a requirement's *semantics* from `informationRequirement`
/// but draws only what has a `DMNEdge`, so a fixture without one renders its
/// inputs floating free of the decision — a picture saying the opposite of the
/// model. That is bad enough in a document whose whole subject is that inputs
/// must be declared.
///
/// The reason it is a test rather than a style note is what happens next.
/// diagram-js's `ReplaceShapeHandler` re-attaches a shape's *rendered*
/// incoming connections, so morphing a decision — table to literal expression,
/// through the context pad — silently dropped every undrawn requirement from
/// the XML. The decision then answered its else branch forever, with nothing
/// to see. `09-demo-triage.dmn` shipped that way, and it is the fixture the
/// editor opens on.
///
/// Deliberately crude: `informationRequirement` is what binds a name, so it is
/// what gets counted. Text is enough — the semantics are already checked
/// above, and this is a question about the file, not about the model.
#[test]
fn every_declared_requirement_is_also_drawn() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut failures = String::new();
    for dir in ["accept", "reject"] {
        for path in fixture_files(&root.join(dir)) {
            let xml = fs::read_to_string(&path).unwrap();
            let declared = xml.matches("<informationRequirement").count();
            // A fixture with no DI at all is a semantics-only fixture and not
            // this test's business; one that draws *something* has taken on
            // the job of being renderable.
            if declared == 0 || !xml.contains("DMNShape") {
                continue;
            }
            let drawn = xml.matches("<dmndi:DMNEdge").count();
            if drawn < declared {
                writeln!(
                    failures,
                    "{}: {declared} information requirement(s), {drawn} DMNEdge(s) — \
                     the undrawn ones vanish when the decision is morphed",
                    path.display()
                )
                .unwrap();
            }
        }
    }
    assert!(failures.is_empty(), "\n{failures}");
}
