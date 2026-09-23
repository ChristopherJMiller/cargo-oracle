//! Slice v3: which tests would actually *fail* if a symbol broke.
//!
//! This is the slice the other three exist to reach. Coverage says a symbol
//! ran; attribution says which test ran it; only mutation says whether anything
//! would notice it breaking.
//!
//! # Why body replacement is the right operator here
//!
//! `cargo-mutants` replaces a function body with a type-appropriate default
//! (`Ok(Default::default())`, `()`, `""`, `0`). That is *extreme mutation* — the
//! same operator Descartes implements for Java to find pseudo-tested methods —
//! and it is the default behaviour of the mainstream Rust tool rather than a
//! bolt-on. The logic is that if no test notices the entire body vanishing, no
//! test will notice a subtler fault either.
//!
//! # The cost, stated plainly
//!
//! PIT mutates JVM bytecode in memory and runs thousands of mutants a minute.
//! cargo-mutants must **recompile for every mutant**, so build and link dominate
//! and test execution is the cheap half. This is why `--in-diff` is not an
//! optimization here but the only practical way to run the slice on anything
//! large.
//!
//! # Attribution caveat
//!
//! nextest cancels the run on the first failure, so the log names *a* test that
//! killed the mutant, not every test that would have. `verified by X` should be
//! read as "X is sufficient", never "X is the only one". Passing
//! `--no-fail-fast` through to the test tool lifts this at proportional cost.

use crate::inventory::{Inventory, TestId};
use crate::symbol::SymbolId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// What happened when one mutant was tested.
pub enum MutantOutcome {
    /// A test failed. The symbol is verified against this mutation.
    Caught,
    /// Every test still passed with the body destroyed.
    Missed,
    /// The mutant did not compile. Not a gap and not a pass — see
    /// [`Verification::NoViableMutant`].
    Unviable,
    /// The suite hung, usually because the mutation removed a loop's exit.
    Timeout,
    /// cargo-mutants could not evaluate it.
    Failure,
}

