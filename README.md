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

cargo-oracle  static audit

  2 files, 5 scorable symbols, 5 tests (1 doctest)

3 of 5 tests have an oracle that cannot discriminate:

  src/config.rs:70  test_set_retries           no oracle
      high   tautological-assert (ORC007)
             assert!(true)
      high   oracle-shape-mismatch (ORC010)
             on weak_suite::config::Config::set_retries
             `c.set_retries(..)` mutates `c`, which no assertion observes

  src/config.rs:64  test_validate              no oracle
      high   no-oracle (ORC001)

  src/config.rs:59  test_parse                 weak
      high   discriminant-only (ORC002)
             assert!(parse("h:1").is_ok())

  2 tests not shown: no findings. Use -v to list every test.

summary
  oracle strength   2 strong, 0 partial, 1 weak, 2 with none
  findings          4 high, 0 medium, 0 low
```

The report is organized around the **test**, because a test is what you fix.
Every count reconciles: if five tests exist and three are listed, the report
says where the other two went.

`ORC010` is the finding you cannot get anywhere else. Rust puts effects in
types, so `fn set_retries(&mut self, n: u8)` announces that its result lives in
the receiver — and a test that calls it and never mentions `c` again is
*structurally* blind to it, no matter how many assertions it has.

Any rule will explain itself:

```
$ cargo oracle explain ORC010

ORC010  oracle-shape-mismatch   severity: high

why it matters
  The claimed symbol mutates its receiver, but no assertion observes that
  receiver after the call. The required oracle is post-state; the test only
  has return-value oracles.

how to fix it
  Assert on the receiver after the call: `obj.mutate();
  assert_eq!(obj.field, expected);` -- or compare the whole value if it
  derives PartialEq.
```

### And what v3 says, with evidence

```
$ cargo oracle verify --with-attribution

src/config.rs
  Config::new           not mutated       no mutant exists for this signature -- unscorable, not unverified
  Config::set_retries   PSEUDO-TESTED     1 mutant survived: ()
                        ^ predicted by ORC010 -- `c.set_retries(..)` mutates `c`, which no assertion observes
  Config::validate      PSEUDO-TESTED     1 mutant survived: Ok(())
  config::parse         verified          by parse_extracts_host_and_port
  config::redact        PSEUDO-TESTED     5 mutants survived: String::new(), "xyzzy".into(), ==
                        ^ no test executes it at all

per-test verdict
  TEST                                          EXECUTES  VERIFIES
  weak_suite::config::tests::test_parse                2         0  <- runs code, verifies none of it
  weak_suite::config::tests::test_validate             2         0  <- runs code, verifies none of it
  weak_suite::config::tests::test_set_retries          2         0  <- runs code, verifies none of it
  weak_suite::config::tests::parse_extracts_ho~        2         1
```

Two lines there need all four slices at once.

`executes 2, verifies 0` is the question this project started from, answered
directly — and no other tool reports mutation score per *test*.

`predicted by ORC010` is the argument for building it in layers. The static rule
costs milliseconds and no build; the mutation evidence costs a rebuild per
mutant. When the cheap check calls it correctly, you run that on every commit
and save the expensive one for the diff.

## Honesty about evidence

Each slice claims only what its evidence supports. v0 and v1 never report a
symbol as *verified*. v3 never reports an unscorable symbol as a passing one —
and there are **three** distinct ways to be unscorable, none of which mean
verified:

| State | Cause |
|---|---|
| `no viable mutant` | The mutant did not compile. Partly type enforcement, partly our operator being weak — not a guarantee |
| `not mutated` | cargo-mutants examined the file and produced nothing for this signature (`-> Self` on a constructor) |
| `out of scope` | The run never examined this file at all (`--file`, `--in-diff`) |

That last distinction is not pedantry. Scoping a run to one file leaves the rest
of the workspace unexamined, and an earlier version reported all of it as "no
mutant exists" — a gap laundered into a reassurance, which is precisely the
failure that makes coverage numbers untrustworthy.

## Usage

```sh
nix develop                       # rustc, cargo-nextest, cargo-llvm-cov, cargo-mutants
cargo build --release

cargo oracle lint                 # static oracle audit (no build, no test run)
cargo oracle explain ORC010       # what a rule means and how to fix it
cargo oracle lint --deny high     # exit 1 on any high-severity finding, for CI
cargo oracle inventory            # symbols and the oracle shape each requires
cargo oracle claims               # which tests speak for which symbols
cargo oracle coverage             # which symbols actually run (builds + runs tests)
cargo oracle coverage --coverage-json report.json   # reuse an existing report
cargo oracle attribute            # which test runs which symbol (O(tests), slow)
cargo oracle attribute --tests parse --dry-run     # scope it first
cargo oracle verify --since origin/main            # mutate only what the branch changed
cargo oracle verify --in-diff pr.diff              # or supply the diff yourself
cargo oracle verify --with-attribution            # adds the per-test verdict
cargo oracle fastverify --dry-run                 # experimental: one build, N runs
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
- **v3 — verification** ✅ : `cargo-mutants` body-replacement mutants, with
  kills attributed back to the individual test that caught them. Produces the
  per-test `executes N, verifies M` verdict, and confirms or refutes the ORC010
  predictions v0 made for free.

All four slices are implemented and each runs on its own.

`cargo oracle fastverify` is an experimental fifth: it compiles every mutation
in at once behind a runtime switch, so the cost is one build plus N test runs
rather than N builds. It can also report a symbol as *unreached* rather than
*missed*, a distinction cargo-mutants cannot make. See
[docs/design.md](docs/design.md) for how it avoids one uncompilable default
breaking the whole build -- and for its limitations, which are real.

## Library use

`oracle-core` is the library behind the subcommand, and every slice is usable on
its own:

```sh
cargo doc -p oracle-core --open
cargo run --example static_audit -- path/to/crate   # v0, no build required
cargo run --example verdicts -- . mutants.out       # read an existing run
```

## Documentation

- [docs/design.md](docs/design.md) — why symbol identity is a definition span,
  what each symbol state does and does not claim, how each slice joins onto the
  next, and why Rust's mutation cost model is inverted relative to every other
  ecosystem.
- [docs/oracle-lints.md](docs/oracle-lints.md) — every rule, with the pattern it
  catches and the three that were deliberately *not* made rules.

## License

MIT
