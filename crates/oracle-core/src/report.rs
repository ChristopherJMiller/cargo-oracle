//! Assembling and rendering the audit.
//!
//! The v0 report is deliberately careful about what it claims. Everything here
//! is static: it can say a test's oracles *cannot* discriminate, and it can say
//! a symbol is claimed by nobody. It cannot say a symbol is verified — that
//! needs the mutation evidence of slice v3. Reports that blur the two are how
//! coverage numbers became untrustworthy in the first place.

use crate::claims::ClaimMap;
use crate::inventory::{Inventory, TestKind};
use crate::lint::{self, Finding, OracleStrength, Severity, TestOracles};
use crate::symbol::Triviality;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub files: usize,
    pub symbols_total: usize,
    pub symbols_scorable: usize,
    pub symbols_accessor: usize,
    pub symbols_unclaimed: usize,
    pub tests_total: usize,
    pub doctests: usize,
    pub strength: BTreeMap<OracleStrength, usize>,
    pub findings_high: usize,
    pub findings_medium: usize,
    pub findings_low: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub summary: Summary,
    pub tests: Vec<TestOracles>,
    pub findings: Vec<Finding>,
    /// Scorable symbols that no test claims, even weakly. A coverage proxy that
    /// needs no build: nothing nominally speaks for these at all.
    pub unclaimed: Vec<String>,
    pub parse_failures: BTreeMap<String, String>,
}

impl Report {
    pub fn build(inv: &Inventory) -> Self {
        let claims = ClaimMap::build(inv);
        let mut tests = lint::analyze(inv);

        // ORC010 is computed across the inventory and the claim map, so it is
        // not known when a test is analyzed alone. Fold it back into the
        // per-test view, which is the part of this report worth reading.
        let shape = lint::shape_mismatches(inv, &claims, &tests);
        for finding in &shape {
            if let Some(t) = tests.iter_mut().find(|t| t.test == finding.test) {
                t.findings.push(finding.clone());
            }
        }

        let mut findings: Vec<Finding> = tests.iter().flat_map(|t| t.findings.clone()).collect();

        findings.sort_by(|a, b| {
            b.rule
                .severity()
                .cmp(&a.rule.severity())
                .then_with(|| a.test.file.cmp(&b.test.file))
                .then_with(|| a.line.cmp(&b.line))
        });

        let mut strength: BTreeMap<OracleStrength, usize> = BTreeMap::new();
        for t in &tests {
            *strength.entry(t.strength).or_default() += 1;
        }

        let unclaimed: Vec<String> = claims
            .unclaimed(inv, false)
            .into_iter()
            .map(|s| s.path.clone())
            .collect();

        let count = |sev: Severity| findings.iter().filter(|f| f.rule.severity() == sev).count();

        let summary = Summary {
            files: inv.files.len(),
            symbols_total: inv.symbols.len(),
            symbols_scorable: inv.scorable().count(),
            symbols_accessor: inv
                .symbols
                .iter()
                .filter(|s| s.triviality == Triviality::Accessor)
                .count(),
            symbols_unclaimed: unclaimed.len(),
            tests_total: inv.tests.len(),
            doctests: inv
                .tests
                .iter()
                .filter(|t| t.kind == TestKind::Doctest)
                .count(),
            strength,
            findings_high: count(Severity::High),
            findings_medium: count(Severity::Medium),
            findings_low: count(Severity::Low),
        };

        Self {
            summary,
            tests,
            findings,
            unclaimed,
            parse_failures: inv.parse_failures.clone(),
        }
    }

