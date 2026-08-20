# Crypto Coin Ticker

A lightweight, cross-platform system tray application for monitoring cryptocurrency prices in real-time. Built with Rust for performance and reliability.

## Features

- **Real-time Price Updates**: Live cryptocurrency price streaming from major exchanges
- **System Tray Integration**: Clean, unobtrusive system tray interface
- **Multi-pair Support**: Monitor multiple trading pairs simultaneously
- **Configurable**: TOML-based configuration with sensible defaults
- **Cross-platform**: Works on Windows, macOS, and Linux
- **Low Resource Usage**: Minimal CPU and memory footprint
- **Automatic Reconnection**: Robust error handling with exponential backoff
- **Performance Optimized**: Efficient memory usage and optimized for release builds

## Quick Start

### Installation

```bash
# Clone the repository
git clone https://github.com/your-username/crypto-coin-ticker.git
cd crypto-coin-ticker

# Build the application
cargo build --release

# Run the application
cargo run --release
```

### Configuration

Create a `config.toml` file in the project directory to customize the application:

```toml
# Trading pairs to monitor
trading_pairs = ["BTC-USDT", "ETH-USDT", "SOL-USDT"]

# Update interval in seconds
update_interval_secs = 1

# WebSocket timeouts
ws_connection_timeout_secs = 2
ws_ping_timeout_secs = 5

# UI customization
icon_path = "icons/icon.png"
tooltip = "Crypto Ticker - Real-time price updates"

# Performance settings
max_buffer_size = 1000
debug_logging = false
```

See `config.toml.example` for all available options.

## Release Status

This project is currently source-only. Build and run it from a local checkout until a tagged GitHub release or package artifact is published.

## Limitations

- Prices are displayed from exchange streaming APIs and can be delayed, interrupted, or unavailable.
- The app is a monitoring tool only and is not financial advice or a trading signal.
- Always verify price data with your exchange before making financial decisions.

## Supported Exchanges

- **OKX**: Primary exchange with full WebSocket streaming support
- Additional exchanges require a new streaming client implementation

## Architecture

The application is built with a clean, modular architecture:

- **Config Module**: Configuration management and TOML parsing
- **Exchange Module**: Exchange API integration and price streaming
- **UI Module**: System tray interface and event handling
- **Error Module**: Unified error handling and recovery

## Performance

The application is optimized for minimal resource usage:

- Efficient tokio runtime configuration
- Memory-optimized data structures
- Direct OKX public ticker WebSocket subscriptions
- Configurable buffer sizes
- Release build optimizations (LTO, strip symbols)

## Development

### Building

```bash
# Debug build
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test

# Check code
cargo check
```

### Logging

Set the `RUST_LOG` environment variable to control logging levels:

```bash
# Debug logging
RUST_LOG=debug cargo run

# App-specific logging
RUST_LOG=okk=debug cargo run
```

## License

This project is licensed under the MIT License - see the LICENSE file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
