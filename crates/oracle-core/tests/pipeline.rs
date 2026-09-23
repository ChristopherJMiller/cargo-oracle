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
