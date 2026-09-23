//! Slice v2: which *test* runs which symbol.
//!
//! v1 answers "does anything execute this symbol". That is a bit per symbol,
//! and a bit is not enough to say anything about an individual test. The edge
//! `executes(test, symbol)` is what makes the per-test verdict possible, and it
//! is the prerequisite for v3 attributing a mutation kill to the test that
//! caught it.
//!
//! # How the per-test profile is obtained
//!
//! `cargo-nextest` runs every test in its own process, so a profile can be
//! isolated per test without any cooperation from the test harness. The loop is:
//!
//! ```text
//! for each test T:
//!     cargo llvm-cov clean --profraw-only --workspace
//!     cargo llvm-cov nextest --no-report -E 'test(=T)'
//!     cargo llvm-cov report --json
//! ```
//!
//! The build is shared, so only the run, the `llvm-profdata` merge and the
//! export repeat. That is still **O(tests)** and the dominant cost of this
//! slice: it is a nightly job on a large suite, not a pre-commit hook. The
//! intended fast path is to scope it to the tests touched by a diff, which
//! bounds the cost by the size of the change rather than the size of the suite.

use crate::coverage::{CoverageData, CoverageMap};
use crate::inventory::{Inventory, TestId};
use crate::symbol::SymbolId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// A test as `cargo nextest` addresses it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NextestId {
    /// e.g. `oracle-core` for a lib test, `oracle-core::pipeline` for an
    /// integration test binary.
    pub binary_id: String,
    /// e.g. `config::tests::test_parse`, or a bare function name in an
    /// integration test.
    pub testcase: String,
}

