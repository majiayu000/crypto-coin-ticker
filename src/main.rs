//! # Crypto Coin Ticker - Main Application Entry Point
//!
//! This is the main entry point for the crypto coin ticker application. It orchestrates
//! the initialization of all components including configuration loading, exchange client
//! setup, and system tray UI initialization.
//!
//! ## Application Flow
//! 1. Initialize logging system with configurable levels
//! 2. Load configuration from file or use defaults
//! 3. Create communication channels between components
//! 4. Start exchange client for price monitoring
//! 5. Launch system tray UI and enter event loop
//!
//! ## Configuration
//! The application looks for a `config.toml` file in the current directory.
//! If not found, it uses sensible defaults. See `config.toml.example` for
//! configuration options.
//!
//! ## Logging
//! Set the `RUST_LOG` environment variable to control logging levels:
//! ```bash
//! RUST_LOG=debug cargo run
//! RUST_LOG=exc_okx=debug,okx_streams=debug cargo run
//! ```

use okk::{Config, ExchangeClient, TrayUI};
use std::sync::mpsc::sync_channel;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "exc_okx=debug,okx_streams=debug".into()),
        ));
    tracing_subscriber::registry().with(fmt).init();

    // Load configuration
    let config = Config::from_optional_file("config.toml")?;

    // Create bounded communication channel
    let (tx, rx) = sync_channel(config.max_buffer_size);

    // Start exchange client
    let exchange_client = ExchangeClient::new(config.clone());
    let _handles = exchange_client.start_price_monitoring(tx).await?;

    // Start tray UI
    let tray_ui = TrayUI::new(config);
    tray_ui.run(rx)?;

    Ok(())
}