impl MutantOutcome {
    fn from_summary(summary: &str) -> Option<Self> {
        match summary {
            "CaughtMutant" => Some(MutantOutcome::Caught),
            "MissedMutant" => Some(MutantOutcome::Missed),
            "Unviable" => Some(MutantOutcome::Unviable),
            "Timeout" => Some(MutantOutcome::Timeout),
            "Failure" => Some(MutantOutcome::Failure),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One mutation cargo-mutants generated, and what came of it.
pub struct Mutant {
    /// Source file, relative to the workspace root.
    pub file: String,
    /// As cargo-mutants names it, e.g. `Config::set_retries`.
    pub function_name: String,
    /// Start of the *replaced span*, which lies inside the function body. This
    /// is what we join on: it falls within the inventory's `LineSpan` for the
    /// enclosing symbol, the same containment used for coverage.
    pub line: u32,
    /// 1-based column of the replaced span.
    pub column: u32,
    /// What the body or operator was replaced with, e.g. `Ok(Default::default())`.
    pub replacement: String,
    /// cargo-mutants' own category for the mutation, e.g. `FnValue`.
    pub genre: String,
    /// Whether a test caught it.
    pub outcome: MutantOutcome,
    /// Tests observed failing in this mutant's log.
    pub killed_by: Vec<String>,
}

/// What the evidence supports for one symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    /// At least one mutant was caught: some test fails when this breaks.
    Verified,
    /// Viable mutants existed and every one survived. Covered, and unverified.
    PseudoTested,
    /// Every mutant failed to compile, so body replacement cannot score this.
    /// Part type-enforcement, part operator weakness — see design.md.
    NoViableMutant,
    /// cargo-mutants was in this file and generated nothing for this symbol.
    /// `-> Self` on a constructor is the common case: body replacement has no
    /// default to substitute. Unscorable, not unverified.
    NotMutated,
    /// The run never covered this file at all -- scoped with --file or
    /// --in-diff. Says nothing whatsoever about the symbol, and must never be
    /// confused with a symbol the run examined and found nothing for.
    OutOfScope,
}

impl Verification {
    /// Short label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Verification::Verified => "verified",
            Verification::PseudoTested => "PSEUDO-TESTED",
            Verification::NoViableMutant => "no viable mutant",
            Verification::NotMutated => "not mutated",
            Verification::OutOfScope => "out of scope",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Mutation evidence accumulated for one symbol.
pub struct SymbolVerdict {
    /// Mutants that some test caught.
    pub caught: usize,
    /// Mutants that survived: nothing noticed the change.
    pub missed: usize,
    /// Mutants that did not compile, so could not be scored either way.
    pub unviable: usize,
    /// Timeouts and evaluation failures.
    pub other: usize,
    /// Tests seen killing a mutant of this symbol. Sufficient, not exhaustive.
    pub killed_by: BTreeSet<TestId>,
    /// Surviving mutants, for a report that can say *what* went unnoticed.
    pub survivors: Vec<String>,
}

impl SymbolVerdict {
    /// What this evidence supports.
    ///
    /// One caught mutant is enough to call a symbol verified; it does not mean
    /// every mutant was caught, so [`SymbolVerdict::missed`] is still worth
    /// reporting alongside.
    ///
    /// ```
    /// use oracle_core::mutation::{SymbolVerdict, Verification};
    ///
    /// // One caught mutant is enough, even alongside survivors.
    /// let mixed = SymbolVerdict { caught: 1, missed: 3, ..Default::default() };
    /// assert_eq!(mixed.verification(), Verification::Verified);
    ///
    /// // Viable mutants that all survive: covered, and unverified.
    /// let weak = SymbolVerdict { missed: 2, ..Default::default() };
    /// assert_eq!(weak.verification(), Verification::PseudoTested);
    ///
    /// // A mutant that would not compile is not evidence of a missing test.
    /// let uncompilable = SymbolVerdict { unviable: 2, ..Default::default() };
    /// assert_eq!(uncompilable.verification(), Verification::NoViableMutant);
    /// ```
    pub fn verification(&self) -> Verification {
        if self.caught > 0 {
            Verification::Verified
        } else if self.missed > 0 {
            Verification::PseudoTested
        } else if self.unviable > 0 {
            Verification::NoViableMutant
        } else {
            Verification::NotMutated
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Mutation evidence joined onto an inventory.
pub struct MutationMap {
    /// Per-symbol evidence, for symbols the run examined.
    pub verdicts: BTreeMap<SymbolId, SymbolVerdict>,
    /// Every mutant cargo-mutants evaluated, including those landing on
    /// symbols the inventory does not score. The per-symbol counts below cover
    /// only scorable symbols, so the two differ and the report says both.
    pub total_mutants: usize,
    /// Mutants that fell in no inventory span — macro bodies, or code the
    /// inventory skipped. Reported rather than dropped.
    pub unattributed: Vec<Mutant>,
    /// Killer names from logs that matched no inventoried test.
    pub unresolved_killers: BTreeSet<String>,
    /// Files the run generated at least one mutant for. A symbol outside these
    /// was not examined, which is a different statement from finding nothing.
    pub mutated_files: BTreeSet<String>,
}

impl MutationMap {
    /// Place every mutant onto the inventory by span containment.
    pub fn build(inv: &Inventory, mutants: &[Mutant]) -> Self {
        let mut map = MutationMap {
            total_mutants: mutants.len(),
            ..Default::default()
        };

        let mut by_file: BTreeMap<&str, Vec<&crate::Symbol>> = BTreeMap::new();
        for symbol in &inv.symbols {
            by_file
                .entry(symbol.id.file.as_str())
                .or_default()
                .push(symbol);
        }

        for mutant in mutants {
            map.mutated_files.insert(mutant.file.clone());
        }

        for mutant in mutants {
            // Innermost containing span wins, so a mutant inside a closure is
            // attributed to the function that defines the closure.
            let owner = by_file.get(mutant.file.as_str()).and_then(|candidates| {
                candidates
                    .iter()
                    .filter(|s| s.span.contains(mutant.line))
                    .min_by_key(|s| s.span.end.saturating_sub(s.span.start))
            });

            let Some(symbol) = owner else {
                map.unattributed.push(mutant.clone());
                continue;
            };

            let verdict = map.verdicts.entry(symbol.id.clone()).or_default();
            match mutant.outcome {
                MutantOutcome::Caught => {
                    verdict.caught += 1;
                    for name in &mutant.killed_by {
                        match resolve_test(inv, name) {
                            Some(id) => {
                                verdict.killed_by.insert(id);
                            }
                            None => {
                                map.unresolved_killers.insert(name.clone());
                            }
                        }
                    }
                }
                MutantOutcome::Missed => {
                    verdict.missed += 1;
                    verdict.survivors.push(mutant.replacement.clone());
                }
                MutantOutcome::Unviable => verdict.unviable += 1,
                _ => verdict.other += 1,
            }
        }

        map
    }

    /// Evidence for one symbol, defaulting to empty when the run never saw it.
    pub fn verdict(&self, id: &SymbolId) -> SymbolVerdict {
        self.verdicts.get(id).cloned().unwrap_or_default()
    }

    /// The verdict for a symbol, given what the run actually examined.
    ///
    /// `file` decides between "examined and produced nothing" and "never
    /// looked at", which a scoped run makes the common case.
    pub fn verification_in(&self, id: &SymbolId, file: &str) -> Verification {
        match self.verdicts.get(id) {
            Some(verdict) => verdict.verification(),
            None if self.mutated_files.contains(file) => Verification::NotMutated,
            None => Verification::OutOfScope,
        }
    }

    /// Verdict for a symbol, ignoring whether the run examined its file.
    ///
    /// Prefer [`MutationMap::verification_in`], which distinguishes "examined and
    /// found nothing" from "never looked".
    pub fn verification(&self, id: &SymbolId) -> Verification {
        self.verdicts
            .get(id)
            .map(|v| v.verification())
            .unwrap_or(Verification::NotMutated)
    }
}

/// Match a test name from a log against the inventory, the same suffix rule the
/// attribution slice uses.
fn resolve_test(inv: &Inventory, name: &str) -> Option<TestId> {
    let suffix = format!("::{name}");
    let mut found = inv
        .tests
        .iter()
        .map(|t| &t.id)
        .filter(|id| id.path.ends_with(&suffix) || id.path == name);
    let first = found.next()?;
    // Ambiguous names are dropped rather than guessed.
    match found.next() {
        Some(_) => None,
        None => Some(first.clone()),
    }
}

// ---------------------------------------------------------------------------
// Reading cargo-mutants output
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawOutcomes {
    outcomes: Vec<RawOutcome>,
}

#[derive(Deserialize)]
struct RawOutcome {
    summary: String,
    /// Either the string `"Baseline"` or `{ "Mutant": { .. } }`.
    scenario: serde_json::Value,
    log_path: Option<String>,
}

#[derive(Deserialize)]
struct RawMutant {
    file: String,
    #[serde(default)]
    replacement: String,
    #[serde(default)]
    genre: String,
    function: Option<RawFunction>,
    span: RawSpan,
}

#[derive(Deserialize)]
struct RawFunction {
    function_name: String,
}

#[derive(Deserialize)]
struct RawSpan {
    start: RawPos,
}

#[derive(Deserialize)]
struct RawPos {
    line: u32,
    column: u32,
}

/// Read `mutants.out/outcomes.json` and the per-mutant logs it points at.
pub fn load(out_dir: &Path) -> Result<Vec<Mutant>> {
    let outcomes_path = out_dir.join("outcomes.json");
    let text = std::fs::read_to_string(&outcomes_path)
        .with_context(|| format!("reading {}", outcomes_path.display()))?;
    let raw: RawOutcomes =
        serde_json::from_str(&text).context("parsing cargo-mutants outcomes.json")?;

    let mut mutants = Vec::new();
    for outcome in raw.outcomes {
        // The baseline run is not a mutant.
        let Some(value) = outcome.scenario.get("Mutant") else {
            continue;
        };
        let Some(kind) = MutantOutcome::from_summary(&outcome.summary) else {
            continue;
        };
        let raw_mutant: RawMutant = serde_json::from_value(value.clone())
            .context("parsing a mutant scenario from outcomes.json")?;

        let killed_by = if kind == MutantOutcome::Caught {
            outcome
                .log_path
                .as_ref()
                .map(|p| out_dir.join(p))
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|log| killers_from_log(&log))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        mutants.push(Mutant {
            file: raw_mutant.file,
            function_name: raw_mutant
                .function
                .map(|f| f.function_name)
                .unwrap_or_default(),
            line: raw_mutant.span.start.line,
            column: raw_mutant.span.start.column,
            replacement: raw_mutant.replacement,
            genre: raw_mutant.genre,
            outcome: kind,
            killed_by,
        });
    }

    Ok(mutants)
}

/// Extract failing test names from a mutant's test-run log.
///
/// Handles nextest's `FAIL [ 0.00s] (1/4) <binary> <test>` summary lines and
/// libtest's `test <name> ... FAILED`, falling back to the panicking thread
/// name, which libtest and nextest both set to the test path.
///
/// ```
/// use oracle_core::mutation::killers_from_log;
///
/// let log = "    Summary [ 0.01s] 4 tests run: 3 passed, 1 failed\n\
///            FAIL [ 0.00s] (4/4) weak-suite config::tests::parses_host";
/// assert_eq!(killers_from_log(log), vec!["config::tests::parses_host"]);
///
/// // A log with no failure names nobody, rather than guessing.
/// assert!(killers_from_log("3 passed").is_empty());
/// ```
pub fn killers_from_log(log: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    for line in log.lines() {
        let line = line.trim();

        if line.starts_with("FAIL ") || line.starts_with("TRY 1 FAIL") {
            if let Some(name) = line.split_whitespace().next_back() {
                if name.contains("::") && !found.iter().any(|f| f == name) {
                    found.push(name.to_string());
                }
            }
        }

        if let Some(rest) = line.strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                if !found.iter().any(|f| f == name) {
                    found.push(name.to_string());
                }
            }
        }
    }

    if found.is_empty() {
        for line in log.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("thread '") {
                if let Some((name, _)) = rest.split_once('\'') {
                    if name.contains("::") {
                        found.push(name.to_string());
                        break;
                    }
                }
            }
        }
    }

    found
}

/// Interpret a cargo-mutants exit status.
///
/// The codes are easy to get backwards, and getting them backwards is
/// expensive in both directions: treating 2 as fatal aborts every run that
/// finds something, and treating 4 as success reports a whole crate as
/// unverified when really the baseline suite was already red.
///
/// | Code | Meaning | Ours |
/// |------|---------|------|
/// | 0 | every viable mutant caught | result |
/// | 2 | some mutants survived | result -- the interesting case |
/// | 3 | some tests timed out | result, with a warning |
/// | 1 | usage error | error |
/// | 4 | baseline already failing, nothing was tested | error |
/// | 5, 6 | `--in-diff` does not apply to this tree | error |
/// | 70 | internal error | error |
pub fn classify_exit(code: Option<i32>) -> Result<()> {
    match code {
        Some(0) | Some(2) => Ok(()),
        Some(3) => {
            eprintln!(
                "warning: some mutants timed out; they are reported as neither caught nor missed"
            );
            Ok(())
        }
        Some(1) => bail!("`cargo mutants` usage error -- check the arguments passed after `--`"),
        Some(4) => bail!(
            "the test suite is already failing before any mutation, so nothing was tested. \
             Fix the baseline first: `cargo nextest run`"
        ),
        Some(5) => bail!("the `--in-diff` diff does not match the working tree"),
        Some(6) => bail!("the `--in-diff` file is not a valid diff"),
        Some(70) => bail!("`cargo mutants` hit an internal error; see mutants.out/debug.log"),
        Some(other) => bail!("`cargo mutants` exited with an unrecognized code {other}"),
        None => bail!("`cargo mutants` was terminated by a signal"),
    }
}

/// Run cargo-mutants, returning the output directory it wrote.
pub fn run_mutants(manifest_dir: &Path, extra_args: &[String]) -> Result<PathBuf> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(manifest_dir)
        .args(["mutants", "--test-tool", "nextest"])
        .args(extra_args);

    let status = cmd.status().context(
        "running `cargo mutants` (install it with `cargo install cargo-mutants`, \
         or enter the nix dev shell)",
    )?;

    classify_exit(status.code())?;

    Ok(manifest_dir.join("mutants.out"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEXTEST_LOG: &str = r#"
    test result: FAILED. 0 passed; 1 failed; 0 ignored
  stderr
    thread 'config::tests::parse_extracts_host_and_port' (172703) panicked at src/config.rs:77:9:
    assertion `left == right` failed
     Summary [   0.012s] 4 tests run: 3 passed, 1 failed, 0 skipped
        FAIL [   0.006s] (4/4) weak-suite config::tests::parse_extracts_host_and_port
error: test run failed
"#;

    #[test]
    fn the_nextest_fail_line_names_the_killing_test() {
        let killers = killers_from_log(NEXTEST_LOG);
        assert_eq!(killers, vec!["config::tests::parse_extracts_host_and_port"]);
    }

    #[test]
    fn the_libtest_failure_format_is_also_understood() {
        let log =
            "running 2 tests\ntest config::tests::rejects_empty ... FAILED\ntest ok_one ... ok\n";
        assert_eq!(killers_from_log(log), vec!["config::tests::rejects_empty"]);
    }

    #[test]
    fn a_panicking_thread_name_is_the_fallback_when_no_summary_is_present() {
        let log = "thread 'mycrate::tests::boom' panicked at src/lib.rs:1:1:\nboom\n";
        assert_eq!(killers_from_log(log), vec!["mycrate::tests::boom"]);
    }

    #[test]
    fn a_log_with_no_failure_names_nobody() {
        assert!(killers_from_log("all good\n3 passed\n").is_empty());
    }

    #[test]
    fn surviving_mutants_are_a_result_not_a_failure() {
        // Code 2 is what a run that finds something returns. Treating it as
        // fatal would abort exactly the runs worth reading.
        assert!(classify_exit(Some(2)).is_ok());
        assert!(classify_exit(Some(0)).is_ok());
        assert!(
            classify_exit(Some(3)).is_ok(),
            "timeouts are reported, not fatal"
        );
    }

    #[test]
    fn a_failing_baseline_is_an_error_not_an_unverified_crate() {
        // Code 4 means nothing was mutated at all. Reporting it as success
        // would mark every symbol in the crate unverified on the strength of
        // an unrelated red test.
        let err = classify_exit(Some(4)).unwrap_err().to_string();
        assert!(err.contains("already failing"), "unhelpful: {err}");
        assert!(
            err.contains("nextest run"),
            "should say how to fix it: {err}"
        );
    }

    #[test]
    fn usage_and_diff_errors_are_distinguished_from_results() {
        assert!(classify_exit(Some(1))
            .unwrap_err()
            .to_string()
            .contains("usage"));
        assert!(classify_exit(Some(5))
            .unwrap_err()
            .to_string()
            .contains("working tree"));
        assert!(classify_exit(Some(6))
            .unwrap_err()
            .to_string()
            .contains("valid diff"));
        assert!(classify_exit(None)
            .unwrap_err()
            .to_string()
            .contains("signal"));
    }

    #[test]
    fn a_symbol_in_an_unexamined_file_is_out_of_scope_not_unscorable() {
        let mut map = MutationMap::default();
        map.mutated_files.insert("src/claims.rs".into());

        let examined = SymbolId::new("src/claims.rs", 10, 0);
        let untouched = SymbolId::new("src/lint.rs", 10, 0);

        // Same absence of a verdict, two very different meanings: a scoped run
        // must not report the rest of the workspace as having no mutant.
        assert_eq!(
            map.verification_in(&examined, "src/claims.rs"),
            Verification::NotMutated
        );
        assert_eq!(
            map.verification_in(&untouched, "src/lint.rs"),
            Verification::OutOfScope
        );
    }

    #[test]
    fn a_caught_mutant_outranks_survivors_in_the_verdict() {
        let verdict = SymbolVerdict {
            caught: 1,
            missed: 3,
            ..Default::default()
        };
        assert_eq!(
            verdict.verification(),
            Verification::Verified,
            "one test that fails is enough to call the symbol verified"
        );
    }

    #[test]
    fn viable_mutants_that_all_survive_are_pseudo_tested() {
        let verdict = SymbolVerdict {
            missed: 2,
            ..Default::default()
        };
        assert_eq!(verdict.verification(), Verification::PseudoTested);
    }

    #[test]
    fn only_unviable_mutants_is_distinct_from_being_unverified() {
        let verdict = SymbolVerdict {
            unviable: 2,
            ..Default::default()
        };
        assert_eq!(
            verdict.verification(),
            Verification::NoViableMutant,
            "a mutant that would not compile is not evidence of a missing test"
        );
    }

    #[test]
    fn no_mutants_at_all_is_its_own_state() {
        assert_eq!(
            SymbolVerdict::default().verification(),
            Verification::NotMutated
        );
    }
}

// ---------------------------------------------------------------------------
// Diff scoping
// ---------------------------------------------------------------------------

/// Produce a diff of the working tree against `base`, for `--in-diff`.
///
/// cargo-mutants requires the *new* side of the diff to match the tree it is
/// mutating, which rules out `git diff base..head` on a clean checkout of some
/// other commit. `git diff <base>` compares the working tree to `base`, so the
/// new side is by construction what is on disk.
///
/// # Errors
///
/// Fails if `git` is unavailable, or if `base` is not a commit this repository
/// knows about.
pub fn diff_since(repo: &Path, base: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--no-color", "--no-ext-diff", base, "--"])
        .output()
        .context("running `git diff` (is git installed and is this a repository?)")?;

    if !output.status.success() {
        bail!(
            "`git diff {base}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("`git diff` produced non-UTF-8 output")
}

/// Which files a unified diff touches, as paths relative to the repository root.
///
/// Used to tell "the diff changed nothing we can mutate" from "the diff changed
/// Rust code but every mutant survived" — two very different reports.
///
/// ```
/// use oracle_core::mutation::files_in_diff;
///
/// let diff = "\
/// diff --git a/src/lib.rs b/src/lib.rs
/// index 1234567..89abcde 100644
/// --- a/src/lib.rs
/// +++ b/src/lib.rs
/// @@ -1 +1 @@
/// -old
/// +new
/// ";
/// assert_eq!(files_in_diff(diff), vec!["src/lib.rs"]);
/// ```
pub fn files_in_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("+++ ") else {
            continue;
        };
        let path = rest.trim();
        // `/dev/null` is a deletion: there is no new side to mutate.
        if path == "/dev/null" {
            continue;
        }
        let path = path.strip_prefix("b/").unwrap_or(path);
        let path = path.split('\t').next().unwrap_or(path).to_string();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
}

/// Whether a diff touches any Rust source at all.
///
/// ```
/// use oracle_core::mutation::diff_touches_rust;
///
/// assert!(diff_touches_rust("+++ b/src/lib.rs\n"));
/// assert!(!diff_touches_rust("+++ b/README.md\n"));
/// assert!(!diff_touches_rust(""), "an empty diff touches nothing");
/// ```
pub fn diff_touches_rust(diff: &str) -> bool {
    files_in_diff(diff).iter().any(|f| f.ends_with(".rs"))
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/config.rs b/src/config.rs
--- a/src/config.rs
+++ b/src/config.rs
@@ -1,3 +1,3 @@
-old
+new
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-a
+b
diff --git a/src/gone.rs b/src/gone.rs
--- a/src/gone.rs
+++ /dev/null
";

    #[test]
    fn every_changed_file_is_listed_once_without_the_b_prefix() {
        assert_eq!(files_in_diff(DIFF), vec!["src/config.rs", "README.md"]);
    }

    #[test]
    fn a_deleted_file_has_no_new_side_to_mutate() {
        assert!(
            !files_in_diff(DIFF).contains(&"src/gone.rs".to_string()),
            "a file deleted in the diff cannot be mutated"
        );
    }

    #[test]
    fn a_docs_only_diff_is_distinguished_from_a_code_diff() {
        let docs_only = "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(!diff_touches_rust(docs_only));
        assert!(diff_touches_rust(DIFF));
    }
}
