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
    let request = match build_okx_subscribe_request("ticker-0", "BTC-USDT") {
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
            "id": "ticker-0",
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
fn okx_subscription_ack_validates_pair_and_channel() {
    let response = parse_okx_subscription_response(
        r#"{
            "id": "ticker-0",
            "event": "subscribe",
            "arg": {"channel": "tickers", "instId": "BTC-USDT"},
            "connId": "a4d3ae55"
        }"#,
    )
    .expect("valid acknowledgement");
    assert_eq!(
        response,
        Some(OkxSubscriptionResponse::Acknowledged {
            request_id: "ticker-0".to_string(),
            pair: "BTC-USDT".to_string(),
        })
    );

    let error = parse_okx_subscription_response(
        r#"{"id":"ticker-0","event":"subscribe","arg":{"channel":"books","instId":"BTC-USDT"}}"#,
    )
    .expect_err("unexpected channel must fail");
    assert!(error.to_string().contains("unexpected channel"));
}

#[test]
fn subscription_setup_keeps_ticker_when_another_pair_is_rejected() {
    let mut pending = HashMap::from([
        ("ticker-0".to_string(), "BTC-USDT".to_string()),
        ("ticker-1".to_string(), "BAD-USDT".to_string()),
    ]);

    let acknowledgement = classify_subscription_frame(
        r#"{
            "id": "ticker-0",
            "event": "subscribe",
            "arg": {"channel": "tickers", "instId": "BTC-USDT"}
        }"#,
        &mut pending,
    )
    .expect("valid pair should be acknowledged");
    assert_eq!(
        acknowledgement,
        OkxSubscriptionFrame::Acknowledged("BTC-USDT".to_string())
    );

    let ticker = classify_subscription_frame(
        r#"{
            "arg": {"channel": "tickers", "instId": "BTC-USDT"},
            "data": [{"instId": "BTC-USDT", "last": "9999.99"}]
        }"#,
        &mut pending,
    )
    .expect("ticker should be processed while another acknowledgement is pending");
    assert_eq!(
        ticker,
        OkxSubscriptionFrame::Updates(vec![OkxTicker {
            pair: "BTC-USDT".to_string(),
            last: price("9999.99"),
        }])
    );

    let rejection = classify_subscription_frame(
        r#"{
            "id": "ticker-1",
            "event": "error",
            "code": "60012",
            "msg": "Invalid request"
        }"#,
        &mut pending,
    )
    .expect("rejected pair should be isolated by request id");
    assert!(matches!(
        rejection,
        OkxSubscriptionFrame::Rejected { pair, error }
            if pair == "BAD-USDT" && error.contains("60012")
    ));
    assert!(pending.is_empty());
}

#[test]
fn malformed_ticker_item_does_not_discard_valid_updates() {
    let updates = parse_okx_ticker_updates(
        r#"{
            "arg": {"channel": "tickers", "instId": "BTC-USDT"},
            "data": [
                {"instId": "BTC-USDT", "last": "9999.99"},
                {"instId": "MISSING-LAST"},
                {"instId": "BAD-PRICE", "last": "not-a-number"}
            ]
        }"#,
    )
    .expect("a malformed ticker item should not fail the frame");

    assert_eq!(
        updates,
        vec![OkxTicker {
            pair: "BTC-USDT".to_string(),
            last: price("9999.99"),
        }]
    );
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