    /// Findings at or above a severity floor.
    pub fn findings_at_least(&self, floor: Severity) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(move |f| f.rule.severity() >= floor)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report is serializable")
    }

    pub fn to_text(&self, verbose: bool) -> String {
        let mut out = String::new();
        let s = &self.summary;

        let _ = writeln!(
            out,
            "cargo-oracle  static audit (v0)\n  {} files, {} symbols ({} scorable, {} skipped), {} tests ({} doctests)\n",
            s.files,
            s.symbols_total,
            s.symbols_scorable,
            plural(s.symbols_accessor, "accessor"),
            s.tests_total,
            s.doctests
        );

        if !self.parse_failures.is_empty() {
            let _ = writeln!(out, "could not parse:");
            for (file, err) in &self.parse_failures {
                let _ = writeln!(out, "  {file}: {err}");
            }
            out.push('\n');
        }

        // Findings, grouped by the file they live in.
        if self.findings.is_empty() {
            let _ = writeln!(out, "no findings.\n");
        } else {
            let mut by_file: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
            for f in &self.findings {
                by_file.entry(f.test.file.as_str()).or_default().push(f);
            }
            for (file, group) in by_file {
                let _ = writeln!(out, "{file}");
                for f in group {
                    let sev = match f.rule.severity() {
                        Severity::High => "high",
                        Severity::Medium => "med ",
                        Severity::Low => "low ",
                    };
                    let _ = writeln!(
                        out,
                        "  {}:{:<5} {} {:<24} {}",
                        sev,
                        f.line,
                        f.rule.id(),
                        f.rule.name(),
                        leaf(&f.test.path)
                    );
                    if let Some(symbol) = &f.symbol {
                        let _ = writeln!(out, "         on {symbol}");
                    }
                    if !f.snippet.is_empty() {
                        let _ = writeln!(out, "         {}", truncate(&f.snippet, 96));
                    }
                    if verbose {
                        let _ = writeln!(out, "         {}", wrap(f.rule.why(), 9, 78));
                    }
                }
                out.push('\n');
            }
        }

        // Per-test verdict: the inversion that makes a weak new test visible
        // instead of averaging it away into a file-level number.
        let _ = writeln!(out, "per-test oracle strength");
        let mut tests: Vec<&TestOracles> = self.tests.iter().collect();
        tests.sort_by(|a, b| {
            a.strength
                .cmp(&b.strength)
                .then(a.test.path.cmp(&b.test.path))
        });
        for t in tests {
            if !verbose && t.strength >= OracleStrength::Partial && t.findings.is_empty() {
                continue;
            }
            let rules: Vec<&str> = t.findings.iter().map(|f| f.rule.id()).collect();
            let _ = writeln!(
                out,
                "  {:<8} {:<58} {}",
                format!("{:?}", t.strength).to_lowercase(),
                truncate(&t.test.path, 58),
                rules.join(" ")
            );
        }

        if !self.unclaimed.is_empty() {
            let _ = writeln!(
                out,
                "\nunclaimed symbols ({}) -- no test speaks for these at all",
                self.unclaimed.len()
            );
            for path in self
                .unclaimed
                .iter()
                .take(if verbose { usize::MAX } else { 15 })
            {
                let _ = writeln!(out, "  {path}");
            }
            if !verbose && self.unclaimed.len() > 15 {
                let _ = writeln!(out, "  ... and {} more", self.unclaimed.len() - 15);
            }
        }

        let strength_line: Vec<String> = [
            OracleStrength::Strong,
            OracleStrength::Partial,
            OracleStrength::Weak,
            OracleStrength::None,
        ]
        .iter()
        .map(|k| {
            format!(
                "{} {}",
                format!("{k:?}").to_lowercase(),
                s.strength.get(k).copied().unwrap_or(0)
            )
        })
        .collect();

        let _ = writeln!(
            out,
            "\nsummary\n  oracle strength   {}\n  findings          {} high, {} medium, {} low\n  unclaimed         {} of {} scorable symbols",
            strength_line.join("   "),
            s.findings_high,
            s.findings_medium,
            s.findings_low,
            s.symbols_unclaimed,
            s.symbols_scorable
        );

        let _ = writeln!(
            out,
            "\nthis is a static audit: it reports oracles that cannot discriminate and\nsymbols nobody claims. whether a symbol is actually *verified* needs the\nmutation evidence of `cargo oracle verify` (slice v3)."
        );

        out
    }
}

