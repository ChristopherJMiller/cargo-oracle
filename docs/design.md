# Design

## The question

Coverage answers *did this line run*. It structurally cannot answer *did anything
check the result*. Both questions matter, and only one of them has tooling.

The gap has always existed. What changed is who is writing the tests. Three
findings shape this design:

- Agent-authored tests carry a **median of 2.0 assertions against 1.0 for
  human-written ones**, while detecting *fewer* injected faults. Assertion
  density is not a weak signal for AI-written tests — it is an inverted one, so
  any metric built on counting assertions ranks the worst suites highest.
- Prompting a model with buggy code produces tests that pin the bug in place as
  expected behaviour. The oracle is faithful; it is faithful to the wrong thing.
- When one model writes both the implementation and its tests, a shared
  misunderstanding yields a green suite with no independent check anywhere in it.

So the question this tool asks is not "is there an assertion?" but "**would any
assertion fail if this symbol stopped working?**"

## Four slices

Each slice is independently useful and ships on its own. Later slices join onto
the symbol identity that v0 establishes.

| Slice | Question | Evidence | Cost |
|---|---|---|---|
| **v0** static | Can this oracle fail at all? Does anything claim this symbol? | `syn` parse | milliseconds, no build |
| **v1** coverage | Which symbols run? | `cargo llvm-cov --json` | one instrumented build |
| **v2** attribution | Which *test* runs which symbol? | `cargo-nextest`, one profile per test | N profiles + merges |
| **v3** verification | Which test *fails* when the symbol's body is destroyed? | `cargo-mutants` | one rebuild per mutant |

v0 and v1 are implemented. v2 and v3 are sketched below so the data model does
not have to change to accept them.

## Symbol identity is a span, not a name

This is the decision everything else rests on.

Each tool we join against reports `file:line:col`, and none of them agree on
names:

- **llvm-cov** reports mangled, *monomorphized* symbols. One generic `fn parse<T>`
  appears once per instantiation; closures appear as `parse::{{closure}}`; an
  `async fn` appears as the state machine the compiler generated for it.
- **cargo-mutants** reports the source path and the unmangled function name.
- **`syn`** — our inventory — sees only what was written.

Reconciling mangled names across those three is a swamp. Definition spans are
not. Every tool emits one, and *containment* performs the join for free:

- N monomorphized coverage entries all fall inside the one source span that
  defined them, so instantiations collapse with no demangling.
- Code with no source span — derive output, macro expansions — falls outside
  every inventory span and filters itself out, which is the behaviour we want.

`SymbolId` is therefore `(file, line, col)` of the *name* in the definition,
with a `LineSpan` from the `fn` keyword to the closing brace used for
containment. Columns are 1-based to match `cargo-mutants` output; `proc-macro2`
hands us 0-based columns and `SymbolId::new` adjusts.

## Symbol states

| State | Meaning | Evidence needed |
|---|---|---|
| `unexecuted` | Nothing runs it | v1 |
| `claimed, not run` | A test module or doctest claims it, and nothing runs it | v0 + v1 |
| `executed` | Runs. Whether anything *checks* it is a separate question | v1 |
| `pseudo-tested` | Runs, but destroying its body fails no test | v3 |
| `verified` | A named test fails when the body is destroyed | v3 |
| `no viable mutant` | Cannot be scored by body replacement | v0 predicts, v3 confirms |

`claimed, not run` is worth separating from plain `unexecuted`. Both are
uncovered, but in the first case someone took responsibility for the symbol —
put a test module in its file, or wrote a doctest — and then did not exercise
it. That is a broken promise rather than an absence.

The last state needs care, and an earlier draft of this document got it wrong.
When `Default::default()` does not typecheck as a replacement body,
cargo-mutants drops the mutant as *unviable*, and it is tempting to read that as
"the type system carries this function's contract". Sometimes it is: a return
type with no meaningful `Default` genuinely constrains what the function can do.
But `-> impl Trait`, `-> !` and `const fn` are unviable for a duller reason —
our mutation operator is body replacement, and body replacement does not apply.
That is a **blind spot, not a guarantee**, and reporting it as "type-enforced"
would launder a gap into a reassurance.

So v0 predicts the category from the signature and names it `no viable mutant`.
v3 confirms it and can finally distinguish the two causes, because cargo-mutants
reports *why* it skipped a mutant.


## The oracle a signature requires

Rust puts effects in types, which makes a check available here that is not
available elsewhere: the *shape* of a required oracle is readable from the
signature alone.

| Signature | Required oracle | A test that only asserts on the return value is |
|---|---|---|
| `fn(&self) -> T` | observe the return value | fine |
| `fn(&mut self)` | observe the receiver afterwards | **structurally blind** |
| `fn(&mut self) -> T` | both | half blind |
| `fn(..)` with neither | side channel (I/O, globals) | out of reach of any signature-derived oracle |

`RequiredOracle::derive` computes this, and ORC010 enforces it. No amount of
coverage or assertion density substitutes: a test that calls `fn(&mut self)` and
never mentions the receiver again cannot detect a fault in it.

## Slice v1: the coverage join

