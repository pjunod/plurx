//! plurx core: the storage boundary, configuration, and shared domain types.
//!
//! The single most load-bearing decision in this crate is the [`store::Store`]
//! trait: *all* replicated durable state (users, settings, library metadata,
//! watch state) is accessed through it. Phase 0–2 run a single-node SQLite
//! backend; Phase 3–4 swap in a raft-replicated backend (hiqlite, or
//! openraft + SQLite) behind the same trait. Nothing outside this crate may
//! assume which backend is in play. See `docs/ARCHITECTURE.md` §2.

pub mod auth;
pub mod cluster;
pub mod config;
pub mod domain;
pub mod error;
pub mod fmp4;
pub mod mediafacts;
pub mod metadata;
pub mod playback;
pub mod scan;
pub mod store;
/// Media fixtures for the test suites, shared so `plurx-core` and `plurxd`
/// cannot drift onto different GOP structures and disagree about what the
/// classifier should have said. Not part of the shipped library.
#[cfg(any(test, feature = "fixtures"))]
pub mod testfixtures;
pub mod tracks;
pub mod trakt;
pub mod transcode;
