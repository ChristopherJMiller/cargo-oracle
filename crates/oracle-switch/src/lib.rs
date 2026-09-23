#![warn(missing_docs)]
//! Runtime support for **single-compile mutation**.
//!
//! cargo-mutants recompiles the crate for every mutant, and in Rust build and
//! link dominate: a mutant costs seconds of compiler time and milliseconds of
//! test time. This crate is the other half of the trade — every mutation is
//! compiled in *once*, behind a runtime switch, and selected by an environment
//! variable. One build, then one test run per mutant.
//!
//! [`cargo-oracle`] rewrites each function body to:
//!
//! ```ignore
//! fn parse(s: &str) -> Result<Config, String> {
//!     if let Some(v) = oracle_switch::default_for!(Result<Config, String>) {
//!         if oracle_switch::active(17) {
//!             return v;
//!         }
//!     }
//!     // ... original body ...
//! }
//! ```
//!
//! # The problem this crate exists to solve
//!
//! Body replacement needs a value of the return type, and not every return type
//! implements [`Default`]. cargo-mutants can shrug that off: the mutant simply
//! fails to compile and is reported as unviable. Single-compile mutation
//! cannot — **one uncompilable default would break the entire build**, taking
//! every other mutant with it.
//!
//! [`default_for!`] resolves this with autoref specialization, which picks the
//! [`Default`] implementation when the type has one and a `None` fallback when
//! it does not — decided at compile time, with no trait bound on the caller and
//! no nightly features. A type without `Default` yields `None`, the `if let`
//! does not fire, and that mutant is inert rather than fatal. It is then
//! reported as unviable, exactly as cargo-mutants would have.
//!
//! # What the rewriter must still do itself
//!
//! [`default_for!`] only answers "does this type have a `Default`". Two cases
//! need the rewriter to look at the type first:
//!
//! - `Result<T, E>` has **no** `Default` in std -- there is no reason to prefer
//!   `Ok` over `Err`. Probing it directly would make every fallible function
//!   inert, which is most of a Rust codebase. The rewriter probes `T` and emits
//!   `Ok(v)`, matching cargo-mutants.
//! - `Option<T>` needs nothing special: `impl<T> Default for Option<T>` is
//!   unconditional, so the probe already yields `None` for any `T`.
//!
//! [`cargo-oracle`]: https://github.com/chrismiller/cargo-oracle

use std::sync::OnceLock;

/// The environment variable naming the active mutant.
pub const ENV: &str = "ORACLE_MUTANT";

/// Whether mutant `id` is the one selected for this process.
///
/// Reads [`ENV`] once and caches it: this sits on the hot path of every
/// instrumented function, so it must not parse the environment per call.
///
/// With no variable set — an ordinary `cargo test` of the rewritten tree —
/// nothing is active and every function runs its real body. That is the
/// baseline run, and it must pass before any mutant result means anything.
pub fn active(id: u32) -> bool {
    static SELECTED: OnceLock<Option<u32>> = OnceLock::new();
    *SELECTED.get_or_init(|| std::env::var(ENV).ok()?.trim().parse().ok()) == Some(id)
}

/// Probe type carrying `T` for [`default_for!`]. Not useful on its own.
pub struct Probe<T>(pub core::marker::PhantomData<T>);

impl<T> Probe<T> {
    /// Construct a probe for `T`.
    pub const fn new() -> Self {
        Probe(core::marker::PhantomData)
    }
}

impl<T> Default for Probe<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Specialized arm: chosen when `T: Default`.
///
/// Implemented for `&Probe<T>`, one autoref *closer* than [`ViaFallback`], so
/// method resolution reaches it first whenever the bound is satisfied.
pub trait ViaDefault<T> {
    /// A default value of `T`.
    fn oracle_default(&self) -> Option<T>;
}

impl<T: Default> ViaDefault<T> for &Probe<T> {
    fn oracle_default(&self) -> Option<T> {
        Some(T::default())
    }
}

