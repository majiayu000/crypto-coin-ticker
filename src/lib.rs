//! # Crypto Coin Ticker
//!
//! A lightweight, cross-platform system tray application for monitoring cryptocurrency prices
//! in real-time. Built with Rust for performance and reliability.
//!
//! ## Features
//! - **Real-time Price Updates**: Live cryptocurrency price streaming from major exchanges
//! - **System Tray Integration**: Clean, unobtrusive system tray interface
//! - **Multi-pair Support**: Monitor multiple trading pairs simultaneously
//! - **Configurable**: TOML-based configuration with sensible defaults
//! - **Cross-platform**: Works on Windows, macOS, and Linux
//! - **Low Resource Usage**: Minimal CPU and memory footprint
//!
//! ## Quick Start
//! ```rust
//! use okk::{Config, ExchangeClient, TrayUI};
//! use std::sync::mpsc::channel;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = Config::default();
//!     let (tx, rx) = channel();
//!
//!     let exchange_client = ExchangeClient::new(config.clone());
//!     let _handles = exchange_client.start_price_monitoring(tx).await?;
//!
//!     let tray_ui = TrayUI::new(config);
//!     tray_ui.run(rx)?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Architecture
//! The application is structured into several key modules:
//! - [`config`]: Configuration management and TOML parsing
//! - [`error`]: Unified error handling and custom error types
//! - [`exchange`]: Exchange API integration and price streaming
//! - [`ui`]: System tray user interface and event handling
//!
//! ## Configuration
//! Create a `config.toml` file to customize the application:
//! ```toml
//! trading_pairs = ["BTC-USDT", "ETH-USDT"]
//! update_interval_secs = 1
//! icon_path = "icons/icon.png"
//! tooltip = "Crypto Ticker"
//! ```

pub mod config;
pub mod error;
pub mod exchange;
pub mod ui;

pub use config::Config;
pub use error::{Result, TickerError};
pub use exchange::ExchangeClient;
pub use ui::TrayUI;
