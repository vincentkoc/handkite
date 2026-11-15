//! Optional, unsupported Apple-account/Anisette tooling for Handkite.
//!
//! This crate is excluded from the root workspace. Nothing in the Handkite
//! host, daemon, or MCP bridge depends on it.

pub mod anisette_v3;
pub mod developer_probe;
#[cfg(target_os = "macos")]
mod fs_anchor;
mod layout;
