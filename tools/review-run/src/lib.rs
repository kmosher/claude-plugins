//! The review harness. See docs/review-harness.md for the design and the
//! build order; this crate is at step 2 of it — cold generators and one
//! critic, schema-enforced, mechanical nodes, and triggers that make lenses
//! conditional, with the capture bundle written by the harness.

pub mod conventions;
pub mod graph;
pub mod provider;
pub mod reuse;
pub mod reviewed;
pub mod run;
pub mod schema;
pub mod triggers;
