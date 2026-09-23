# cargo-oracle

**Symbol-level test attribution for Rust.** Not "was this line executed?" but
"**would any test fail if this symbol stopped working?**"

## The problem

Code coverage answers *did we run the line*. It cannot answer *did we check the
result*. That gap has always existed, but agent-written tests widen it sharply:
a test that executes thirty symbols and asserts `result.is_ok()` on one of them
reports as excellent coverage and verifies almost nothing.

Counting assertions doesn't help either. Empirical work on AI-authored test
commits finds agent tests carry a *median of 2.0 assertions vs 1.0 for humans* —
by assertion density, agent tests score **better** than human ones while
detecting fewer injected faults. The presence of an oracle is not the question.
Whether it can fail is.

## What cargo-oracle reports

Per symbol, one of four states:

| State | Meaning |
|---|---|
| `unexecuted` | No test runs it. An ordinary coverage gap. |
| `pseudo-tested` | Tests run it, but destroying its body fails no test. |
| `verified` | Some test fails when the body is destroyed — and we name it. |
| `type-enforced` | No viable mutant: the type system carries the contract. |

And, inverted, per *test* — the report that actually catches agent slop:

```
test config::tests::test_merge_and_validate
  executes 34 symbols, verifies 0          <- pure smoke test
```

No other tool emits mutation score per test; they aggregate to file or project,
which is exactly the granularity at which a weak new test disappears into a
healthy average.

## Status

Built in slices. Each is independently useful and ships on its own.

- **v0 — static** *(in progress)*: symbol inventory, claim map, oracle-strength
  lints. No build, no execution, runs in milliseconds on any crate.
- **v1 — execution**: function-level coverage from `cargo llvm-cov --json`,
  joined to the inventory by definition span.
- **v2 — per-test attribution**: `cargo-nextest`'s process-per-test model gives
  one profile per test, so `executes` becomes an edge rather than a bit.
- **v3 — verification**: `cargo-mutants` body-replacement mutants, with kills
  attributed back to the individual test that caught them.

See [docs/design.md](docs/design.md) for the architecture and the reasoning
behind the span-based join key.

## Quickstart

```sh
nix develop          # or: cargo build --release
cargo oracle lint    # static oracle audit, no build required
```

## License

MIT
