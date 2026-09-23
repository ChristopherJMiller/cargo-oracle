//! End-to-end: parse a real crate, build its claim map, and audit its oracles.
//!
//! The fixture in `tests/fixtures/weak-suite` is a small crate whose test
//! module is deliberately typical of agent-written suites — four tests, plenty
//! of assertions, and almost no discrimination.

use oracle_core::claims::{ClaimKind, ClaimMap};
use oracle_core::inventory::{walk_workspace, Inventory, TestKind};
use oracle_core::lint::{self, OracleStrength, Rule};
use oracle_core::symbol::{RequiredOracle, SelfKind, Triviality};
use std::path::Path;

fn fixture() -> Inventory {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/weak-suite");
    walk_workspace(&dir).expect("fixture crate must parse")
}

fn sym<'a>(inv: &'a Inventory, name: &str) -> &'a oracle_core::Symbol {
    inv.symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no symbol named `{name}`"))
}

fn findings_for(inv: &Inventory, test_name: &str) -> Vec<Rule> {
    let result = lint::analyze(inv)
        .into_iter()
        .find(|o| o.test.path.ends_with(test_name))
        .unwrap_or_else(|| panic!("no test named `{test_name}`"));
    let mut rules: Vec<Rule> = result.findings.iter().map(|f| f.rule).collect();
    rules.sort();
    rules.dedup();
    rules
}

#[test]
fn inventory_finds_every_function_and_classifies_accessors() {
    let inv = fixture();
    assert!(inv.parse_failures.is_empty(), "{:?}", inv.parse_failures);

    let names: Vec<&str> = inv.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"parse"));
    assert!(names.contains(&"new"));
    assert!(names.contains(&"set_retries"));
    assert!(names.contains(&"validate"));

    // `fn host(&self) -> &str { &self.host }` carries no behaviour to destroy.
    assert_eq!(sym(&inv, "host").triviality, Triviality::Accessor);
    assert_eq!(sym(&inv, "parse").triviality, Triviality::Normal);
    assert!(!inv.scorable().any(|s| s.name == "host"));
}

#[test]
fn required_oracle_is_derived_from_the_signature() {
    let inv = fixture();

    let set_retries = sym(&inv, "set_retries");
    assert_eq!(set_retries.self_kind, SelfKind::RefMut);
    assert_eq!(set_retries.required_oracle, RequiredOracle::PostState);

    let validate = sym(&inv, "validate");
    assert_eq!(validate.self_kind, SelfKind::Ref);
    assert_eq!(validate.required_oracle, RequiredOracle::ReturnValue);
}

#[test]
fn doctest_is_inventoried_and_claims_its_own_item_exactly() {
    let inv = fixture();
    assert_eq!(sym(&inv, "new").doctests, 1);

    let doctests: Vec<_> = inv
        .tests
        .iter()
        .filter(|t| t.kind == TestKind::Doctest)
        .collect();
    assert_eq!(doctests.len(), 1);

    let claims = ClaimMap::build(&inv);
    let claimants = claims.claimants(&sym(&inv, "new").id);
    assert!(
        claimants
            .iter()
            .any(|(_, kind)| *kind == ClaimKind::Doctest),
        "the doctest on `Config::new` should claim it exactly"
    );
}

#[test]
fn same_file_test_module_claims_the_file_s_symbols() {
    let inv = fixture();
    let claims = ClaimMap::build(&inv);

    let parse_claimants = claims.claimants(&sym(&inv, "parse").id);
    assert!(
        parse_claimants
            .iter()
            .any(|(_, k)| *k == ClaimKind::SameFileTestModule),
        "`mod tests` lives in config.rs, so it claims `parse`"
    );

    // Every scorable symbol is spoken for by the same-file module.
    assert!(
        claims.unclaimed(&inv, true).is_empty(),
        "unclaimed: {:?}",
        claims
            .unclaimed(&inv, true)
            .iter()
            .map(|s| &s.path)
            .collect::<Vec<_>>()
    );
}

#[test]
fn is_ok_test_is_reported_as_discriminant_only() {
    let inv = fixture();
    assert_eq!(
        findings_for(&inv, "test_parse"),
        vec![Rule::DiscriminantOnly]
    );
}

