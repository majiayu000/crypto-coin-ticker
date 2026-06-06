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
//! use okk::exchange::PriceUpdate;
//! use std::sync::mpsc::sync_channel;
//!
//! let config = Config::default();
//! let (tx, rx) = sync_channel::<PriceUpdate>(config.max_buffer_size);
//! let client = ExchangeClient::new(config);
//! // let handles = client.start_price_monitoring(tx).await?;
//! ```

use chrono;
use exc::prelude::*;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::Result;
use futures::StreamExt;
use rust_decimal::Decimal;
use tokio::task::JoinHandle;

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
    pub async fn start_price_monitoring(
        &self,
        tx: SyncSender<PriceUpdate>,
    ) -> Result<Vec<JoinHandle<()>>> {
        tracing::info!(
            "Starting price monitoring for {} pairs",
            self.config.trading_pairs.len()
        );

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
        tx: SyncSender<PriceUpdate>,
        pair: String,
        update_interval: Duration,
    ) {
        let mut consecutive_errors = 0;
        let mut last_sent_at = None;
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
                                let now = Instant::now();
                                if !should_emit_update(last_sent_at, now, update_interval) {
                                    continue;
                                }

                                let update = PriceUpdate::new(pair.clone(), ticker.last);

                                tracing::debug!("{}: {}", pair, ticker.last);

                                match tx.try_send(update) {
                                    Ok(()) => {
                                        last_sent_at = Some(now);
                                    }
                                    Err(TrySendError::Full(_)) => {
                                        tracing::warn!(
                                            "Price update buffer full for {}, dropping stale update",
                                            pair
                                        );
                                    }
                                    Err(TrySendError::Disconnected(_)) => {
                                        tracing::error!(
                                            "Channel closed, stopping monitoring for {}",
                                            pair
                                        );
                                        return; // Exit if channel is closed
                                    }
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
                        pair,
                        consecutive_errors,
                        MAX_CONSECUTIVE_ERRORS,
                        err
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
            let jitter =
                (chrono::Utc::now().timestamp_millis() % (backoff_secs as i64 / 2 + 1)) as u64;
            let sleep_duration = Duration::from_secs(backoff_secs + jitter);

            tracing::info!(
                "Waiting {:?} before reconnecting to {}",
                sleep_duration,
                pair
            );
            tokio::time::sleep(sleep_duration).await;
        }
    }
}

fn should_emit_update(
    last_sent_at: Option<Instant>,
    now: Instant,
    update_interval: Duration,
) -> bool {
    match last_sent_at {
        Some(last_sent_at) => now.duration_since(last_sent_at) >= update_interval,
        None => true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_interval_allows_first_update() {
        assert!(should_emit_update(
            None,
            Instant::now(),
            Duration::from_secs(10)
        ));
    }

    #[test]
    fn update_interval_throttles_recent_update() {
        let last_sent_at = Instant::now();
        let now = last_sent_at + Duration::from_secs(1);

        assert!(!should_emit_update(
            Some(last_sent_at),
            now,
            Duration::from_secs(10)
        ));
    }

    #[test]
    fn update_interval_allows_elapsed_update() {
        let last_sent_at = Instant::now();
        let now = last_sent_at + Duration::from_secs(10);

        assert!(should_emit_update(
            Some(last_sent_at),
            now,
            Duration::from_secs(10)
        ));
    }
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
        chrono::Utc
            .timestamp_millis_opt(self.timestamp_ms)
            .single()
            .unwrap_or_else(chrono::Utc::now)
    }
}
