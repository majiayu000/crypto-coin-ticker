//! # UI Module
//!
//! This module handles the system tray user interface for the crypto ticker application.
//! It provides a clean interface for displaying real-time cryptocurrency prices in the
//! system tray with menu controls for user interaction.
//!
//! ## Features
//! - System tray icon with real-time price updates
//! - Context menu with quit functionality
//! - Cross-platform support via tao and tray-icon crates
//! - Configurable icon and tooltip text
//!
//! ## Usage
//! ```rust
//! use okk::{Config, TrayUI};
//! use std::sync::mpsc::Receiver;
//!
//! let config = Config::default();
//! let tray_ui = TrayUI::new(config);
//! // tray_ui.run(price_receiver)?;
//! ```

use crate::config::Config;
use crate::error::{Result, TickerError};
use crate::exchange::{Price, PriceUpdate};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

/// Apply a price update onto the per-pair cache, clearing on reconnect.
///
/// When `connection_generation` advances, prior-connection prices are dropped so
/// unrecovered pairs after a partial reconnect cannot linger beside live ones.
/// Quiet but healthy pairs on the *same* generation are retained.
pub(crate) fn apply_price_update(
    prices: &mut HashMap<String, Price>,
    current_generation: &mut u64,
    update: &PriceUpdate,
) {
    if update.connection_generation != *current_generation {
        prices.clear();
        *current_generation = update.connection_generation;
    }
    prices.insert(update.pair.clone(), update.price.clone());
}

