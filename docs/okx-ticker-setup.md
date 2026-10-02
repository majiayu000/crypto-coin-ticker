# Configure an OKX system tray price ticker

Crypto Coin Ticker displays prices received from OKX's public ticker WebSocket.
It is a source-distributed Rust application, and the Cargo executable is named
`okk`. See [build instructions](../README.md#installation) and the
[complete configuration example](../config.toml.example).

## Run with a configuration in the expected directory

From the repository root, after building:

```bash
cp config.toml.example config.toml
cargo run --release
```

The app reads `config.toml` from its current working directory. If it is absent,
it uses defaults, including `BTC-USDT`. An existing malformed file is an error;
it is not silently replaced with defaults. Launching the same binary from a
second directory can therefore select different configuration.

To change the watchlist, edit the existing `trading_pairs` field before restarting:

```toml
trading_pairs = ["BTC-USDT", "ETH-USDT"]
```

Use OKX instrument IDs, keep pairs unique, and leave the other required example
fields in place. A single shared connection subscribes to the configured pairs;
this is not an exchange-aggregated price feed.

## Prices are missing or stop changing

Run from a terminal to inspect the existing logs:

```bash
RUST_LOG=okk=debug cargo run --release
```

- **Configuration error:** check TOML syntax, duplicate pairs, and the working
  directory. The update interval and buffer size must be positive.
- **Subscription rejected:** confirm that the instrument ID is available on OKX.
  Subscription errors, connection failures, and pong deadlines are reported in
  the logs; an unsupported pair is not proof that every configured pair failed.
- **Timeout or disconnect:** check connectivity to OKX's public WebSocket and the
  example's connection/ping settings. Automatic reconnects do not establish that
  a displayed value is fresh during an interruption.
- **Icon problem:** check `icon_path` and whether the referenced file exists. The
  provided example uses `icons/icon.png`; an absolute path is also supported.

Record the commit, OS, watchlist, and relevant error when filing an
[issue](https://github.com/majiayu000/crypto-coin-ticker/issues/new?template=bug_report.yml).

## Do I need an exchange API key or trading account?

The implemented client connects to OKX's **public ticker channel** without private
account authentication. It displays market data; it does not place orders or
manage a portfolio. Prices can be delayed or unavailable.

For a packaged Mac menu-bar interface, the separate
[Coin Tick App Store listing](https://apps.apple.com/us/app/coin-tick-menu-bar-crypto/id1141688067)
provides that product's own installation and feature details. This repository's
current path is a Rust source build with direct OKX streaming; no feature or
latency comparison has been measured.

[Supported exchanges](../README.md#supported-exchanges) · [Configuration](../README.md#configuration) ·
[MIT license](../LICENSE)
