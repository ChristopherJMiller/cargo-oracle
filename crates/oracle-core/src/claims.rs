//! Which tests *claim* which symbols — computed statically, with no build.
//!
//! In most languages "what does this test claim to test?" is a guess from a
//! naming convention (`FooTest` speaks for `Foo`, probably). Rust encodes the
//! answer in the source:
//!
//! - A `#[cfg(test)] mod tests` lives **inside the file** whose items it speaks
//!   for. That is containment, not a heuristic.
//! - A doctest is lexically **attached to the item** it documents. Exact
//!   attribution, at zero cost.
//!
//! A claim is not evidence of anything. It is the denominator: the set a test
//! has taken responsibility for, against which slices v1-v3 measure what it
//! actually executes and actually verifies. A wide gap between *claimed* and
//! *verified* is the shape agent-written test suites take.

use crate::inventory::{Inventory, TestId, TestKind};
use crate::symbol::{Symbol, SymbolId, Visibility};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimKind {
    /// The doctest is attached to this exact item. Certain.
    Doctest,
    /// The test lives in a `#[cfg(test)]` module in the same file. Structural.
    SameFileTestModule,
    /// The test's name contains the symbol's name. Suggestive only.
    NameSimilarity,
    /// An integration test in `tests/`, which can only reach the public API.
    /// Every `pub` symbol in the package is nominally in scope, so this claim
    /// is broad enough to be near-worthless alone — it exists so that a symbol
    /// reachable *only* this way is not reported as entirely unclaimed.
    IntegrationSurface,
}

impl ClaimKind {
    /// How much weight to give this edge when a symbol has several.
    pub fn confidence(self) -> f32 {
        match self {
            ClaimKind::Doctest => 1.0,
            ClaimKind::SameFileTestModule => 0.9,
            ClaimKind::NameSimilarity => 0.7,
            ClaimKind::IntegrationSurface => 0.2,
        }
    }

    /// Whether this edge is specific enough to hold a test responsible for the
    /// symbol in a report.
    pub fn is_direct(self) -> bool {
        matches!(self, ClaimKind::Doctest | ClaimKind::SameFileTestModule)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub test: TestId,
    pub symbol: SymbolId,
    pub kind: ClaimKind,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClaimMap {
    pub claims: Vec<Claim>,
}

impl ClaimMap {
    pub fn build(inv: &Inventory) -> Self {
        let mut claims = Vec::new();

        // Index scorable symbols by file so the containment check is cheap.
        let mut by_file: BTreeMap<&str, Vec<&Symbol>> = BTreeMap::new();
        for symbol in inv.scorable() {
            by_file
                .entry(symbol.id.file.as_str())
                .or_default()
                .push(symbol);
        }
        let public: Vec<&Symbol> = inv
            .scorable()
            .filter(|s| s.visibility == Visibility::Public)
            .collect();

        for test in &inv.tests {
            match test.kind {
                TestKind::Doctest => {
                    if let Some(target) = &test.doctest_target {
                        claims.push(Claim {
                            test: test.id.clone(),
                            symbol: target.clone(),
                            kind: ClaimKind::Doctest,
                        });
                    }
                }

                TestKind::Unit => {
                    let same_file = by_file.get(test.id.file.as_str());

                    if test.enclosing_test_module.is_some() {
                        for symbol in same_file.into_iter().flatten() {
                            claims.push(Claim {
                                test: test.id.clone(),
                                symbol: symbol.id.clone(),
                                kind: ClaimKind::SameFileTestModule,
                            });
                        }
                    }

                    // A name match reaches across files, catching the common case
                    // of a test module that tests a sibling module's symbol.
                    let test_name = leaf_name(&test.id.path);
                    for symbol in inv.scorable() {
                        if symbol.id.file == test.id.file && test.enclosing_test_module.is_some() {
                            continue; // already claimed, more strongly, above
                        }
                        if name_claims(&test_name, symbol) {
                            claims.push(Claim {
                                test: test.id.clone(),
                                symbol: symbol.id.clone(),
                                kind: ClaimKind::NameSimilarity,
                            });
                        }
                    }
                }

                TestKind::Integration => {
                    let test_name = leaf_name(&test.id.path);
                    for symbol in &public {
                        let kind = if name_claims(&test_name, symbol) {
                            ClaimKind::NameSimilarity
                        } else {
                            ClaimKind::IntegrationSurface
                        };
                        claims.push(Claim {
                            test: test.id.clone(),
                            symbol: symbol.id.clone(),
                            kind,
                        });
                    }
                }
            }
        }

        Self { claims }
    }

    /// Symbols this test has taken responsibility for.
    pub fn claimed_by(&self, test: &TestId) -> BTreeSet<&SymbolId> {
        self.claims
            .iter()
            .filter(|c| &c.test == test)
            .map(|c| &c.symbol)
            .collect()
    }

    /// Tests that speak for this symbol, strongest edge first.
    pub fn claimants(&self, symbol: &SymbolId) -> Vec<(&TestId, ClaimKind)> {
        let mut found: Vec<_> = self
            .claims
            .iter()
            .filter(|c| &c.symbol == symbol)
            .map(|c| (&c.test, c.kind))
            .collect();
        found.sort_by_key(|(_, kind)| *kind);
        found
    }

    /// Symbols no test claims at all, at or above the given specificity.
    ///
    /// This is a coverage proxy that needs no build: if nothing even *nominally*
    /// speaks for a symbol, no amount of execution data will change that.
    pub fn unclaimed<'a>(&self, inv: &'a Inventory, direct_only: bool) -> Vec<&'a Symbol> {
        let claimed: BTreeSet<&SymbolId> = self
            .claims
            .iter()
            .filter(|c| !direct_only || c.kind.is_direct())
            .map(|c| &c.symbol)
            .collect();
        inv.scorable()
            .filter(|s| !claimed.contains(&s.id))
            .collect()
    }
}

