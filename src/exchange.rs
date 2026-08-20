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
use std::collections::{HashMap, HashSet};
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

            tracing::info!("Waiting {:?} before reconnecting to OKX", sleep_duration,);
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
        let (stream, _response) =
            tokio::time::timeout(connection_timeout, connect_async(OKX_PUBLIC_WS_URL))
                .await
                .map_err(|_| TickerError::NetworkError("Timed out connecting to OKX".to_string()))?
                .map_err(|err| {
                    TickerError::NetworkError(format!("Failed to connect to OKX: {err}"))
                })?;
        let (mut write, mut read) = stream.split();

        let mut pending = HashMap::<String, String>::new();
        for (index, pair) in pairs.iter().enumerate() {
            let request_id = format!("ticker-{index}");
            let subscribe_request = build_okx_subscribe_request(&request_id, pair)?;
            write
                .send(Message::Text(subscribe_request.into()))
                .await
                .map_err(|err| {
                    TickerError::NetworkError(format!("Failed to subscribe to {pair}: {err}"))
                })?;
            pending.insert(request_id, pair.clone());
        }

        let mut active_pairs = HashSet::<String>::new();
        let confirmation_result = tokio::time::timeout(connection_timeout, async {
            while !pending.is_empty() {
                match read.next().await {
                    Some(Ok(Message::Text(raw))) => {
                        match classify_subscription_frame(&raw, &mut pending)? {
                            OkxSubscriptionFrame::Acknowledged(pair) => {
                                active_pairs.insert(pair);
                            }
                            OkxSubscriptionFrame::Rejected { pair, error } => {
                                tracing::warn!(
                                    "OKX rejected ticker subscription for {pair}: {error}"
                                );
                            }
                            OkxSubscriptionFrame::Updates(updates) => Self::emit_ticker_updates(
                                tx,
                                last_sent_at,
                                consecutive_errors,
                                update_interval,
                                updates,
                            )?,
                            OkxSubscriptionFrame::Pong | OkxSubscriptionFrame::Other => {}
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
        .await;

        match confirmation_result {
            Ok(result) => result?,
            Err(_) if active_pairs.is_empty() => {
                return Err(TickerError::NetworkError(
                    "Timed out waiting for any OKX subscription confirmation".to_string(),
                ));
            }
            Err(_) => tracing::warn!(
                "Timed out waiting for {} OKX subscription confirmations; keeping {} confirmed pairs active",
                pending.len(),
                active_pairs.len()
            ),
        }

        if active_pairs.is_empty() {
            return Err(TickerError::ExchangeError(
                "OKX rejected all ticker subscriptions".to_string(),
            ));
        }

        tracing::info!(
            "OKX confirmed {} of {} ticker subscriptions",
            active_pairs.len(),
            pairs.len()
        );
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
                        if let Some(response) = parse_okx_subscription_response(&raw)? {
                            match response {
                                OkxSubscriptionResponse::Acknowledged { request_id, pair } => {
                                    tracing::debug!(
                                        "Received late OKX subscription acknowledgement {request_id} for {pair}"
                                    );
                                }
                                OkxSubscriptionResponse::Rejected { request_id, error } => {
                                    tracing::warn!(
                                        "OKX rejected subscription request {request_id}: {error}"
                                    );
                                }
                            }
                            continue;
                        }
                        let updates = parse_okx_ticker_updates(&raw)?;
                        Self::emit_ticker_updates(
                            tx,
                            last_sent_at,
                            consecutive_errors,
                            update_interval,
                            updates,
                        )?;
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

    fn emit_ticker_updates(
        tx: &SyncSender<PriceUpdate>,
        last_sent_at: &mut HashMap<String, Instant>,
        consecutive_errors: &mut u32,
        update_interval: Duration,
        updates: Vec<OkxTicker>,
    ) -> Result<()> {
        if !updates.is_empty() {
            *consecutive_errors = 0;
        }
        for ticker in updates {
            let now = Instant::now();
            if !should_emit_update(
                last_sent_at.get(&ticker.pair).copied(),
                now,
                update_interval,
            ) {
                continue;
            }
            let pair = ticker.pair.clone();
            match tx.try_send(PriceUpdate::new(ticker.pair, ticker.last)) {
                Ok(()) => {
                    last_sent_at.insert(pair, now);
                }
                Err(TrySendError::Full(_)) => {
                    tracing::warn!("Price update buffer full for {pair}, dropping stale update")
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(TickerError::ChannelError(
                        "Price update receiver closed".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct OkxSubscribeRequest<'a> {
    id: &'a str,
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
    id: Option<String>,
    event: Option<String>,
    code: Option<String>,
    msg: Option<String>,
    arg: Option<OkxResponseArg>,
    data: Option<Vec<serde_json::Value>>,
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

#[derive(Debug, PartialEq)]
enum OkxSubscriptionResponse {
    Acknowledged { request_id: String, pair: String },
    Rejected { request_id: String, error: String },
}

#[derive(Debug, PartialEq)]
enum OkxSubscriptionFrame {
    Acknowledged(String),
    Rejected { pair: String, error: String },
    Updates(Vec<OkxTicker>),
    Pong,
    Other,
}

fn build_okx_subscribe_request(request_id: &str, pair: &str) -> Result<String> {
    let request = OkxSubscribeRequest {
        id: request_id,
        op: "subscribe",
        args: vec![OkxSubscribeArg {
            channel: OKX_TICKERS_CHANNEL,
            inst_id: pair,
        }],
    };

    serde_json::to_string(&request).map_err(|e| {
        TickerError::ExchangeError(format!("Failed to serialize OKX subscription: {}", e))
    })
}

fn parse_okx_subscription_response(raw: &str) -> Result<Option<OkxSubscriptionResponse>> {
    let payload: OkxTickerPayload = serde_json::from_str(raw)
        .map_err(|err| TickerError::ExchangeError(format!("Invalid OKX message: {err}")))?;
    if payload.event.as_deref() == Some("error") {
        let request_id = payload.id.clone().ok_or_else(|| {
            TickerError::ExchangeError("OKX subscription error omitted request id".to_string())
        })?;
        return Ok(Some(OkxSubscriptionResponse::Rejected {
            request_id,
            error: okx_payload_error(payload).to_string(),
        }));
    }
    if payload.event.as_deref() != Some("subscribe") {
        return Ok(None);
    }
    let request_id = payload.id.ok_or_else(|| {
        TickerError::ExchangeError(
            "OKX subscription acknowledgement omitted request id".to_string(),
        )
    })?;
    let arg = payload.arg.ok_or_else(|| {
        TickerError::ExchangeError("OKX subscription acknowledgement omitted arg".to_string())
    })?;
    if arg.channel != OKX_TICKERS_CHANNEL {
        return Err(TickerError::ExchangeError(format!(
            "OKX acknowledged unexpected channel {}",
            arg.channel
        )));
    }
    Ok(Some(OkxSubscriptionResponse::Acknowledged {
        request_id,
        pair: arg.inst_id,
    }))
}

fn classify_subscription_frame(
    raw: &str,
    pending: &mut HashMap<String, String>,
) -> Result<OkxSubscriptionFrame> {
    if raw == OKX_PONG {
        return Ok(OkxSubscriptionFrame::Pong);
    }

    match parse_okx_subscription_response(raw)? {
        Some(OkxSubscriptionResponse::Acknowledged { request_id, pair }) => {
            let expected_pair = pending.remove(&request_id).ok_or_else(|| {
                TickerError::ExchangeError(format!(
                    "Unexpected OKX subscription acknowledgement id {request_id}"
                ))
            })?;
            if expected_pair != pair {
                return Err(TickerError::ExchangeError(format!(
                    "OKX subscription {request_id} acknowledged {pair} instead of {expected_pair}"
                )));
            }
            Ok(OkxSubscriptionFrame::Acknowledged(pair))
        }
        Some(OkxSubscriptionResponse::Rejected { request_id, error }) => {
            let pair = pending.remove(&request_id).ok_or_else(|| {
                TickerError::ExchangeError(format!(
                    "Unexpected OKX subscription error id {request_id}"
                ))
            })?;
            Ok(OkxSubscriptionFrame::Rejected { pair, error })
        }
        None => {
            let updates = parse_okx_ticker_updates(raw)?;
            if updates.is_empty() {
                Ok(OkxSubscriptionFrame::Other)
            } else {
                Ok(OkxSubscriptionFrame::Updates(updates))
            }
        }
    }
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

    let updates = payload
        .data
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| {
            let ticker = match serde_json::from_value::<OkxTickerData>(value) {
                Ok(ticker) => ticker,
                Err(err) => {
                    tracing::warn!("Ignoring malformed OKX ticker item: {err}");
                    return None;
                }
            };
            let last = match Price::parse(&ticker.last) {
                Ok(last) => last,
                Err(err) => {
                    tracing::warn!(
                        "Ignoring invalid OKX ticker price for {}: {}",
                        ticker.inst_id,
                        err
                    );
                    return None;
                }
            };

            Some(OkxTicker {
                pair: ticker.inst_id,
                last,
            })
        })
        .collect();

    Ok(updates)
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
mod tests;

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
