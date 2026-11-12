//! PMU configs: uncore event groups that a client (rhea-worker) sets at
//! runtime. While a config is set, the periodic collectors of its box types
//! count its groups instead of their default ones and export raw cumulative
//! counts, so analysis takes deltas between two samples.
//!
//! - `spec`: the JSON request and its validation into a programmed config.
//! - `types`: the programming and the raw counts the collectors export.

pub mod spec;
pub mod types;
