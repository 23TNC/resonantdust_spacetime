//! Bit-packing helpers — single source of truth lives in the shared
//! DSL crate at `resonantdust_codec::packed`.
//!
//! This module exists so `use crate::packed::*;` keeps resolving for
//! every shard call site without each one having to know that the
//! definitions actually come from `resonantdust_codec`. Glob-re-
//! exports the entire `packed` module verbatim — every function,
//! constant, and the `StackedState` enum.
//!
//! Tests for these helpers live alongside their definitions in the
//! shared crate (`bin/shared test`). No shard-side duplication.

pub use resonantdust_codec::packed::*;
