#![feature(const_precise_live_drops)]
#![feature(exhaustive_patterns)]
#![allow(non_camel_case_types)]

// Core modules (implemented)
pub mod cas;
pub mod crdt;
#[cfg(feature = "federation")]
pub mod federation;
pub mod graph;
pub mod index;
pub mod ingest;
pub mod prelude;
pub mod store;
pub mod utils;

// VM and Query modules (implemented)
pub mod query;
pub mod vm;