fn leaf(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}~")
}

fn wrap(text: &str, indent: usize, width: usize) -> String {
    let pad = " ".repeat(indent);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + word.len() + 1 > width - indent {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.join(&format!("\n{pad}"))
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

// ---------------------------------------------------------------------------
// Slice v1 rendering: execution state per symbol
// ---------------------------------------------------------------------------

use crate::coverage::CoverageMap;
use crate::symbol::Symbol;

/// What v1 can say about a symbol. `Verified` is deliberately absent: proving a
/// test would *fail* if the symbol broke needs the mutation evidence of v3, and
/// conflating "ran" with "checked" is how coverage numbers lost their meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionState {
    /// No test runs it.
    Unexecuted,
    /// A test claims it, but nothing runs it. The claim is unbacked — a
    /// stronger finding than plain `Unexecuted`, because someone believed
    /// otherwise.
    ClaimedButUnexecuted,
    /// Runs. Whether anything checks the result is a v3 question.
    Executed,
    /// Predicted to have no viable mutant, so v3 will not be able to score it.
    ///
    /// This is a blind spot, not a guarantee. Some of it really is the type
    /// system carrying the contract (a return type with no meaningful
    /// `Default`); some of it is our mutation operator being too weak
    /// (`-> impl Trait`, `const fn`). Only v3 can tell them apart, by
    /// reporting why cargo-mutants skipped the mutant.
    NoViableMutant,
}

impl ExecutionState {
    /// Report label. Derived `Debug` would render `ClaimedButUnexecuted` as one
    /// unreadable word, so the wording is spelled out here.
    pub fn label(self) -> &'static str {
        match self {
            ExecutionState::Unexecuted => "unexecuted",
            ExecutionState::ClaimedButUnexecuted => "claimed, not run",
            ExecutionState::Executed => "executed",
            ExecutionState::NoViableMutant => "no viable mutant",
        }
    }
}

pub fn execution_state(
    symbol: &Symbol,
    coverage: &CoverageMap,
    claims: &ClaimMap,
) -> ExecutionState {
    if coverage.executed(&symbol.id) {
        if symbol.likely_unviable_to_mutate() {
            return ExecutionState::NoViableMutant;
        }
        return ExecutionState::Executed;
    }
    if claims
        .claimants(&symbol.id)
        .iter()
        .any(|(_, kind)| kind.is_direct())
    {
        ExecutionState::ClaimedButUnexecuted
    } else {
        ExecutionState::Unexecuted
    }
}

pub fn render_coverage(inv: &Inventory, coverage: &CoverageMap, verbose: bool) -> String {
    let claims = ClaimMap::build(inv);
    let mut out = String::new();
    let mut counts: BTreeMap<ExecutionState, usize> = BTreeMap::new();

    let mut by_file: BTreeMap<&str, Vec<&Symbol>> = BTreeMap::new();
    for symbol in inv.scorable() {
        by_file
            .entry(symbol.id.file.as_str())
            .or_default()
            .push(symbol);
    }

    for (file, symbols) in by_file {
        let mut lines = Vec::new();
        for symbol in symbols {
            let state = execution_state(symbol, coverage, &claims);
            *counts.entry(state).or_default() += 1;
            if !verbose && state == ExecutionState::Executed {
                continue;
            }

            let cov = coverage.for_symbol(&symbol.id);
            let mut notes = Vec::new();
            if cov.entries > 1 && verbose {
                notes.push(format!("{} coverage entries", cov.entries));
            }
            if cov.nested_uncovered > 0 {
                notes.push(format!(
                    "{} of {} never ran",
                    cov.nested_uncovered,
                    plural(cov.nested, "nested closure")
                ));
            }
            lines.push(format!(
                "  {:<46} {:<22} {:>6}  {}",
                truncate(leaf_path(&symbol.path), 46),
                state.label(),
                if cov.executed() {
                    format!("{}x", cov.execution_count)
                } else {
                    "-".to_string()
                },
                notes.join(", ")
            ));
        }
        if !lines.is_empty() {
            let _ = writeln!(out, "{file}");
            for line in lines {
                let _ = writeln!(out, "{}", line.trim_end());
            }
            out.push('\n');
        }
    }

    let get = |s: ExecutionState| counts.get(&s).copied().unwrap_or(0);
    let _ = writeln!(
        out,
        "summary\n  executed          {}\n  unexecuted        {}\n  claimed but not   {}\n  no viable mutant  {}",
        get(ExecutionState::Executed),
        get(ExecutionState::Unexecuted),
        get(ExecutionState::ClaimedButUnexecuted),
        get(ExecutionState::NoViableMutant),
    );

    if coverage.unmatched > 0 {
        let _ = writeln!(
            out,
            "  unmatched entries {} (macro and derive output, which has no source definition)",
            coverage.unmatched
        );
    }
    if !coverage.files_without_coverage.is_empty() {
        let _ = writeln!(
            out,
            "\n{} inventoried file(s) absent from the coverage report -- stale report, or the crate was not built:",
            coverage.files_without_coverage.len()
        );
        for f in coverage
            .files_without_coverage
            .iter()
            .take(if verbose { usize::MAX } else { 8 })
        {
            let _ = writeln!(out, "  {f}");
        }
    }

    let _ = writeln!(
        out,
        "\nexecution is not verification: `executed` means a test ran the symbol,\nnot that anything checked the result. that needs slice v3."
    );
    out
}

