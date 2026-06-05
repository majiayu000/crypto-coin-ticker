//! # Configuration Module
//!
//! This module handles application configuration management, providing a clean interface
//! for loading, saving, and managing application settings. Configuration can be loaded
//! from TOML files or use sensible defaults.
//!
//! ## Features
//! - TOML-based configuration files
//! - Serde serialization/deserialization
//! - Sensible default values
//! - Validation and error handling
//! - Support for both relative and absolute icon paths
//!
//! ## Configuration Options
//! - Trading pairs to monitor
//! - Update intervals and timeouts
//! - UI customization (icon, tooltip)
//! - WebSocket connection parameters
//!
//! ## Usage
//! ```rust,no_run
//! use okk::{Config, Result};
//!
//! fn main() -> Result<()> {
//!     // Load from file
//!     let config = Config::from_file("config.toml")?;
//!
//!     // Use defaults
//!     let config = Config::default();
//!
//!     // Save to file
//!     config.save_to_file("config.toml")?;
//!
//!     Ok(())
//! }
//! ```

use crate::error::{Result, TickerError};
use serde::{Deserialize, Serialize};
use std::{io::ErrorKind, path::Path};

/// Default maximum buffer size for price updates
fn default_max_buffer_size() -> usize {
    1000
}

/// Default debug logging setting
fn default_debug_logging() -> bool {
    false
}

/// Application configuration with performance optimizations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Trading pairs to monitor
    pub trading_pairs: Vec<String>,
    /// Update interval in seconds
    pub update_interval_secs: u64,
    /// WebSocket connection timeout in seconds
    pub ws_connection_timeout_secs: u64,
    /// WebSocket ping timeout in seconds
    pub ws_ping_timeout_secs: u64,
    /// Icon path
    pub icon_path: String,
    /// Tray tooltip text
    pub tooltip: String,
    /// Maximum number of price updates to buffer (for memory management)
    #[serde(default = "default_max_buffer_size")]
    pub max_buffer_size: usize,
    /// Enable debug logging (impacts performance)
    #[serde(default = "default_debug_logging")]
    pub debug_logging: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trading_pairs: vec!["BTC-USDT".to_string()],
            update_interval_secs: 1,
            ws_connection_timeout_secs: 2,
            ws_ping_timeout_secs: 5,
            icon_path: "icons/icon.png".to_string(),
            tooltip: "Crypto Ticker - Real-time price updates".to_string(),
            max_buffer_size: default_max_buffer_size(),
            debug_logging: default_debug_logging(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| TickerError::ConfigError(format!("Failed to read config file: {}", e)))?;

        let config: Config = toml::from_str(&content)
            .map_err(|e| TickerError::ConfigError(format!("Failed to parse config: {}", e)))?;

        config.validate()?;

        Ok(config)
    }

    /// Load configuration from a file when it exists, otherwise use defaults.
    pub fn from_optional_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        match std::fs::metadata(path) {
            Ok(_) => Self::from_file(path),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(TickerError::ConfigError(format!(
                "Failed to inspect config file: {}",
                e
            ))),
        }
    }

    /// Save configuration to a TOML file
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| TickerError::ConfigError(format!("Failed to serialize config: {}", e)))?;

        std::fs::write(path, content)
            .map_err(|e| TickerError::ConfigError(format!("Failed to write config file: {}", e)))?;

        Ok(())
    }

    /// Get the full icon path
    pub fn get_icon_path(&self) -> String {
        if self.icon_path.starts_with('/') {
            self.icon_path.clone()
        } else {
            format!("{}/{}", env!("CARGO_MANIFEST_DIR"), self.icon_path)
        }
    }

    fn validate(&self) -> Result<()> {
        if self.update_interval_secs == 0 {
            return Err(TickerError::ConfigError(
                "update_interval_secs must be greater than zero".to_string(),
            ));
        }

        if self.max_buffer_size == 0 {
            return Err(TickerError::ConfigError(
                "max_buffer_size must be greater than zero".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path(name: &str) -> PathBuf {
        let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_nanos(),
            Err(e) => panic!("system time should be after Unix epoch: {e}"),
        };

        std::env::temp_dir().join(format!("crypto-coin-ticker-{name}-{nanos}.toml"))
    }

    #[test]
    fn missing_optional_config_uses_defaults() {
        let path = temp_config_path("missing");

        let config = match Config::from_optional_file(path) {
            Ok(config) => config,
            Err(e) => panic!("missing config should use defaults: {e}"),
        };

        assert_eq!(config, Config::default());
    }

    #[test]
    fn invalid_existing_config_returns_error() {
        let path = temp_config_path("invalid");
        if let Err(e) = std::fs::write(&path, "trading_pairs = [") {
            panic!("test config should be writable: {e}");
        }

        let error = match Config::from_optional_file(&path) {
            Ok(_) => panic!("invalid config should fail"),
            Err(error) => error,
        };

        if let Err(e) = std::fs::remove_file(path) {
            panic!("test config should be removable: {e}");
        }
        assert!(error.to_string().contains("Failed to parse config"));
    }

    #[test]
    fn zero_runtime_limits_are_rejected() {
        let path = temp_config_path("zero-limits");
        let zero_limit_config = r#"
trading_pairs = ["BTC-USDT"]
update_interval_secs = 0
ws_connection_timeout_secs = 2
ws_ping_timeout_secs = 5
icon_path = "icons/icon.png"
tooltip = "Crypto Ticker"
max_buffer_size = 0
debug_logging = false
"#;

        if let Err(e) = std::fs::write(&path, zero_limit_config) {
            panic!("test config should be writable: {e}");
        }

        let error = match Config::from_file(&path) {
            Ok(_) => panic!("zero values should fail validation"),
            Err(error) => error,
        };

        if let Err(e) = std::fs::remove_file(path) {
            panic!("test config should be removable: {e}");
        }
        assert!(
            error
                .to_string()
                .contains("update_interval_secs must be greater than zero")
        );
    }
}
