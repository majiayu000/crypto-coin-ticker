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
//! Prefer `RUST_LOG` when set. Otherwise `debug_logging = true` in `config.toml`
//! enables `okk=debug`; the default filter is `okk=info`.
//! ```bash
//! RUST_LOG=debug cargo run
//! RUST_LOG=okk=debug cargo run
//! ```

use okk::{Config, ExchangeClient, TrayUI};
use std::sync::mpsc::sync_channel;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration before logging so debug_logging can set the default filter
    let config = Config::from_optional_file("config.toml")?;

    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::EnvFilter::new(
            config.resolve_log_filter(),
        ));
    tracing_subscriber::registry().with(fmt).init();

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
