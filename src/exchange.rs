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
use std::collections::{HashMap, HashSet};
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

        if self.config.trading_pairs.is_empty() {
            return Ok(Vec::new());
        }

        let pairs = self.config.trading_pairs.clone();
        let update_interval = Duration::from_secs(self.config.update_interval_secs);
        let connection_timeout = Duration::from_secs(self.config.ws_connection_timeout_secs.max(1));
        let pong_timeout = Duration::from_secs(self.config.ws_ping_timeout_secs.max(1));
        let handle = tokio::spawn(async move {
            Self::monitor_pairs(tx, pairs, update_interval, connection_timeout, pong_timeout).await;
        });

        tracing::info!("Started one shared OKX monitoring task");
        Ok(vec![handle])
    }

    /// Monitor all configured pairs over one OKX connection.
    async fn monitor_pairs(
        tx: SyncSender<PriceUpdate>,
        pairs: Vec<String>,
        update_interval: Duration,
        connection_timeout: Duration,
        pong_timeout: Duration,
    ) {
        let mut consecutive_errors = 0_u32;
        let mut last_sent_at = HashMap::<String, Instant>::new();
        const BASE_BACKOFF_SECS: u64 = 1;

        loop {
            tracing::info!("Connecting to OKX for {} pairs", pairs.len());
            let result = Self::run_connection(
                &tx,
                &pairs,
                &mut last_sent_at,
                &mut consecutive_errors,
                update_interval,
                connection_timeout,
                pong_timeout,
            )
            .await;

            if matches!(result, Err(TickerError::ChannelError(_))) {
                tracing::error!("Price update channel closed; stopping OKX monitoring");
                return;
            }
            if let Err(err) = result {
                consecutive_errors = consecutive_errors.saturating_add(1);
                tracing::warn!(
                    "OKX connection attempt {} failed: {}",
                    consecutive_errors,
                    err
                );
            }

            // Exponential backoff with simple jitter
            let backoff_secs = BASE_BACKOFF_SECS * 2_u64.pow(consecutive_errors.min(5));
            let jitter =
                (chrono::Utc::now().timestamp_millis() % (backoff_secs as i64 / 2 + 1)) as u64;
            let sleep_duration = Duration::from_secs(backoff_secs + jitter);

            tracing::info!(
                "Waiting {:?} before reconnecting to OKX",
                sleep_duration,
            );
            tokio::time::sleep(sleep_duration).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_connection(
        tx: &SyncSender<PriceUpdate>,
        pairs: &[String],
        last_sent_at: &mut HashMap<String, Instant>,
        consecutive_errors: &mut u32,
        update_interval: Duration,
        connection_timeout: Duration,
        pong_timeout: Duration,
    ) -> Result<()> {
        let (stream, _response) = tokio::time::timeout(
            connection_timeout,
            connect_async(OKX_PUBLIC_WS_URL),
        )
        .await
        .map_err(|_| TickerError::NetworkError("Timed out connecting to OKX".to_string()))?
        .map_err(|err| TickerError::NetworkError(format!("Failed to connect to OKX: {err}")))?;
        let (mut write, mut read) = stream.split();

        let subscribe_request = build_okx_subscribe_request(pairs)?;
        write
            .send(Message::Text(subscribe_request.into()))
            .await
            .map_err(|err| TickerError::NetworkError(format!("Failed to subscribe: {err}")))?;

        let mut pending: HashSet<String> = pairs.iter().cloned().collect();
        tokio::time::timeout(connection_timeout, async {
            while !pending.is_empty() {
                match read.next().await {
                    Some(Ok(Message::Text(raw))) => {
                        if raw == OKX_PONG {
                            continue;
                        }
                        if let Some(pair) = parse_okx_subscription_ack(&raw)? {
                            if !pending.remove(&pair) {
                                return Err(TickerError::ExchangeError(format!(
                                    "Unexpected OKX subscription acknowledgement for {pair}"
                                )));
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => write
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| TickerError::NetworkError(err.to_string()))?,
                    Some(Ok(Message::Close(frame))) => {
                        return Err(TickerError::NetworkError(format!(
                            "OKX closed before subscription confirmation: {frame:?}"
                        )));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(TickerError::NetworkError(err.to_string())),
                    None => {
                        return Err(TickerError::NetworkError(
                            "OKX ended before subscription confirmation".to_string(),
                        ));
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            TickerError::NetworkError("Timed out waiting for OKX subscription confirmation".to_string())
        })??;

        tracing::info!("OKX confirmed {} ticker subscriptions", pairs.len());
        let mut ping_interval = tokio::time::interval(pong_timeout);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut awaiting_pong_since: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    if awaiting_pong_since.is_some_and(|sent| sent.elapsed() >= pong_timeout) {
                        return Err(TickerError::NetworkError("OKX pong deadline exceeded".to_string()));
                    }
                    if awaiting_pong_since.is_none() {
                        write.send(Message::Text(OKX_PING.into())).await
                            .map_err(|err| TickerError::NetworkError(format!("Failed to ping OKX: {err}")))?;
                        awaiting_pong_since = Some(Instant::now());
                    }
                }
                result = read.next() => match result {
                    Some(Ok(Message::Text(raw))) => {
                        if raw == OKX_PONG {
                            awaiting_pong_since = None;
                            continue;
                        }
                        let updates = parse_okx_ticker_updates(&raw)?;
                        if !updates.is_empty() {
                            *consecutive_errors = 0;
                        }
                        for ticker in updates {
                            let now = Instant::now();
                            if !should_emit_update(last_sent_at.get(&ticker.pair).copied(), now, update_interval) {
                                continue;
                            }
                            let pair = ticker.pair.clone();
                            match tx.try_send(PriceUpdate::new(ticker.pair, ticker.last)) {
                                Ok(()) => {
                                    last_sent_at.insert(pair, now);
                                }
                                Err(TrySendError::Full(_)) => tracing::warn!(
                                    "Price update buffer full for {pair}, dropping stale update"
                                ),
                                Err(TrySendError::Disconnected(_)) => return Err(
                                    TickerError::ChannelError("Price update receiver closed".to_string())
                                ),
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => write.send(Message::Pong(payload)).await
                        .map_err(|err| TickerError::NetworkError(err.to_string()))?,
                    Some(Ok(Message::Close(frame))) => return Err(TickerError::NetworkError(
                        format!("OKX stream closed: {frame:?}")
                    )),
                    Some(Ok(Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => {}
                    Some(Err(err)) => return Err(TickerError::NetworkError(err.to_string())),
                    None => return Err(TickerError::NetworkError("OKX stream ended".to_string())),
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct OkxSubscribeRequest<'a> {
    op: &'static str,
    args: Vec<OkxSubscribeArg<'a>>,
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
    arg: Option<OkxResponseArg>,
    data: Option<Vec<OkxTickerData>>,
}

#[derive(Debug, Deserialize)]
struct OkxResponseArg {
    channel: String,
    #[serde(rename = "instId")]
    inst_id: String,
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

fn build_okx_subscribe_request(pairs: &[String]) -> Result<String> {
    let request = OkxSubscribeRequest {
        op: "subscribe",
        args: pairs
            .iter()
            .map(|pair| OkxSubscribeArg {
                channel: OKX_TICKERS_CHANNEL,
                inst_id: pair,
            })
            .collect(),
    };

    serde_json::to_string(&request).map_err(|e| {
        TickerError::ExchangeError(format!("Failed to serialize OKX subscription: {}", e))
    })
}

fn parse_okx_subscription_ack(raw: &str) -> Result<Option<String>> {
    let payload: OkxTickerPayload = serde_json::from_str(raw)
        .map_err(|err| TickerError::ExchangeError(format!("Invalid OKX message: {err}")))?;
    if payload.event.as_deref() == Some("error") {
        return Err(okx_payload_error(payload));
    }
    if payload.event.as_deref() != Some("subscribe") {
        return Ok(None);
    }
    let arg = payload.arg.ok_or_else(|| {
        TickerError::ExchangeError("OKX subscription acknowledgement omitted arg".to_string())
    })?;
    if arg.channel != OKX_TICKERS_CHANNEL {
        return Err(TickerError::ExchangeError(format!(
            "OKX acknowledged unexpected channel {}",
            arg.channel
        )));
    }
    Ok(Some(arg.inst_id))
}

fn okx_payload_error(payload: OkxTickerPayload) -> TickerError {
    let code = payload.code.unwrap_or_else(|| "unknown".to_string());
    let msg = payload
        .msg
        .unwrap_or_else(|| "OKX WebSocket error".to_string());
    TickerError::ExchangeError(format!("OKX WebSocket error {code}: {msg}"))
}

fn parse_okx_ticker_updates(raw: &str) -> Result<Vec<OkxTicker>> {
    if raw == OKX_PONG || raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    let payload: OkxTickerPayload = serde_json::from_str(raw)
        .map_err(|e| TickerError::ExchangeError(format!("Invalid OKX message: {}", e)))?;

    if payload.event.as_deref() == Some("error") {
        return Err(okx_payload_error(payload));
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
        let request = match build_okx_subscribe_request(&[
            "BTC-USDT".to_string(),
            "ETH-USDT".to_string(),
        ]) {
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
                    },
                    {
                        "channel": "tickers",
                        "instId": "ETH-USDT"
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
    fn okx_subscription_ack_validates_pair_and_channel() {
        let pair = parse_okx_subscription_ack(
            r#"{
                "event": "subscribe",
                "arg": {"channel": "tickers", "instId": "BTC-USDT"},
                "connId": "a4d3ae55"
            }"#,
        )
        .expect("valid acknowledgement");
        assert_eq!(pair.as_deref(), Some("BTC-USDT"));

        let error = parse_okx_subscription_ack(
            r#"{"event":"subscribe","arg":{"channel":"books","instId":"BTC-USDT"}}"#,
        )
        .expect_err("unexpected channel must fail");
        assert!(error.to_string().contains("unexpected channel"));
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
