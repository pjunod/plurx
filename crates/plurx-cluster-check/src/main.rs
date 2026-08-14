//! Entry point for the separate-process cluster harness.
//!
//! Every mode lives in the crate's library so the harness itself is reachable
//! from tests. This file stays a wrapper on purpose: `cargo run -p
//! plurx-cluster-check -- check` behaves exactly as it did when the same code
//! was a single binary target.

use anyhow::Result;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    plurx_cluster_check::run(std::env::args().collect::<Vec<_>>()).await
}