#[test]
fn test_that_calls_without_asserting_has_no_oracle() {
    let inv = fixture();
    assert_eq!(findings_for(&inv, "test_validate"), vec![Rule::NoOracle]);
}

#[test]
fn genuine_value_assertions_produce_no_findings() {
    let inv = fixture();
    let result = lint::analyze(&inv)
        .into_iter()
        .find(|o| o.test.path.ends_with("parse_extracts_host_and_port"))
        .expect("test must be inventoried");

    assert!(
        result.findings.is_empty(),
        "unexpected findings: {:?}",
        result.findings.iter().map(|f| f.rule).collect::<Vec<_>>()
    );
    assert_eq!(result.strength, OracleStrength::Strong);
}

#[test]
fn mutating_call_with_no_post_state_assertion_is_a_shape_mismatch() {
    let inv = fixture();
    let claims = ClaimMap::build(&inv);
    let oracles = lint::analyze(&inv);
    let mismatches = lint::shape_mismatches(&inv, &claims, &oracles);

    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    let finding = &mismatches[0];
    assert_eq!(finding.rule, Rule::OracleShapeMismatch);
    assert!(finding.test.path.ends_with("test_set_retries"));
    assert_eq!(
        finding.symbol.as_deref(),
        Some("weak_suite::config::Config::set_retries")
    );
    assert!(finding.snippet.contains("mutates `c`"));
}

#[test]
fn the_whole_fixture_suite_grades_out_as_weak() {
    let inv = fixture();
    let oracles = lint::analyze(&inv);

    let strong = oracles
        .iter()
        .filter(|o| o.strength == OracleStrength::Strong)
        .count();
    assert_eq!(
        strong, 2,
        "only `parse_extracts_host_and_port` and the doctest carry a strong oracle"
    );
    assert!(
        oracles.iter().filter(|o| !o.findings.is_empty()).count() >= 3,
        "three of the four unit tests should be flagged"
    );
}

#[test]
fn a_nested_fixture_crate_is_not_attributed_to_its_host_package() {
    // `tests/fixtures/weak-suite` is a crate of its own living inside
    // oracle-core's `tests/` tree. Walking oracle-core must not adopt it.
    let host = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inv = walk_workspace(host).expect("oracle-core must parse");

    assert!(
        !inv.files.iter().any(|f| f.contains("fixtures/weak-suite")),
        "fixture files leaked into the host inventory: {:?}",
        inv.files
    );
    assert!(
        !inv.tests.iter().any(|t| t.id.path.contains("set_retries")),
        "fixture tests were attributed to oracle_core"
    );
}

// ---------------------------------------------------------------------------
// Slice v1: the coverage join
// ---------------------------------------------------------------------------

/// Real `cargo llvm-cov --json` output for the weak-suite fixture, with the
/// machine-specific workspace root templated out. Using a genuine artifact
/// rather than a hand-written one means the join is tested against the schema
/// llvm-cov actually emits, monomorphization and closure entries included.
fn fixture_coverage() -> (Inventory, oracle_core::coverage::CoverageMap) {
    use oracle_core::coverage;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/weak-suite");
    let raw = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/weak-suite-coverage.json"),
    )
    .expect("coverage fixture must exist")
    .replace("{ROOT}", root.to_str().expect("root path is utf-8"));

    let data = coverage::parse(&raw, &root).expect("llvm-cov output must parse");
    let inv = fixture();
    let map = coverage::CoverageMap::join(&inv, &data);
    (inv, map)
}

#[test]
fn coverage_places_every_entry_with_no_leftovers() {
    let (_, map) = fixture_coverage();
    assert_eq!(
        map.unmatched, 0,
        "every coverage entry should land on a symbol or a test"
    );
    assert_eq!(
        map.tests.len(),
        4,
        "four #[test] functions were inventoried"
    );
    assert!(map.files_without_coverage.is_empty());
}

#[test]
fn execution_counts_are_summed_onto_the_defining_symbol() {
    let (inv, map) = fixture_coverage();

    // `Config::new` is reached by every test, directly or through `parse`.
    let new = map.for_symbol(&sym(&inv, "new").id);
    assert_eq!(new.execution_count, 4);
    assert_eq!(new.entries, 1, "one binary, no generics: a single entry");
    assert!(new.executed());

    let parse = map.for_symbol(&sym(&inv, "parse").id);
    assert_eq!(parse.execution_count, 2);
}