`-C instrument-coverage` is unusual in being natively *function*-granular:
`cargo llvm-cov --json` emits a `functions[]` array with an execution count and
source regions per function. The symbol layer is the native unit here and the
line view is derived; most tooling has that backwards.

Names are never compared. Each coverage entry is placed by the start line of its
first region, and the inventory span containing that line claims it:

| Entry starts | Classified as | Count handling |
|---|---|---|
| exactly at a symbol's `fn` line | an entry *for* that symbol | summed |
| strictly inside a symbol's span | **nested** — a closure, `async` block, generator | tracked separately |
| in no span at all | macro or derive output | counted and reported |

Verified against real llvm-cov output for the fixture crate: every symbol's
first region started exactly at the `fn` line, and `parse`'s `.map_err(|_| ..)`
closure appeared as its own entry starting at line 40, inside `parse`'s span of
38–42. Folding the closure's zero count into its parent would have been wrong
in both directions — `parse` ran twice, and the closure genuinely never ran.

An uncovered closure inside a covered function is often the most useful line in
the report: it is an error path nothing exercised.

**One entry per symbol is not guaranteed, and more than one does not mean
generics.** The same function compiled into a library and into the test harness
that links it produces two entries, exactly as a generic monomorphized twice
does. The join is correct either way and the counts sum to total executions, but
the number cannot be read as an instantiation count — an earlier version of the
report labelled it that way and was wrong on every non-generic function in the
workspace.

## The claim map

In most languages, "what does this test claim to test?" is a guess from naming
convention. Rust encodes it structurally in two places:

- A `#[cfg(test)] mod tests` lives **inside the file** whose items it speaks for.
  Containment, not heuristic.
- A doctest is lexically **attached to the item** it documents. Exact, free.

Those are `ClaimKind::SameFileTestModule` and `ClaimKind::Doctest`, the two
`is_direct()` edges. Name similarity and the public surface reachable from
`tests/` are kept as explicitly weaker edges, so a symbol reachable only through
an integration test is not reported as entirely unclaimed.

A claim is not evidence. It is the *denominator*: the set a test has taken
responsibility for, against which v1–v3 measure what it actually executes and
actually verifies. A wide gap between claimed and verified is the shape
agent-written suites take.

## Module paths without name resolution

We do not resolve `mod` declarations. A file's module path is read off its
location under `src/`, which is what Rust's own convention encodes:
`src/parser/config.rs` is `parser::config`, `src/parser/mod.rs` is `parser`,
`src/lib.rs` is the crate root.

The tradeoff: a `.rs` file under `src/` that no `mod` declaration reaches still
gets inventoried. That is the safe direction for an audit tool — the symbol
simply reports as unexecuted once v1 lands.

Directories containing their own `Cargo.toml` are pruned from the walk, so a
fixture crate or vendored dependency inside `tests/` is not attributed to its
host package. (Found by dogfooding, not by the unit tests.)

## The cost model is inverted in Rust

This is the constraint that shapes v3, and it is worth stating plainly because
it is the opposite of the situation in other ecosystems.

PIT mutates JVM bytecode in memory: no rebuild, thousands of mutants a minute.
**cargo-mutants must recompile for every mutant.** Build and link dominate;
test execution is the cheap half.

Two consequences:

1. **Coverage-guided test selection buys little here.** Running only the tests
   that touch the mutated function is the standard mutation-testing
   optimization, and PIT does it natively. In Rust it optimizes the half that
   was not expensive. cargo-mutants does not implement it, and that is arguably
   a rational omission rather than a gap.
2. **Diff scoping is doubly important**, and it bounds *both* expensive halves.
   Per-test coverage is only needed for the tests in the diff, and mutants only
   for the symbols in the diff, so cost is `k changed symbols × m new tests`
   rather than `N × M`.

The real fix is single-compile, runtime-switched mutants — rewrite each body
once to `if mutant_active(ID) { Default::default() } else { <original> }`,
compile once, select by environment variable. [mutagen] proved the approach and
is unmaintained; [muttest] is the active attempt. That is the difference between
a nightly CI job and an interactive tool, and it is the hardest part: `const fn`
cannot host the switch, the branch perturbs inlining, and `Default` bounds do
not always hold.

[mutagen]: https://github.com/llogiq/mutagen
[muttest]: https://github.com/samuelpilz/muttest-rs

## Known gaps

- `no_run` doctests compile but never execute, so their assertions never run.
  They are currently linted as if they do.
- ORC010 fires only for `&mut self`. Detecting a mutated `&mut` argument needs
  dataflow we do not have.
- Async state machines and macro-heavy crates will stress the span join once v1
  lands; untested until there is coverage data to join against.
- Visibility is read from the item, so a `pub` item inside a private module is
  reported as public. Only affects the weakest claim edge.
- Coverage entry counts merge across binaries (a lib and the test harness that
  links it), so `entries > 1` cannot be read as a generic instantiation count.
- Doctests are not included in coverage unless `cargo llvm-cov --doctests` is
  passed, so a symbol covered *only* by a doctest currently reports as
  unexecuted.
