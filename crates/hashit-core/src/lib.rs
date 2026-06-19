//! Shared core for the hashit tools.
//!
//! This crate is intentionally dependency-light: it holds the portable
//! `.hashit` manifest model, the hashing primitives, the scan engine, and
//! hash-aware file operations. Both the `hashit` scanner and the `hashit-idx`
//! search daemon build on it, so it pulls in neither the image/EXIF crates nor
//! SQLite nor the gRPC stack.

pub mod fileops;
pub mod hash;
pub mod manifest;
pub mod scan;