#[test]
fn an_uncalled_accessor_is_reported_as_unexecuted() {
    let (inv, map) = fixture_coverage();
    let host = map.for_symbol(&sym(&inv, "host").id);
    assert_eq!(host.execution_count, 0);
    assert!(!host.executed());
}

#[test]
fn a_closure_lands_inside_its_enclosing_symbol_not_beside_it() {
    let (inv, map) = fixture_coverage();

    // `parse` contains a `.map_err(|_| ...)` closure that llvm-cov reports as
    // its own function entry, starting inside `parse`'s span. It must not be
    // mistaken for a symbol, and its zero count must not drag down `parse`.
    let parse = map.for_symbol(&sym(&inv, "parse").id);
    assert_eq!(parse.nested, 1, "the map_err closure is a nested entry");
    assert_eq!(
        parse.nested_uncovered, 1,
        "no test exercises the bad-port path, so the closure never ran"
    );
    assert_eq!(
        parse.execution_count, 2,
        "the closure's count must not be folded into its parent's"
    );
}

#[test]
fn execution_state_distinguishes_claimed_but_unrun_from_executed() {
    use oracle_core::claims::ClaimMap;
    use oracle_core::report::{execution_state, ExecutionState};

    let (inv, map) = fixture_coverage();
    let claims = ClaimMap::build(&inv);

    assert_eq!(
        execution_state(sym(&inv, "parse"), &map, &claims),
        ExecutionState::Executed
    );

    // `redact` is claimed by the same-file test module and never called. That
    // is a stronger finding than "nothing claims it": someone took
    // responsibility for it and then did not exercise it.
    assert_eq!(
        execution_state(sym(&inv, "redact"), &map, &claims),
        ExecutionState::ClaimedButUnexecuted
    );
    assert_eq!(map.for_symbol(&sym(&inv, "redact").id).execution_count, 0);
}

#[test]
fn an_accessor_is_excluded_from_scoring_and_so_from_claims() {
    let (inv, _) = fixture_coverage();
    let claims = ClaimMap::build(&inv);

    // `Config::host` is a bare field read. It is inventoried, but not scorable,
    // so no test is held responsible for it and it never reaches a report.
    assert_eq!(sym(&inv, "host").triviality, Triviality::Accessor);
    assert!(claims.claimants(&sym(&inv, "host").id).is_empty());
}

// ---------------------------------------------------------------------------
// Slice v3: mutation verification
// ---------------------------------------------------------------------------

/// Real `cargo mutants` output for the weak-suite fixture: outcomes.json plus
/// the one caught mutant's log. Testing against the genuine schema rather than
/// a hand-written one is the point -- the `scenario` field is either the string
/// "Baseline" or a `{ "Mutant": .. }` object, which a synthetic fixture would
/// almost certainly get wrong.
fn fixture_mutation() -> (Inventory, oracle_core::mutation::MutationMap) {
    use oracle_core::mutation::{self, MutationMap};

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/weak-suite-mutants");
    let mutants = mutation::load(&dir).expect("cargo-mutants output must parse");
    let inv = fixture();
    let map = MutationMap::build(&inv, &mutants);
    (inv, map)
}

#[test]
fn mutants_join_onto_the_symbol_whose_body_contains_them() {
    let (inv, map) = fixture_mutation();

    // cargo-mutants' own function span starts at the doc comment, so it does
    // not equal our `fn` line. Joining on the *replaced* span sidesteps that.
    assert!(
        map.unattributed.is_empty(),
        "every mutant should land in a symbol: {:?}",
        map.unattributed
            .iter()
            .map(|m| (&m.function_name, m.line))
            .collect::<Vec<_>>()
    );
    assert_eq!(map.total_mutants, 10);
    assert!(map.verdicts.contains_key(&sym(&inv, "set_retries").id));
}

#[test]
fn a_test_with_a_real_oracle_verifies_its_symbol_and_is_named() {
    use oracle_core::mutation::Verification;

    let (inv, map) = fixture_mutation();
    let parse = map.verdict(&sym(&inv, "parse").id);

    assert_eq!(parse.verification(), Verification::Verified);
    assert_eq!(parse.caught, 1);
    let killers: Vec<&str> = parse.killed_by.iter().map(|t| t.path.as_str()).collect();
    assert_eq!(
        killers,
        vec!["weak_suite::config::tests::parse_extracts_host_and_port"],
        "the kill is attributed to the one test that asserts on values"
    );
}

