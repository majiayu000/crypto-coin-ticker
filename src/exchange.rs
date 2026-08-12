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
//! - Direct OKX public WebSocket integration
//!
//! ## Supported Exchanges
//! - OKX public ticker channel
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

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config::Config;
use crate::error::{Result, TickerError};

const OKX_PUBLIC_WS_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
const OKX_TICKERS_CHANNEL: &str = "tickers";
const OKX_PING: &str = "ping";
const OKX_PONG: &str = "pong";

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

        // Pre-allocate vector with known capacity to avoid reallocations
        let mut handles = Vec::with_capacity(self.config.trading_pairs.len());

        for pair in &self.config.trading_pairs {
            let tx = tx.clone();
            let pair = pair.clone(); // Clone only once per iteration
            let update_interval = Duration::from_secs(self.config.update_interval_secs);
            let connection_timeout = Duration::from_secs(self.config.ws_connection_timeout_secs);
            let ping_timeout = Duration::from_secs(self.config.ws_ping_timeout_secs.max(1));

            let handle = tokio::spawn(async move {
                Self::monitor_pair(tx, pair, update_interval, connection_timeout, ping_timeout)
                    .await;
            });

            handles.push(handle);
        }

        tracing::info!("Started {} monitoring tasks", handles.len());
        Ok(handles)
    }

    /// Monitor a single trading pair with automatic reconnection and error recovery
    async fn monitor_pair(
        tx: SyncSender<PriceUpdate>,
        pair: String,
        update_interval: Duration,
        connection_timeout: Duration,
        ping_timeout: Duration,
    ) {
        let mut consecutive_errors = 0;
        let mut last_sent_at = None;
        const MAX_CONSECUTIVE_ERRORS: u32 = 5;
        const BASE_BACKOFF_SECS: u64 = 1;

        loop {
            tracing::info!("Starting monitoring for {}", pair);

            match tokio::time::timeout(connection_timeout, connect_async(OKX_PUBLIC_WS_URL)).await {
                Ok(Ok((stream, _response))) => {
                    consecutive_errors = 0; // Reset error counter on successful connection
                    tracing::info!("Successfully connected to {} stream", pair);

                    let (mut write, mut read) = stream.split();
                    let subscribe_request = match build_okx_subscribe_request(&pair) {
                        Ok(request) => request,
                        Err(err) => {
                            tracing::error!(
                                "Failed to build OKX subscription for {}: {}",
                                pair,
                                err
                            );
                            return;
                        }
                    };

                    if let Err(err) = write.send(Message::Text(subscribe_request.into())).await {
                        consecutive_errors += 1;
                        tracing::warn!("Failed to subscribe to {}: {}", pair, err);
                    } else {
                        tracing::info!("Subscribed to OKX ticker stream for {}", pair);
                        let mut ping_interval = tokio::time::interval(ping_timeout);

                        loop {
                            tokio::select! {
                                _ = ping_interval.tick() => {
                                    if let Err(err) = write.send(Message::Text(OKX_PING.into())).await {
                                        tracing::warn!("Failed to send OKX ping for {}: {}", pair, err);
                                        break;
                                    }
                                }
                                result = read.next() => {
                                    match result {
                                        Some(Ok(message)) => {
                                            match message {
                                                Message::Text(raw) => {
                                                    let raw = raw.to_string();
                                                    match parse_okx_ticker_updates(&raw) {
                                                        Ok(updates) => {
                                                            for ticker in updates {
                                                                let now = Instant::now();
                                                                if !should_emit_update(last_sent_at, now, update_interval) {
                                                                    continue;
                                                                }

                                                                tracing::debug!("{}: {}", ticker.pair, ticker.last);

                                                                match tx.try_send(PriceUpdate::new(ticker.pair, ticker.last)) {
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
                                                        }
                                                        Err(err) => {
                                                            tracing::warn!("Failed to parse OKX ticker message for {}: {}", pair, err);
                                                            break;
                                                        }
                                                    }
                                                }
                                                Message::Ping(payload) => {
                                                    if let Err(err) = write.send(Message::Pong(payload)).await {
                                                        tracing::warn!("Failed to answer OKX ping for {}: {}", pair, err);
                                                        break;
                                                    }
                                                }
                                                Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                                                Message::Close(frame) => {
                                                    tracing::warn!("OKX stream for {} closed: {:?}", pair, frame);
                                                    break;
                                                }
                                            }
                                        }
                                        Some(Err(err)) => {
                                            tracing::warn!("Stream error for {}: {}", pair, err);
                                            break; // Break inner loop to reconnect
                                        }
                                        None => {
                                            tracing::warn!("Stream for {} ended, attempting reconnection...", pair);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Err(err)) => {
                    consecutive_errors += 1;
                    tracing::error!(
                        "Failed to connect to OKX for {} (attempt {}/{}): {}",
                        pair,
                        consecutive_errors,
                        MAX_CONSECUTIVE_ERRORS,
                        err
                    );
                }
                Err(_elapsed) => {
                    consecutive_errors += 1;
                    tracing::error!(
                        "Timed out connecting to OKX for {} (attempt {}/{})",
                        pair,
                        consecutive_errors,
                        MAX_CONSECUTIVE_ERRORS
                    );
                }
            }

            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                tracing::error!(
                    "Max consecutive errors reached for {}, backing off longer",
                    pair
                );
                tokio::time::sleep(Duration::from_secs(BASE_BACKOFF_SECS * 10)).await;
                consecutive_errors = 0; // Reset after long backoff
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

#[derive(Debug, Serialize)]
struct OkxSubscribeRequest<'a> {
    op: &'static str,
    args: [OkxSubscribeArg<'a>; 1],
}

#[derive(Debug, Serialize)]
struct OkxSubscribeArg<'a> {
    channel: &'static str,
    #[serde(rename = "instId")]
    inst_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct OkxTickerPayload {
    event: Option<String>,
    code: Option<String>,
    msg: Option<String>,
    data: Option<Vec<OkxTickerData>>,
}

#[derive(Debug, Deserialize)]
struct OkxTickerData {
    #[serde(rename = "instId")]
    inst_id: String,
    last: String,
}

#[derive(Debug, PartialEq)]
struct OkxTicker {
    pair: String,
    last: Price,
}

fn build_okx_subscribe_request(pair: &str) -> Result<String> {
    let request = OkxSubscribeRequest {
        op: "subscribe",
        args: [OkxSubscribeArg {
            channel: OKX_TICKERS_CHANNEL,
            inst_id: pair,
        }],
    };

    serde_json::to_string(&request).map_err(|e| {
        TickerError::ExchangeError(format!("Failed to serialize OKX subscription: {}", e))
    })
}

fn parse_okx_ticker_updates(raw: &str) -> Result<Vec<OkxTicker>> {
    if raw == OKX_PONG || raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    let payload: OkxTickerPayload = serde_json::from_str(raw)
        .map_err(|e| TickerError::ExchangeError(format!("Invalid OKX message: {}", e)))?;

    if payload.event.as_deref() == Some("error") {
        let code = payload.code.unwrap_or_else(|| "unknown".to_string());
        let msg = payload
            .msg
            .unwrap_or_else(|| "OKX WebSocket error".to_string());
        return Err(TickerError::ExchangeError(format!(
            "OKX WebSocket error {}: {}",
            code, msg
        )));
    }

    payload
        .data
        .unwrap_or_default()
        .into_iter()
        .map(|ticker| {
            let last = Price::parse(&ticker.last).map_err(|_e| {
                TickerError::ExchangeError(format!(
                    "Invalid OKX ticker price for {}: {}",
                    ticker.inst_id, ticker.last
                ))
            })?;

            Ok(OkxTicker {
                pair: ticker.inst_id,
                last,
            })
        })
        .collect()
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

/// Price text received from an exchange, validated as a decimal number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    raw: String,
}

impl Price {
    /// Parse a decimal price string from an exchange payload.
    pub fn parse(raw: &str) -> Result<Self> {
        if !is_valid_decimal(raw) {
            return Err(TickerError::ExchangeError(format!(
                "Invalid price: {}",
                raw
            )));
        }

        Ok(Self {
            raw: raw.to_string(),
        })
    }

    /// Return the exact price text received from the exchange.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Format the price with a fixed number of fractional digits.
    pub fn format_with_precision(&self, precision: usize) -> String {
        format_decimal_with_precision(&self.raw, precision)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn is_valid_decimal(raw: &str) -> bool {
    if raw.trim() != raw {
        return false;
    }

    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    if unsigned.is_empty() {
        return false;
    }

    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();

    if parts.next().is_some() || whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }

    match fraction {
        Some(fraction) => !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()),
        None => true,
    }
}

fn format_decimal_with_precision(raw: &str, precision: usize) -> String {
    let (sign, unsigned) = match raw.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", raw),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let mut whole_digits = normalized_integer_digits(whole).into_bytes();
    let mut fraction_digits = vec![b'0'; precision];

    for (index, digit) in fraction.bytes().take(precision).enumerate() {
        fraction_digits[index] = digit;
    }

    if fraction
        .as_bytes()
        .get(precision)
        .is_some_and(|digit| *digit >= b'5')
    {
        increment_fixed_decimal(&mut whole_digits, &mut fraction_digits);
    }

    let whole = String::from_utf8(whole_digits).unwrap_or_else(|_| "0".to_string());
    if precision == 0 {
        return format!("{sign}{whole}");
    }

    let fraction = String::from_utf8(fraction_digits).unwrap_or_default();
    format!("{sign}{whole}.{fraction}")
}

fn normalized_integer_digits(whole: &str) -> String {
    let trimmed = whole.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn increment_fixed_decimal(whole_digits: &mut Vec<u8>, fraction_digits: &mut [u8]) {
    for digit in fraction_digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            return;
        }
    }

    for digit in whole_digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            return;
        }
    }

    whole_digits.insert(0, b'1');
}

/// Price update information optimized for memory efficiency
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    /// Trading pair symbol (e.g., "BTC-USDT")
    pub pair: String,
    /// Current price received from the exchange
    pub price: Price,
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

    #[test]
    fn okx_subscription_request_uses_tickers_channel_and_inst_id() {
        let request = match build_okx_subscribe_request("BTC-USDT") {
            Ok(request) => request,
            Err(e) => panic!("subscription request should serialize: {e}"),
        };
        let request: serde_json::Value = match serde_json::from_str(&request) {
            Ok(request) => request,
            Err(e) => panic!("subscription request should be JSON: {e}"),
        };

        assert_eq!(
            request,
            serde_json::json!({
                "op": "subscribe",
                "args": [
                    {
                        "channel": "tickers",
                        "instId": "BTC-USDT"
                    }
                ]
            })
        );
    }

    #[test]
    fn price_formats_with_fixed_precision() {
        let price = match Price::parse("9999.999") {
            Ok(price) => price,
            Err(e) => panic!("price should parse: {e}"),
        };

        assert_eq!(price.format_with_precision(2), "10000.00");
    }

    #[test]
    fn okx_ticker_message_converts_last_price_to_price() {
        let updates = match parse_okx_ticker_updates(
            r#"{
                "arg": {"channel": "tickers", "instId": "BTC-USDT"},
                "data": [
                    {
                        "instType": "SPOT",
                        "instId": "BTC-USDT",
                        "last": "9999.99",
                        "ts": "1597026383085"
                    }
                ]
            }"#,
        ) {
            Ok(updates) => updates,
            Err(e) => panic!("ticker message should parse: {e}"),
        };

        assert_eq!(
            updates,
            vec![OkxTicker {
                pair: "BTC-USDT".to_string(),
                last: price("9999.99"),
            }]
        );
    }

    #[test]
    fn okx_subscription_ack_is_ignored() {
        let updates = match parse_okx_ticker_updates(
            r#"{
                "event": "subscribe",
                "arg": {"channel": "tickers", "instId": "BTC-USDT"},
                "connId": "a4d3ae55"
            }"#,
        ) {
            Ok(updates) => updates,
            Err(e) => panic!("subscription acknowledgement should parse: {e}"),
        };

        assert!(updates.is_empty());
    }

    #[test]
    fn okx_error_event_reports_exchange_error() {
        let error = match parse_okx_ticker_updates(
            r#"{
                "event": "error",
                "code": "60012",
                "msg": "Invalid request"
            }"#,
        ) {
            Ok(_) => panic!("error event should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("OKX WebSocket error 60012"));
    }

    fn price(raw: &str) -> Price {
        match Price::parse(raw) {
            Ok(price) => price,
            Err(e) => panic!("price should parse: {e}"),
        }
    }
}

impl PriceUpdate {
    /// Create a new price update with current timestamp
    pub fn new(pair: String, price: Price) -> Self {
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
