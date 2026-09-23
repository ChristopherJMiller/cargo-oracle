# cargo-oracle

**Symbol-level test attribution for Rust.** Not "was this line executed?" but
"**would any test fail if this symbol stopped working?**"

## The problem

Code coverage answers *did we run the line*. It cannot answer *did we check the
result*. That gap has always existed; agent-written tests widen it sharply.

Counting assertions does not help. Empirical work on AI-authored test commits
finds agent tests carry a **median of 2.0 assertions against 1.0 for human
ones** — while detecting *fewer* injected faults. Assertion density is not a
weak signal for agent tests, it is an inverted one. The presence of an oracle is
not the question. Whether it can fail is.

## What it looks like

```
$ cargo oracle lint

cargo-oracle  static audit (v0)
  2 files, 5 symbols (4 scorable, 1 accessor skipped), 5 tests (1 doctests)

src/config.rs
  high:50    ORC002 discriminant-only        test_parse
         parse("h:1").is_ok()
  high:54    ORC001 no-oracle                test_validate
  high:62    ORC010 oracle-shape-mismatch    test_set_retries
         on weak_suite::config::Config::set_retries
         `c.set_retries(..)` mutates `c`, which no assertion observes
  high:63    ORC007 tautological-assert      test_set_retries
         true

per-test oracle strength
  none     weak_suite::config::tests::test_set_retries    ORC007 ORC010
  none     weak_suite::config::tests::test_validate       ORC001
  weak     weak_suite::config::tests::test_parse          ORC002

summary
  oracle strength   strong 2   partial 0   weak 1   none 2
  findings          4 high, 0 medium, 0 low
  unclaimed         0 of 4 scorable symbols
```

The report leads with the **per-test** verdict. Every other tool aggregates to
file or project, which is exactly the granularity at which one weak new test
disappears into a healthy average.

`ORC010` is the finding you cannot get anywhere else. Rust puts effects in
types, so `fn set_retries(&mut self, n: u8)` announces that its result lives in
the receiver — and a test that calls it and never mentions `c` again is
*structurally* blind to it, no matter how many assertions it has.

## Usage

```sh
nix develop                       # rustc, cargo-nextest, cargo-llvm-cov, cargo-mutants
cargo build --release

cargo oracle lint                 # static oracle audit (no build, no test run)
cargo oracle lint --deny high     # exit 1 on any high-severity finding, for CI
cargo oracle inventory            # symbols and the oracle shape each requires
cargo oracle claims               # which tests speak for which symbols
cargo oracle coverage             # which symbols actually run (builds + runs tests)
cargo oracle coverage --coverage-json report.json   # reuse an existing report
cargo oracle attribute            # which test runs which symbol (O(tests), slow)
cargo oracle attribute --tests parse --dry-run     # scope it first
cargo oracle --format json lint   # machine-readable
```

## Status

Built in slices, each independently useful.

- **v0 — static** ✅ : symbol inventory, claim map, ten oracle-strength lints.
  No build, no execution, milliseconds on a whole workspace.
- **v1 — execution** ✅ : function-level coverage from `cargo llvm-cov --json`,
  joined to the inventory by definition span. Separates monomorphized entries
  from nested closures, and `claimed, not run` from plain `unexecuted`.
- **v2 — per-test attribution** ✅ : `cargo-nextest`'s process-per-test model
  gives one profile per test, so *executes* becomes an edge rather than a bit.
  Surfaces tests with broad reach and no discrimination, symbols no test
  reaches, and symbols only one test reaches.
- **v3 — verification**: `cargo-mutants` body-replacement mutants, with kills
  attributed back to the individual test that caught them.

v0 and v1 deliberately stop short of claiming a symbol is *verified* — that
needs mutation evidence. What they can say is that an oracle cannot
discriminate, that nothing claims a symbol, and that something claims it but
never runs it.

## Documentation

- [docs/design.md](docs/design.md) — why symbol identity is a definition span,
  the four symbol states, and why Rust's mutation cost model is inverted.
- [docs/oracle-lints.md](docs/oracle-lints.md) — every rule, with the pattern it
  catches and the three that were deliberately *not* made rules.

## License

MIT