/// Drop the crate and module prefix, keeping the last two path segments.
fn leaf_path(path: &str) -> &str {
    // A trait-impl method is written `module::<Type as Trait>::method`; cutting
    // it on "::" boundaries would leave the meaningless tail `Trait>::method`.
    if let Some(idx) = path.rfind("::<") {
        return &path[idx + 2..];
    }
    match path.rmatch_indices("::").nth(1) {
        Some((idx, _)) => &path[idx + 2..],
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_path_keeps_the_last_two_segments() {
        assert_eq!(leaf_path("mycrate::config::Config::new"), "Config::new");
        assert_eq!(leaf_path("mycrate::config::parse"), "config::parse");
        assert_eq!(leaf_path("solo"), "solo");
    }

    #[test]
    fn leaf_path_does_not_split_a_trait_impl_path_on_its_inner_separator() {
        // Cutting `<SymbolId as fmt::Display>::fmt` on "::" boundaries leaves
        // the meaningless tail `Display>::fmt`.
        assert_eq!(
            leaf_path("oracle_core::symbol::<SymbolId as fmt::Display>::fmt"),
            "<SymbolId as fmt::Display>::fmt"
        );
    }

    #[test]
    fn truncate_marks_the_cut_and_leaves_short_strings_alone() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc~");
        assert_eq!(truncate("abcdef", 4).chars().count(), 4);
    }

    #[test]
    fn plural_agrees_the_noun_with_the_count() {
        assert_eq!(plural(1, "accessor"), "1 accessor");
        assert_eq!(plural(0, "accessor"), "0 accessors");
        assert_eq!(plural(2, "accessor"), "2 accessors");
    }

    #[test]
    fn execution_state_labels_are_readable_not_debug_formatted() {
        assert_eq!(
            ExecutionState::ClaimedButUnexecuted.label(),
            "claimed, not run"
        );
        assert_eq!(ExecutionState::NoViableMutant.label(), "no viable mutant");
        assert_eq!(ExecutionState::Executed.label(), "executed");
    }

    #[test]
    fn wrap_indents_continuation_lines_and_respects_the_width() {
        let text = "the quick brown fox jumps over the lazy dog again and again";
        let wrapped = wrap(text, 4, 30);
        let lines: Vec<&str> = wrapped.split('\n').collect();
        assert!(lines.len() > 1, "long text must wrap");
        assert!(lines[0].len() <= 30);
        assert!(
            lines[1..].iter().all(|l| l.starts_with("    ")),
            "continuations carry the indent: {lines:?}"
        );
    }
}