#[test]
fn orc010_predicted_the_pseudo_tested_symbol_before_any_mutant_ran() {
    use oracle_core::claims::ClaimMap;
    use oracle_core::lint;
    use oracle_core::mutation::Verification;

    let (inv, map) = fixture_mutation();
    let set_retries = sym(&inv, "set_retries");

    // v3's evidence: the body was replaced with `()` and nothing failed.
    let verdict = map.verdict(&set_retries.id);
    assert_eq!(verdict.verification(), Verification::PseudoTested);
    assert_eq!(verdict.missed, 1);
    assert!(verdict.killed_by.is_empty());

    // v0's prediction, from the signature alone, at no build cost.
    let claims = ClaimMap::build(&inv);
    let oracles = lint::analyze(&inv);
    let predicted = lint::shape_mismatches(&inv, &claims, &oracles);
    assert!(
        predicted
            .iter()
            .any(|f| f.symbol.as_deref() == Some(set_retries.path.as_str())),
        "ORC010 should have called this statically: {predicted:?}"
    );
}

#[test]
fn a_symbol_no_test_reaches_is_pseudo_tested_across_every_mutant() {
    use oracle_core::mutation::Verification;

    let (inv, map) = fixture_mutation();
    let redact = map.verdict(&sym(&inv, "redact").id);

    assert_eq!(redact.verification(), Verification::PseudoTested);
    assert_eq!(redact.caught, 0);
    assert!(
        redact.missed >= 4,
        "redact has value and comparison mutants, all surviving: {redact:?}"
    );
}

#[test]
fn a_signature_with_no_mutant_is_unscorable_rather_than_unverified() {
    use oracle_core::mutation::Verification;

    let (inv, map) = fixture_mutation();

    // `Config::new` returns `Self`; body replacement has no default to
    // substitute, so cargo-mutants emits nothing. The constructor is in fact
    // well tested -- reporting it as a gap would be wrong.
    let new = map.verdict(&sym(&inv, "new").id);
    assert_eq!(new.caught + new.missed + new.unviable, 0);
    assert_eq!(
        map.verification(&sym(&inv, "new").id),
        Verification::NotMutated
    );
}

// ---------------------------------------------------------------------------
// Report rendering: the output is the product, so its shape is tested too
// ---------------------------------------------------------------------------

#[test]
fn the_rendered_report_accounts_for_every_test_it_does_not_show() {
    use oracle_core::report::Report;

    let inv = fixture();
    let text = Report::build(&inv).to_text(false);

    // The fixture has five tests; two have a strong oracle and no findings, so
    // the detail section lists three. A reader who sees "5 tests" in the header
    // and counts three below must be told why, or the report reads as a bug.
    assert!(
        text.contains("3 of 5 tests"),
        "the count of flagged tests must be stated: {text}"
    );
    assert!(
        text.contains("2 tests not shown"),
        "the tests omitted must be accounted for: {text}"
    );
}

#[test]
fn findings_name_the_rule_not_only_its_code() {
    use oracle_core::report::Report;

    let inv = fixture();
    let text = Report::build(&inv).to_text(false);

    // `ORC002` alone tells a first-time reader nothing. The name carries the
    // meaning and the code is for filtering and `explain`.
    assert!(text.contains("discriminant-only (ORC002)"), "{text}");
    assert!(text.contains("oracle-shape-mismatch (ORC010)"), "{text}");
}

#[test]
fn a_clean_report_says_so_rather_than_printing_an_empty_section() {
    use oracle_core::claims::ClaimMap;
    use oracle_core::report::Report;

    let inv = fixture();
    let report = Report::build(&inv);
    let text = report.to_text(false);

    // Every scorable symbol in the fixture is claimed, so the report should
    // state that outright instead of leaving a bare heading.
    assert!(ClaimMap::build(&inv).unclaimed(&inv, false).is_empty());
    assert!(text.contains("Every scorable symbol is claimed"), "{text}");
}
