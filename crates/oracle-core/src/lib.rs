//! Core analysis for `cargo-oracle`.
//!
//! The library is layered so each slice of the tool is independently useful:
//!
//! - [`symbol`] — the unit of attribution and its span-based identity.
//! - [`inventory`] — parse a workspace into symbols and tests.
//! - [`claims`] — which tests *claim* which symbols, statically.
//! - [`lint`] — whether a test's oracles could fail at all.
//!
//! Slices v1-v3 (coverage, per-test attribution, mutation verification) join
//! onto [`symbol::SymbolId`] and are added as further modules.

pub mod attribution;
pub mod claims;
pub mod coverage;
pub mod inventory;
pub mod lint;
pub mod report;
pub mod symbol;

pub use inventory::{Inventory, TestItem, TestKind};
pub use symbol::{Symbol, SymbolId};
