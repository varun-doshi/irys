//! This crate is a dependency for both [chain] and [actors] crates. It exposes
//! database methods for reading and writing from the database as well as some
//! database value types.
pub mod config;
pub mod database;

/// When data is unconfirmed it is stored in db_cache tables. Once the data
/// (which is part of transactions and blocks) is well confirmed it moves from
/// the cache to one of the db_index tables.
/// Data in the caches can be pending or in a block still subject to re-org so
/// it is not suitable for mining.
pub mod db_cache;

/// Data in the indexes is confirmed data
pub mod db_index;
pub mod tables;

pub use database::*;
