//! `coverage`: which symbols actually run.
//!
//! `-C instrument-coverage` is unusual among coverage backends in being
//! natively *function*-granular. `cargo llvm-cov --json` emits a `functions[]`
//! array carrying an execution count and source regions per function, so the
//! symbol layer is the native unit here and the line view is the derived one.
//! Most tooling has that backwards and has to reconstruct symbol coverage by
//! mapping line hits onto a parsed AST.
//!
//! # The join
//!
//! Coverage names are mangled and monomorphized; inventory names are what was
//! written. We never compare them. Instead each coverage entry is placed by the
//! start line of its first region, and the inventory span that contains that
//! line claims it:
//!
//! - An entry starting *exactly* at a symbol's `fn` line is an **instantiation**
//!   of it. A generic function produces one per monomorphization, and their
//!   counts sum to the symbol's total executions.
//! - An entry starting *inside* the body is nested: a closure, an `async`
//!   block, a generator. Its count is tracked separately, because a closure that
//!   never ran inside a function that did is a real and interesting signal, but
//!   adding it to the parent's count would be meaningless.
//! - An entry matching no span at all is macro or derive output. It is counted
//!   and reported rather than silently dropped.

use crate::inventory::{Inventory, TestId};
use crate::symbol::SymbolId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// One `functions[]` entry from the llvm-cov export, reduced to what we join on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoverageFunction {
    /// Mangled and monomorphized. Kept for diagnostics only, never joined on.
    pub name: String,
    /// How many times this entry was executed.
    pub count: u64,
    /// Relative to the workspace root, matching `SymbolId::file`.
    pub file: String,
    /// Start line of the entry's first region, which is what places it.
    pub start_line: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Every function entry in an llvm-cov export.
pub struct CoverageData {
    /// The entries, in report order.
    pub functions: Vec<CoverageFunction>,
}

/// Execution evidence for one inventory symbol.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SymbolCoverage {
    /// Total executions, summed across every coverage entry for this symbol.
    pub execution_count: u64,
    /// How many coverage entries collapsed onto this one source definition.
    ///
    /// Greater than one has two causes that the report cannot distinguish: a
    /// generic function monomorphized several times, or the same function
    /// compiled into several binaries (a library and the test harness linking
    /// it). Either way the join is correct and the counts sum to total
    /// executions -- but this is not a count of generic instantiations.
    pub entries: usize,
    /// Closures and `async` blocks defined inside the body.
    pub nested: usize,
    /// ...of which never ran. An uncovered closure inside a covered function is
    /// an error path nothing exercised -- often the most interesting line in
    /// the report.
    pub nested_uncovered: usize,
}

