//! Fixture corpus runner.
//!
//! Every `tests/fixtures/**/*.bpmn` declares its expected diagnostics in a
//! leading XML comment:
//!
//! ```xml
//! <!-- expect-diagnostics:
//!   error no-inclusive-gateway @ gateway_1
//!   warn  no-foreign-implementation @ task_2
//! -->
//! ```
//!
//! The runner asserts the linter produces exactly that set (severity, rule,
//! element — messages are free to evolve, ids are not). Fixtures under
//! `reject/` must produce at least one error; fixtures under `accept/` none.

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
        .filter(|p| p.extension().is_some_and(|ext| ext == "bpmn"))
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

            let checked = match rbpmn_model::check(&xml) {
                Ok(c) => c,
                Err(e) => {
                    writeln!(failures, "{}: XML did not parse: {e}", path.display()).unwrap();
                    continue;
                }
            };

            let mut actual: Vec<Expected> = checked
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
                for d in &checked.diagnostics {
                    writeln!(failures, "    {d}").unwrap();
                }
            }

            let has_errors = rbpmn_model::has_errors(&checked.diagnostics);
            if dir == "accept" && has_errors {
                writeln!(
                    failures,
                    "{}: accept fixture produced errors",
                    path.display()
                )
                .unwrap();
            }
            if dir == "reject" && !has_errors {
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
        total >= 40,
        "fixture corpus unexpectedly small: {total} files"
    );
    assert!(failures.is_empty(), "\n{failures}");
}