/// Fallback arm: chosen when `T` has no [`Default`].
///
/// Implemented for `Probe<T>` with no bound, so it always applies, but sits one
/// autoref further away than [`ViaDefault`] and so only wins when that arm does
/// not apply.
pub trait ViaFallback<T> {
    /// No value is available; the mutation is inert.
    fn oracle_default(&self) -> Option<T>;
}

impl<T> ViaFallback<T> for Probe<T> {
    fn oracle_default(&self) -> Option<T> {
        None
    }
}

/// `Some(T::default())` when `T: Default`, otherwise `None`.
///
/// Resolved entirely at compile time, on stable Rust, without constraining the
/// caller. This is what lets every mutation be compiled in at once: a return
/// type with no `Default` makes its mutant inert instead of breaking the build.
///
/// ```
/// # use oracle_switch::default_for;
/// struct NoDefault(#[allow(dead_code)] u8);
///
/// assert_eq!(default_for!(u32), Some(0));
/// assert_eq!(default_for!(String), Some(String::new()));
/// assert!(default_for!(NoDefault).is_none());
/// ```
#[macro_export]
macro_rules! default_for {
    ($t:ty) => {{
        #[allow(unused_imports)]
        use $crate::{ViaDefault as _, ViaFallback as _};
        // Two autorefs: `&&Probe<T>` matches `ViaDefault for &Probe<T>` at the
        // first step when `T: Default`, and otherwise derefs one level to
        // `&Probe<T>` and matches `ViaFallback for Probe<T>`. One `&` fewer and
        // the fallback would win outright, for every type.
        (&&$crate::Probe::<$t>::new()).oracle_default()
    }};
}

#[cfg(test)]
mod tests {
    #[derive(Debug, PartialEq)]
    struct NoDefault(u8);

    #[derive(Debug, Default, PartialEq)]
    struct HasDefault {
        a: u8,
        b: String,
    }

    #[test]
    fn a_type_with_default_yields_its_default_value() {
        assert_eq!(default_for!(u32), Some(0));
        assert_eq!(default_for!(Option<u8>), Some(None));
        assert_eq!(
            default_for!(HasDefault),
            Some(HasDefault {
                a: 0,
                b: String::new()
            })
        );
    }

    #[test]
    fn a_type_without_default_yields_none_rather_than_failing_to_compile() {
        // The whole design rests on this: one uncompilable default would take
        // the entire build down, and every other mutant with it.
        assert_eq!(default_for!(NoDefault), None);
        assert_eq!(default_for!(&NoDefault), None);
    }

    #[test]
    fn the_unit_type_has_a_default_so_void_functions_are_mutable() {
        assert_eq!(default_for!(()), Some(()));
    }

    #[test]
    fn a_reference_resolves_through_its_own_default_impl() {
        assert_eq!(default_for!(&str), Some(""));
        assert_eq!(default_for!(&[u8]), Some(&[][..]));
    }

    #[test]
    fn result_has_no_default_which_is_why_the_rewriter_unwraps_it() {
        // `Result<T, E>` deliberately has no `Default` in std: there is no
        // reason to prefer `Ok` over `Err`. Probing it directly would make
        // every fallible function inert -- most of a Rust codebase.
        assert!(default_for!(Result<u8, String>).is_none());

        // So the rewriter probes the *success* type and wraps the result,
        // which is the same replacement cargo-mutants generates.
        assert_eq!(default_for!(u8).map(Ok::<u8, String>), Some(Ok(0)));
    }

    #[test]
    fn option_defaults_to_none_whatever_it_wraps() {
        // `impl<T> Default for Option<T>` is unconditional -- the default is
        // `None`, which exists for every `T`. So unlike `Result`, `Option`
        // needs no special handling in the rewriter at all.
        assert_eq!(default_for!(Option<u8>), Some(None));
        assert_eq!(default_for!(Option<NoDefault>), Some(None));
    }

    #[test]
    fn nothing_is_active_without_the_environment_variable() {
        // The baseline run: every function keeps its real body.
        assert!(!super::active(0));
        assert!(!super::active(17));
    }
}
