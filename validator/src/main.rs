#![feature(portable_simd)]
#![feature(random)]
#![feature(sync_unsafe_cell)]
#![feature(ptr_as_ref_unchecked)]

use tokio;
use tracing::info;

mod validator;

#[tokio::main]
async fn main() {
    // Initialize logging with tracing
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    // Create and initialize validator
    info!("Initializing validator...");
    validator::Validator::new().await.run().await;
}
