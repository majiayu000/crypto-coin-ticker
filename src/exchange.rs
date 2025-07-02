//! # Exchange Module
//!
//! This module provides a high-level interface for connecting to cryptocurrency exchanges
//! and streaming real-time price data. It abstracts away the complexity of WebSocket
//! connections, reconnection logic, and error handling.
//!
//! ## Features
//! - Automatic reconnection on connection failures
//! - Support for multiple trading pairs simultaneously
//! - Configurable connection timeouts and ping intervals
//! - Structured price update events with timestamps
//! - Built on the `exc` crate for exchange connectivity
//!
//! ## Supported Exchanges
//! - OKX (primary implementation)
//! - Extensible architecture for additional exchanges
//!
//! ## Usage
//! ```rust
//! use okk::{Config, ExchangeClient};
//! use std::sync::mpsc::channel;
//!
//! let config = Config::default();
//! let client = ExchangeClient::new(config);
//! let (tx, rx) = channel();
//! // let handles = client.start_price_monitoring(tx).await?;
//! ```

use std::time::Duration;
use std::sync::mpsc::Sender;
use chrono;
use exc::prelude::*;

use futures::StreamExt;
use tokio::task::JoinHandle;
use rust_decimal::Decimal;
use crate::error::Result;
use crate::config::Config;

/// Exchange client wrapper for handling cryptocurrency price streams
pub struct ExchangeClient {
    config: Config,
}

impl ExchangeClient {
    /// Create a new exchange client with the given configuration
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Start monitoring price streams for all configured trading pairs with optimized resource usage
    pub async fn start_price_monitoring(&self, tx: Sender<PriceUpdate>) -> Result<Vec<JoinHandle<()>>> {
        tracing::info!("Starting price monitoring for {} pairs", self.config.trading_pairs.len());

        // Create a single exchange connection to be shared across all pairs
        let exchange = Okx::endpoint()
            .ws_ping_timeout(Duration::from_secs(self.config.ws_ping_timeout_secs))
            .ws_connection_timeout(Duration::from_secs(self.config.ws_connection_timeout_secs))
            .connect_exc();

        // Pre-allocate vector with known capacity to avoid reallocations
        let mut handles = Vec::with_capacity(self.config.trading_pairs.len());

        for pair in &self.config.trading_pairs {
            let client = exchange.clone();
            let tx = tx.clone();
            let pair = pair.clone(); // Clone only once per iteration
            let update_interval = Duration::from_secs(self.config.update_interval_secs);

            let handle = tokio::spawn(async move {
                Self::monitor_pair(client, tx, pair, update_interval).await;
            });

            handles.push(handle);
        }

        tracing::info!("Started {} monitoring tasks", handles.len());
        Ok(handles)
    }

    /// Monitor a single trading pair with automatic reconnection and error recovery
    async fn monitor_pair(
        mut client: impl exc::SubscribeTickersService + Clone + Send + 'static,
        tx: Sender<PriceUpdate>,
        pair: String,
        _update_interval: Duration,
    ) {
        let mut consecutive_errors = 0;
        const MAX_CONSECUTIVE_ERRORS: u32 = 5;
        const BASE_BACKOFF_SECS: u64 = 1;

        loop {
            tracing::info!("Starting monitoring for {}", pair);

            match client.subscribe_tickers(&pair).await {
                Ok(mut stream) => {
                    consecutive_errors = 0; // Reset error counter on successful connection
                    tracing::info!("Successfully connected to {} stream", pair);

                    while let Some(result) = stream.next().await {
                        match result {
                            Ok(ticker) => {
                                let update = PriceUpdate::new(pair.clone(), ticker.last);

                                tracing::debug!("{}: {}", pair, ticker.last);

                                if let Err(e) = tx.send(update) {
                                    tracing::error!("Channel closed, stopping monitoring for {}: {}", pair, e);
                                    return; // Exit if channel is closed
                                }
                            }
                            Err(err) => {
                                tracing::warn!("Stream error for {}: {}", pair, err);
                                break; // Break inner loop to reconnect
                            }
                        }
                    }
                    tracing::warn!("Stream for {} ended, attempting reconnection...", pair);
                }
                Err(err) => {
                    consecutive_errors += 1;
                    tracing::error!(
                        "Failed to subscribe to {} (attempt {}/{}): {}",
                        pair, consecutive_errors, MAX_CONSECUTIVE_ERRORS, err
                    );

                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        tracing::error!(
                            "Max consecutive errors reached for {}, backing off longer",
                            pair
                        );
                        tokio::time::sleep(Duration::from_secs(BASE_BACKOFF_SECS * 10)).await;
                        consecutive_errors = 0; // Reset after long backoff
                    }
                }
            }

            // Exponential backoff with simple jitter
            let backoff_secs = BASE_BACKOFF_SECS * 2_u64.pow(consecutive_errors.min(5));
            let jitter = (chrono::Utc::now().timestamp_millis() % (backoff_secs as i64 / 2 + 1)) as u64;
            let sleep_duration = Duration::from_secs(backoff_secs + jitter);

            tracing::info!("Waiting {:?} before reconnecting to {}", sleep_duration, pair);
            tokio::time::sleep(sleep_duration).await;
        }
    }
}

/// Price update information optimized for memory efficiency
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    /// Trading pair symbol (e.g., "BTC-USDT")
    pub pair: String,
    /// Current price as decimal
    pub price: Decimal,
    /// Unix timestamp in milliseconds for better performance
    pub timestamp_ms: i64,
}

impl PriceUpdate {
    /// Create a new price update with current timestamp
    pub fn new(pair: String, price: Decimal) -> Self {
        Self {
            pair,
            price,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Get the timestamp as a DateTime for display purposes
    pub fn datetime(&self) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.timestamp_millis_opt(self.timestamp_ms)
            .single()
            .unwrap_or_else(chrono::Utc::now)
    }
}
