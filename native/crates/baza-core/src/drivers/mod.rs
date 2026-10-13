//! Database drivers: one module per engine family, each with a
//! `pub static SPEC: EngineSpec`, and the list of them all.
//!
//! This file is the only place that names every engine. How to add one is
//! written down in `README.md` next to it.
use crate::engine::EngineSpec;

mod session;
mod sqlx_support;

pub use session::{PageRequest, RowChange, Session};

/// Declare the driver modules and list their descriptors in this order,
/// which is also the order of the product picker.
macro_rules! engines {
    ($($name:ident,)+) => {
        $(pub mod $name;)+

        /// Every engine family this build can open.
        pub static REGISTRY: &[&EngineSpec] = &[$(&$name::SPEC,)+];
    };
}

// Adding an engine: its module file next to this one, and its name on a line
// of its own here. Nothing else in the workspace lists engines.
engines! {
    postgres,
    mysql,
    sqlite,
    mssql,
    redis,
    mongodb,
    clickhouse,
    duckdb,
    cassandra,
    dynamodb,
    oracle,
    elasticsearch,
}

/// Sentence for a result code that one of the drivers defines.
pub fn describe(code: &str, en: bool) -> Option<&'static str> {
    REGISTRY
        .iter()
        .filter_map(|spec| spec.describe)
        .find_map(|describe| describe(code, en))
}
