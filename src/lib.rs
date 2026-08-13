//! Shared library surface for Reyn Studio host tooling.
//!
//! Slice 1 of the OCCT bridge work lives here so the `reyn-cad-bridge` stub
//! binary and Studio-side framing tests share one protocol implementation.
//! The desktop GUI binary (`src/main.rs`) remains a separate crate root and
//! does not yet route STEP import through this bridge.

#![allow(dead_code)]

pub mod cad_bridge;
pub mod cad_identity;