impl NextestId {
    /// The filter expression that selects exactly this test.
    pub fn filter(&self) -> String {
        format!("test(={})", self.testcase)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AttributionMap {
    /// symbol -> the tests that execute it.
    pub executed_by: BTreeMap<SymbolId, BTreeSet<TestId>>,
    /// test -> the symbols it executes.
    pub executes: BTreeMap<TestId, BTreeSet<SymbolId>>,
    /// nextest test names that matched no inventoried test. Reported rather
    /// than dropped: a mismatch here silently hollows out the whole map.
    pub unmatched_tests: Vec<String>,
    /// nextest names that matched more than one inventoried test, and so were
    /// skipped rather than guessed at.
    pub ambiguous_tests: Vec<String>,
}

impl AttributionMap {
    pub fn tests_for(&self, symbol: &SymbolId) -> Option<&BTreeSet<TestId>> {
        self.executed_by.get(symbol)
    }

    /// Symbols this test runs. The denominator for "and verifies how many?"
    pub fn symbols_for(&self, test: &TestId) -> Option<&BTreeSet<SymbolId>> {
        self.executes.get(test)
    }

    fn record(&mut self, test: &TestId, symbols: BTreeSet<SymbolId>) {
        for symbol in &symbols {
            self.executed_by
                .entry(symbol.clone())
                .or_default()
                .insert(test.clone());
        }
        self.executes.insert(test.clone(), symbols);
    }
}

/// Ask nextest what tests exist, without running them.
pub fn list_tests(manifest_dir: &Path) -> Result<Vec<NextestId>> {
    let output = Command::new("cargo")
        .current_dir(manifest_dir)
        .args(["nextest", "list", "--message-format", "json"])
        .output()
        .context("running `cargo nextest list` (install cargo-nextest, or use the nix shell)")?;

    if !output.status.success() {
        bail!(
            "`cargo nextest list` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(Deserialize)]
    struct Listing {
        #[serde(rename = "rust-suites")]
        rust_suites: BTreeMap<String, Suite>,
    }
    #[derive(Deserialize)]
    struct Suite {
        #[serde(rename = "binary-id")]
        binary_id: String,
        testcases: BTreeMap<String, serde_json::Value>,
    }

    let listing: Listing = serde_json::from_slice(&output.stdout)
        .context("parsing `cargo nextest list --message-format json`")?;

    let mut tests = Vec::new();
    for suite in listing.rust_suites.values() {
        for testcase in suite.testcases.keys() {
            tests.push(NextestId {
                binary_id: suite.binary_id.clone(),
                testcase: testcase.clone(),
            });
        }
    }
    tests.sort();
    Ok(tests)
}

/// Run one test under instrumentation and export only its coverage.
///
/// A failing test is not an error here: it still executed code, and its
/// attribution is still wanted.
pub fn profile_one(
    manifest_dir: &Path,
    test: &NextestId,
    scratch: &Path,
    workspace_root: &Path,
) -> Result<CoverageData> {
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Command::new("cargo")
            .current_dir(manifest_dir)
            .args(args)
            .output()
            .with_context(|| format!("running `cargo {}`", args.join(" ")))
    };

    // Discard the previous test's profile so the export sees only this one.
    run(&["llvm-cov", "clean", "--profraw-only", "--workspace"])?;

    let filter = test.filter();
    let _ = run(&["llvm-cov", "nextest", "--no-report", "-E", &filter])?;

    std::fs::create_dir_all(scratch)?;
    let out = scratch.join("per-test.json");
    let out_str = out.to_string_lossy().into_owned();
    let report = run(&[
        "llvm-cov",
        "report",
        "--json",
        "--output-path",
        out_str.as_str(),
    ])?;
    if !report.status.success() {
        bail!(
            "`cargo llvm-cov report` failed for {}:\n{}",
            test.testcase,
            String::from_utf8_lossy(&report.stderr)
        );
    }

    crate::coverage::load(&out, workspace_root)
}

/// Resolve a nextest test name to an inventoried test.
///
/// nextest addresses a lib test as `config::tests::test_parse` while the
/// inventory writes `mycrate::config::tests::test_parse`, so the nextest name
/// is a suffix of ours. An integration test is a bare function name, which is
/// only unique within its binary — when a name resolves to more than one
/// inventoried test we record the ambiguity rather than guessing, because a
/// wrong edge here is worse than a missing one.
pub fn resolve(inv: &Inventory, test: &NextestId) -> Result<TestId, Ambiguity> {
    let suffix = format!("::{}", test.testcase);
    let matches: Vec<&TestId> = inv
        .tests
        .iter()
        .map(|t| &t.id)
        .filter(|id| id.path.ends_with(&suffix) || id.path == test.testcase)
        .collect();

    match matches.len() {
        0 => Err(Ambiguity::None),
        1 => Ok(matches[0].clone()),
        _ => Err(Ambiguity::Several(matches.len())),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ambiguity {
    None,
    Several(usize),
}

/// Profile every listed test and build the attribution map.
///
/// `progress` is called before each test so a caller can report on a run that
/// takes minutes.
pub fn build(
    inv: &Inventory,
    manifest_dir: &Path,
    scratch: &Path,
    tests: &[NextestId],
    mut progress: impl FnMut(usize, usize, &NextestId),
) -> Result<AttributionMap> {
    let workspace_root = Path::new(&inv.root);
    let mut map = AttributionMap::default();

    for (i, test) in tests.iter().enumerate() {
        progress(i + 1, tests.len(), test);

        let resolved = match resolve(inv, test) {
            Ok(id) => id,
            Err(Ambiguity::None) => {
                map.unmatched_tests.push(test.testcase.clone());
                continue;
            }
            Err(Ambiguity::Several(_)) => {
                map.ambiguous_tests.push(test.testcase.clone());
                continue;
            }
        };

        let data = profile_one(manifest_dir, test, scratch, workspace_root)?;
        let per_test = CoverageMap::join(inv, &data);

        let symbols: BTreeSet<SymbolId> = per_test
            .symbols
            .iter()
            .filter(|(_, cov)| cov.executed())
            .map(|(id, _)| id.clone())
            .collect();

        map.record(&resolved, symbols);
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{TestItem, TestKind};
    use crate::symbol::LineSpan;

    fn test_item(path: &str) -> TestItem {
        TestItem {
            id: TestId {
                file: "src/config.rs".into(),
                line: 1,
                path: path.into(),
            },
            kind: TestKind::Unit,
            span: LineSpan { start: 1, end: 9 },
            is_ignored: false,
            is_async: false,
            should_panic: None,
            enclosing_test_module: None,
            doctest_target: None,
            body: None,
        }
    }

    fn inv_with(paths: &[&str]) -> Inventory {
        Inventory {
            tests: paths.iter().map(|p| test_item(p)).collect(),
            ..Default::default()
        }
    }

    fn nextest(testcase: &str) -> NextestId {
        NextestId {
            binary_id: "weak-suite".into(),
            testcase: testcase.into(),
        }
    }

    #[test]
    fn a_nextest_name_resolves_as_a_suffix_of_the_inventory_path() {
        let inv = inv_with(&["weak_suite::config::tests::test_parse"]);
        let resolved = resolve(&inv, &nextest("config::tests::test_parse"))
            .expect("the nextest name is a suffix of the inventory path");
        assert_eq!(resolved.path, "weak_suite::config::tests::test_parse");
    }

    #[test]
    fn an_unknown_test_resolves_to_nothing_rather_than_a_near_miss() {
        let inv = inv_with(&["weak_suite::config::tests::test_parse"]);
        assert_eq!(
            resolve(&inv, &nextest("config::tests::test_absent")),
            Err(Ambiguity::None)
        );
    }

    #[test]
    fn a_name_matching_two_tests_is_recorded_as_ambiguous_not_guessed() {
        let inv = inv_with(&[
            "crate_a::tests::roundtrip",
            "crate_b::other::tests::roundtrip",
        ]);
        assert_eq!(
            resolve(&inv, &nextest("roundtrip")),
            Err(Ambiguity::Several(2)),
            "a wrong attribution edge is worse than a missing one"
        );
    }

    #[test]
    fn a_partial_segment_does_not_count_as_a_suffix_match() {
        // `test_parse` must not match `test_parse_extended`.
        let inv = inv_with(&["weak_suite::config::tests::test_parse_extended"]);
        assert_eq!(
            resolve(&inv, &nextest("config::tests::test_parse")),
            Err(Ambiguity::None)
        );
    }

    #[test]
    fn filter_selects_the_test_exactly() {
        assert_eq!(
            nextest("config::tests::test_parse").filter(),
            "test(=config::tests::test_parse)"
        );
    }
}
