//! # Error Handling Module
//!
//! This module provides a unified error handling system for the crypto ticker application.
//! It defines custom error types that provide clear, actionable error messages and proper
//! error propagation throughout the application.
//!
//! ## Error Categories
//! - **ExchangeError**: Issues with exchange API connections or data
//! - **ConfigError**: Configuration file parsing or validation errors
//! - **UIError**: System tray or user interface related errors
//! - **NetworkError**: Network connectivity and communication errors
//! - **ChannelError**: Inter-thread communication failures
//!
//! ## Features
//! - Structured error types with context
//! - Automatic conversion from common error types
//! - Display formatting for user-friendly error messages
//! - Integration with `anyhow` for error chaining
//!
//! ## Usage
//! ```rust
//! use okk::{Result, TickerError};
//!
//! fn example_function() -> Result<()> {
//!     // Function that might fail
//!     Err(TickerError::ConfigError("Invalid configuration".to_string()))
//! }
//! ```

use std::fmt;

/// Custom error type for the crypto ticker application
#[derive(Debug)]
pub enum TickerError {
    /// Exchange connection errors
    ExchangeError(String),
    /// Configuration errors
    ConfigError(String),
    /// UI/Tray errors
    UIError(String),
    /// Network errors
    NetworkError(String),
    /// Channel communication errors
    ChannelError(String),
}

impl fmt::Display for TickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TickerError::ExchangeError(msg) => write!(f, "Exchange error: {}", msg),
            TickerError::ConfigError(msg) => write!(f, "Configuration error: {}", msg),
            TickerError::UIError(msg) => write!(f, "UI error: {}", msg),
            TickerError::NetworkError(msg) => write!(f, "Network error: {}", msg),
            TickerError::ChannelError(msg) => write!(f, "Channel error: {}", msg),
        }
    }
}

impl std::error::Error for TickerError {}

impl From<anyhow::Error> for TickerError {
    fn from(err: anyhow::Error) -> Self {
        TickerError::ExchangeError(err.to_string())
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for TickerError {
    fn from(err: std::sync::mpsc::SendError<T>) -> Self {
        TickerError::ChannelError(err.to_string())
    }
}

/// Result type alias for the application
pub type Result<T> = std::result::Result<T, TickerError>;
