//! Bit-packing helpers — single source of truth lives in the
//! content crate at [`content/src/packed.rs`](../../../../../content/src/packed.rs).
//!
//! This module exists so `use crate::packed::*;` keeps resolving for
//! every shard call site without each one having to know that the
//! definitions actually come from `resonantdust_content`. Glob-re-
//! exports the entire content `packed` module verbatim — every
//! function, constant, and the `StackedState` enum.
//!
//! Tests for these helpers live alongside their definitions in the
//! content crate (`bin/content test`). No shard-side duplication.

pub use resonantdust_content::packed::*;