/// `mycrate::config::tests::rejects_empty_host` -> `rejects_empty_host`
fn leaf_name(path: &str) -> String {
    path.rsplit("::").next().unwrap_or(path).to_string()
}

/// Does this test's name contain the symbol's name?
///
/// Deliberately conservative. Every token of the symbol name must appear in the
/// test name, so `validate` claims `test_validate_rejects_empty` but not
/// `test_parse`, and a one-or-two-character symbol name never claims anything.
fn name_claims(test_name: &str, symbol: &Symbol) -> bool {
    let sym_tokens = tokenize(&symbol.name);
    if sym_tokens.is_empty() || symbol.name.len() < 3 {
        return false;
    }
    let test_tokens: BTreeSet<String> = tokenize(test_name).into_iter().collect();
    // `test_` / `it_` / `should_` prefixes carry no signal and are already
    // separate tokens, so plain subset containment is enough.
    sym_tokens.iter().all(|t| test_tokens.contains(t))
}

/// Split `snake_case` and `camelCase` alike into lowercase tokens, dropping
/// the scaffolding words that appear in nearly every test name.
fn tokenize(name: &str) -> Vec<String> {
    const NOISE: &[&str] = &["test", "tests", "it", "should", "when", "then", "case"];
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in name.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else if ch.is_uppercase() && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
            current.push(ch.to_ascii_lowercase());
        } else {
            current.push(ch.to_ascii_lowercase());
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens.retain(|t| !NOISE.contains(&t.as_str()) && !t.is_empty());
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_snake_and_camel_and_drops_noise() {
        assert_eq!(tokenize("test_validate_config"), vec!["validate", "config"]);
        assert_eq!(tokenize("parseHostName"), vec!["parse", "host", "name"]);
        assert_eq!(tokenize("it_should_work"), vec!["work"]);
    }

    #[test]
    fn tokenize_on_noise_only_name_yields_nothing() {
        assert!(tokenize("test").is_empty());
        assert!(tokenize("should_test").is_empty());
    }

    #[test]
    fn leaf_name_takes_final_path_segment() {
        assert_eq!(leaf_name("a::b::c"), "c");
        assert_eq!(leaf_name("solo"), "solo");
    }

    #[test]
    fn claim_kind_orders_certain_edges_before_weak_ones() {
        let mut kinds = [
            ClaimKind::IntegrationSurface,
            ClaimKind::Doctest,
            ClaimKind::NameSimilarity,
            ClaimKind::SameFileTestModule,
        ];
        kinds.sort();
        assert_eq!(kinds[0], ClaimKind::Doctest);
        assert_eq!(kinds[3], ClaimKind::IntegrationSurface);
        assert!(ClaimKind::Doctest.confidence() > ClaimKind::NameSimilarity.confidence());
    }
}