/// Format a combined tray title from configured pairs and the latest known prices.
///
/// Pairs are rendered in first-seen `configured_pairs` order (duplicates skipped).
/// Pairs without a price yet are omitted. Returns an empty string when no prices
/// are available.
pub(crate) fn format_combined_title(
    configured_pairs: &[String],
    prices: &HashMap<String, Price>,
) -> String {
    let mut seen = HashSet::new();
    configured_pairs
        .iter()
        .filter(|pair| seen.insert(pair.as_str()))
        .filter_map(|pair| {
            prices
                .get(pair)
                .map(|price| format!("{}: ${}", pair, price.format_with_precision(2)))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Tray UI manager for displaying cryptocurrency prices in the system tray
pub struct TrayUI {
    config: Config,
}

impl TrayUI {
    /// Create a new tray UI with the given configuration
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the tray UI event loop with comprehensive error handling
    pub fn run(self, price_rx: Receiver<PriceUpdate>) -> Result<()> {
        tracing::info!("Initializing system tray UI");

        let icon = self.load_icon().map_err(|e| {
            tracing::error!("Failed to load tray icon: {}", e);
            e
        })?;

        let event_loop = EventLoopBuilder::new().build();

        // Create tray menu with error handling
        let tray_menu = Menu::new();
        let quit_item = MenuItem::new("Quit", true, None);

        tray_menu
            .append_items(&[&PredefinedMenuItem::separator(), &quit_item])
            .map_err(|e| TickerError::UIError(format!("Failed to create menu items: {}", e)))?;

        // Create tray icon with detailed error context
        let mut tray_icon = Some(
            TrayIconBuilder::new()
                .with_id("crypto-ticker")
                .with_menu(Box::new(tray_menu))
                .with_title("Initializing...")
                .with_tooltip(&self.config.tooltip)
                .with_icon(icon)
                .build()
                .map_err(|e| {
                    tracing::error!("Failed to create system tray icon: {}", e);
                    TickerError::UIError(format!("Failed to create tray icon: {}. Make sure your system supports system tray functionality.", e))
                })?
        );

        tracing::info!("System tray initialized successfully");

        // Setup event channels with error handling
        let menu_channel = MenuEvent::receiver();
        let tray_channel = TrayIconEvent::receiver();

        let mut last_price_update = Instant::now();
        let mut connection_status = "Connected";
        let configured_pairs = self.config.trading_pairs.clone();
        // Cached prices keyed by pair; cleared when connection_generation advances.
        let mut latest_prices: HashMap<String, Price> = HashMap::new();
        let mut price_generation = 0_u64;

        tracing::info!("Starting UI event loop");

        // Run event loop with comprehensive event handling
        event_loop.run(move |_event, _, control_flow| {
            *control_flow = ControlFlow::Poll;

            // Handle price updates with connection monitoring
            match price_rx.try_recv() {
                Ok(price_update) => {
                    last_price_update = Instant::now();
                    connection_status = "Connected";

                    apply_price_update(&mut latest_prices, &mut price_generation, &price_update);
                    let title = format_combined_title(&configured_pairs, &latest_prices);
                    if let Some(ref mut tray) = tray_icon {
                        tray.set_title(Some(&title));
                    }

                    tracing::debug!("Updated tray with: {}", title);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Check for connection timeout
                    if last_price_update.elapsed() > Duration::from_secs(30) {
                        if connection_status != "Disconnected" {
                            connection_status = "Disconnected";
                            // Drop cached prices so a later reconnect cannot resurrect
                            // stale pair values beside freshly recovered ones.
                            latest_prices.clear();
                            price_generation = 0;
                            if let Some(ref mut tray) = tray_icon {
                                tray.set_title(Some("Disconnected"));
                            }
                            tracing::warn!("No price updates received for 30 seconds");
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::error!("Price update channel disconnected, shutting down UI");
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }

            // Handle menu events
            if let Ok(event) = menu_channel.try_recv() {
                tracing::debug!("Menu event received: {:?}", event.id);
                if event.id == quit_item.id() {
                    tracing::info!("Quit requested by user");
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                }
            }

            // Handle tray events (clicks, etc.)
            if let Ok(event) = tray_channel.try_recv() {
                tracing::debug!("Tray event received: {:?}", event);
                // Future: Handle tray click events for additional functionality
            }
        })
    }

    /// Load the tray icon from the configured path
    fn load_icon(&self) -> Result<tray_icon::Icon> {
        let icon_path = self.config.get_icon_path();
        let path = std::path::Path::new(&icon_path);

        let (icon_rgba, icon_width, icon_height) = {
            let image = image::open(path)
                .map_err(|e| TickerError::UIError(format!("Failed to open icon: {}", e)))?
                .into_rgba8();
            let (width, height) = image.dimensions();
            let rgba = image.into_raw();
            (rgba, width, height)
        };

        tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height)
            .map_err(|e| TickerError::UIError(format!("Failed to create icon: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(raw: &str) -> Price {
        Price::parse(raw).expect("valid price")
    }

    fn update(pair: &str, raw_price: &str, generation: u64) -> PriceUpdate {
        PriceUpdate {
            pair: pair.to_string(),
            price: price(raw_price),
            timestamp_ms: 0,
            connection_generation: generation,
        }
    }

    #[test]
    fn format_combined_title_single_pair() {
        let pairs = vec!["BTC-USDT".to_string()];
        let mut prices = HashMap::new();
        prices.insert("BTC-USDT".to_string(), price("10000.5"));

        assert_eq!(
            format_combined_title(&pairs, &prices),
            "BTC-USDT: $10000.50"
        );
    }

    #[test]
    fn format_combined_title_multi_pair_preserves_config_order() {
        let pairs = vec![
            "BTC-USDT".to_string(),
            "ETH-USDT".to_string(),
            "SOL-USDT".to_string(),
        ];
        let mut prices = HashMap::new();
        // Insert out of order to ensure output follows configured order
        prices.insert("ETH-USDT".to_string(), price("3000"));
        prices.insert("SOL-USDT".to_string(), price("150.25"));
        prices.insert("BTC-USDT".to_string(), price("65000.1"));

        assert_eq!(
            format_combined_title(&pairs, &prices),
            "BTC-USDT: $65000.10 | ETH-USDT: $3000.00 | SOL-USDT: $150.25"
        );
    }

    #[test]
    fn format_combined_title_upsert_overwrites_same_pair() {
        let pairs = vec!["BTC-USDT".to_string(), "ETH-USDT".to_string()];
        let mut prices = HashMap::new();
        prices.insert("BTC-USDT".to_string(), price("100"));
        prices.insert("ETH-USDT".to_string(), price("200"));
        prices.insert("BTC-USDT".to_string(), price("101.5"));

        assert_eq!(
            format_combined_title(&pairs, &prices),
            "BTC-USDT: $101.50 | ETH-USDT: $200.00"
        );
    }

    #[test]
    fn format_combined_title_omits_pairs_without_price() {
        let pairs = vec![
            "BTC-USDT".to_string(),
            "ETH-USDT".to_string(),
            "SOL-USDT".to_string(),
        ];
        let mut prices = HashMap::new();
        prices.insert("ETH-USDT".to_string(), price("2500"));

        assert_eq!(format_combined_title(&pairs, &prices), "ETH-USDT: $2500.00");
    }

    #[test]
    fn format_combined_title_empty_when_no_prices() {
        let pairs = vec!["BTC-USDT".to_string(), "ETH-USDT".to_string()];
        let prices = HashMap::new();

        assert_eq!(format_combined_title(&pairs, &prices), "");
    }

    #[test]
    fn format_combined_title_dedupes_duplicate_configured_pairs() {
        let pairs = vec![
            "BTC-USDT".to_string(),
            "ETH-USDT".to_string(),
            "BTC-USDT".to_string(),
        ];
        let mut prices = HashMap::new();
        prices.insert("BTC-USDT".to_string(), price("100"));
        prices.insert("ETH-USDT".to_string(), price("200"));

        assert_eq!(
            format_combined_title(&pairs, &prices),
            "BTC-USDT: $100.00 | ETH-USDT: $200.00"
        );
    }

    #[test]
    fn apply_price_update_keeps_quiet_pairs_on_same_generation() {
        let mut prices = HashMap::new();
        let mut generation = 0;
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "100", 1));
        apply_price_update(&mut prices, &mut generation, &update("ETH-USDT", "200", 1));
        // Only BTC updates again; ETH stays cached because generation is unchanged.
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "101", 1));

        assert_eq!(generation, 1);
        assert_eq!(prices.len(), 2);
        assert_eq!(
            prices.get("BTC-USDT").map(|p| p.format_with_precision(2)),
            Some("101.00".to_string())
        );
        assert!(prices.contains_key("ETH-USDT"));
    }

    #[test]
    fn apply_price_update_clears_prior_generation_on_reconnect() {
        let mut prices = HashMap::new();
        let mut generation = 0;
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "100", 1));
        apply_price_update(&mut prices, &mut generation, &update("ETH-USDT", "200", 1));
        // Partial reconnect: only BTC recovers on generation 2.
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "105", 2));

        assert_eq!(generation, 2);
        assert_eq!(prices.len(), 1);
        assert!(prices.contains_key("BTC-USDT"));
        assert!(!prices.contains_key("ETH-USDT"));
    }
}