impl SymbolCoverage {
    /// Whether any test reached this symbol.
    pub fn executed(&self) -> bool {
        self.execution_count > 0
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Coverage joined onto an inventory.
pub struct CoverageMap {
    /// Per-symbol execution evidence.
    pub symbols: BTreeMap<SymbolId, SymbolCoverage>,
    /// Execution counts for inventoried test functions, which per-test attribution builds on.
    pub tests: BTreeMap<TestId, u64>,
    /// Entries that matched no inventory span: macro expansions and derive
    /// output, which have no source definition of their own.
    pub unmatched: usize,
    /// Inventory files that the coverage report never mentions, usually because
    /// the crate was not built or the report is stale.
    pub files_without_coverage: Vec<String>,
}

impl CoverageMap {
    /// Place every coverage entry onto the inventory by span containment.
    pub fn join(inv: &Inventory, data: &CoverageData) -> Self {
        let mut map = CoverageMap::default();

        // Index inventory spans by file so each entry is placed in one pass.
        let mut symbols_by_file: BTreeMap<&str, Vec<&crate::Symbol>> = BTreeMap::new();
        for symbol in &inv.symbols {
            symbols_by_file
                .entry(symbol.id.file.as_str())
                .or_default()
                .push(symbol);
        }
        let mut tests_by_file: BTreeMap<&str, Vec<&crate::TestItem>> = BTreeMap::new();
        for test in &inv.tests {
            tests_by_file
                .entry(test.id.file.as_str())
                .or_default()
                .push(test);
        }

        let mut seen_files: BTreeMap<&str, ()> = BTreeMap::new();

        for func in &data.functions {
            seen_files.insert(func.file.as_str(), ());
            let line = func.start_line;

            // A symbol whose definition line is exactly this entry's start owns
            // it as an instantiation; otherwise the innermost containing span
            // owns it as a nested item.
            let candidates = symbols_by_file.get(func.file.as_str());
            let mut placed = false;

            if let Some(candidates) = candidates {
                if let Some(symbol) = candidates.iter().find(|s| s.span.start == line) {
                    let entry = map.symbols.entry(symbol.id.clone()).or_default();
                    entry.execution_count += func.count;
                    entry.entries += 1;
                    placed = true;
                } else if let Some(symbol) = candidates
                    .iter()
                    .filter(|s| s.span.contains(line))
                    // Innermost wins, so a closure in a method lands on the
                    // method rather than on anything enclosing it.
                    .min_by_key(|s| s.span.end.saturating_sub(s.span.start))
                {
                    let entry = map.symbols.entry(symbol.id.clone()).or_default();
                    entry.nested += 1;
                    if func.count == 0 {
                        entry.nested_uncovered += 1;
                    }
                    placed = true;
                }
            }

            // Test functions are not symbols, but they are inventoried, and
            // per-test attribution needs their identities.
            if !placed {
                if let Some(tests) = tests_by_file.get(func.file.as_str()) {
                    if let Some(test) = tests.iter().find(|t| t.span.contains(line)) {
                        *map.tests.entry(test.id.clone()).or_default() += func.count;
                        placed = true;
                    }
                }
            }

            if !placed {
                map.unmatched += 1;
            }
        }

        // Only files that actually define something can be "missing" coverage.
        // A `mod` declaration file has no functions and so legitimately never
        // appears in the report; flagging it would be a standing false alarm.
        map.files_without_coverage = inv
            .files
            .iter()
            .filter(|f| {
                !seen_files.contains_key(f.as_str())
                    && (symbols_by_file.contains_key(f.as_str())
                        || tests_by_file.contains_key(f.as_str()))
            })
            .cloned()
            .collect();

        map
    }

    /// Evidence for one symbol, defaulting to "never seen" when absent.
    pub fn for_symbol(&self, id: &SymbolId) -> SymbolCoverage {
        self.symbols.get(id).cloned().unwrap_or_default()
    }

    /// Whether any test reached this symbol.
    pub fn executed(&self, id: &SymbolId) -> bool {
        self.symbols.get(id).is_some_and(|c| c.executed())
    }
}

// ---------------------------------------------------------------------------
// Producing and reading the llvm-cov report
// ---------------------------------------------------------------------------

/// The subset of the llvm-cov export schema we depend on. Every other field is
/// ignored, so the parser survives schema revisions (this has been stable from
/// export version 2.x through 3.x).
#[derive(Deserialize)]
struct RawExport {
    data: Vec<RawDatum>,
}

#[derive(Deserialize)]
struct RawDatum {
    #[serde(default)]
    functions: Vec<RawFunction>,
}

#[derive(Deserialize)]
struct RawFunction {
    name: String,
    count: u64,
    filenames: Vec<String>,
    /// `[line_start, col_start, line_end, col_end, count, file_id, expanded_file_id, kind]`
    regions: Vec<Vec<i64>>,
}

/// Parse an `llvm-cov export` JSON report, normalizing paths against the
/// workspace root so they match `SymbolId::file`.
pub fn load(path: &Path, workspace_root: &Path) -> Result<CoverageData> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading coverage report {}", path.display()))?;
    parse(&text, workspace_root)
}

/// Parse an llvm-cov export from a string, normalizing paths against the root.
pub fn parse(text: &str, workspace_root: &Path) -> Result<CoverageData> {
    let raw: RawExport =
        serde_json::from_str(text).context("parsing llvm-cov export JSON (expected `data[]`)")?;

    let mut functions = Vec::new();
    for datum in raw.data {
        for f in datum.functions {
            let Some(region) = f.regions.first() else {
                continue; // No source region: nothing to place it by.
            };
            let start_line = region[0].max(0) as u32;
            let file_id = region.get(5).copied().unwrap_or(0).max(0) as usize;
            let Some(filename) = f.filenames.get(file_id).or_else(|| f.filenames.first()) else {
                continue;
            };

            let file = Path::new(filename)
                .strip_prefix(workspace_root)
                .unwrap_or(Path::new(filename))
                .display()
                .to_string();

            functions.push(CoverageFunction {
                name: f.name,
                count: f.count,
                file,
                start_line,
            });
        }
    }

    Ok(CoverageData { functions })
}

/// Shell out to `cargo llvm-cov` to produce a fresh report.
///
/// This runs the test suite under instrumentation, so it is the first stage
/// that costs a build.
pub fn run_llvm_cov(manifest_dir: &Path, output: &Path, extra_args: &[String]) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new("cargo");
    cmd.current_dir(manifest_dir)
        .arg("llvm-cov")
        .arg("--json")
        .arg("--output-path")
        .arg(output)
        .args(extra_args);

    let status = cmd.status().context(
        "running `cargo llvm-cov` (install it with `cargo install cargo-llvm-cov`, \
         or enter the nix dev shell)",
    )?;

    if !status.success() {
        bail!(
            "`cargo llvm-cov` exited with {}. Run it directly to see the failure.",
            status
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "type": "llvm.coverage.json.export",
      "version": "3.0.1",
      "data": [{
        "functions": [
          {
            "name": "_RNvNtCs_10weak_suite6config5parse",
            "count": 2,
            "filenames": ["/ws/src/config.rs"],
            "regions": [[38,1,42,2,2,0,0,0]]
          },
          {
            "name": "_RNCNvNtCs_10weak_suite6config5parse0B5_",
            "count": 0,
            "filenames": ["/ws/src/config.rs"],
            "regions": [[40,44,40,70,0,0,0,0]]
          },
          {
            "name": "no_regions",
            "count": 7,
            "filenames": ["/ws/src/config.rs"],
            "regions": []
          }
        ]
      }]
    }"#;

    #[test]
    fn parse_normalizes_paths_against_the_workspace_root() {
        let data = parse(SAMPLE, Path::new("/ws")).expect("sample must parse");
        assert_eq!(data.functions.len(), 2, "the region-less entry is skipped");
        assert_eq!(data.functions[0].file, "src/config.rs");
        assert_eq!(data.functions[0].start_line, 38);
        assert_eq!(data.functions[0].count, 2);
    }

    #[test]
    fn a_path_outside_the_workspace_is_kept_verbatim() {
        let data = parse(SAMPLE, Path::new("/elsewhere")).expect("sample must parse");
        assert_eq!(data.functions[0].file, "/ws/src/config.rs");
    }

    #[test]
    fn malformed_json_reports_the_schema_it_wanted() {
        let err = parse("{\"nope\": 1}", Path::new("/ws")).unwrap_err();
        assert!(err.to_string().contains("data[]"), "unhelpful error: {err}");
    }
}
