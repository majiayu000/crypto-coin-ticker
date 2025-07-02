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

use std::sync::mpsc::Receiver;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    TrayIconBuilder, TrayIconEvent,
};
use crate::config::Config;
use crate::error::{Result, TickerError};
use crate::exchange::PriceUpdate;

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

        let icon = self.load_icon()
            .map_err(|e| {
                tracing::error!("Failed to load tray icon: {}", e);
                e
            })?;

        let event_loop = EventLoopBuilder::new().build();

        // Create tray menu with error handling
        let tray_menu = Menu::new();
        let quit_item = MenuItem::new("Quit", true, None);

        tray_menu.append_items(&[
            &PredefinedMenuItem::separator(),
            &quit_item,
        ]).map_err(|e| TickerError::UIError(format!("Failed to create menu items: {}", e)))?;

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

        let mut last_price_update = std::time::Instant::now();
        let mut connection_status = "Connected";

        tracing::info!("Starting UI event loop");

        // Run event loop with comprehensive event handling
        event_loop.run(move |_event, _, control_flow| {
            *control_flow = ControlFlow::Poll;

            // Handle price updates with connection monitoring
            match price_rx.try_recv() {
                Ok(price_update) => {
                    last_price_update = std::time::Instant::now();
                    connection_status = "Connected";

                    // Use more efficient string formatting to reduce allocations
                    let title = format!("{}: ${:.2}", price_update.pair, price_update.price);
                    if let Some(ref mut tray) = tray_icon {
                        let _ = tray.set_title(Some(&title));
                    }

                    tracing::debug!("Updated tray with: {}", title);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Check for connection timeout
                    if last_price_update.elapsed() > std::time::Duration::from_secs(30) {
                        if connection_status != "Disconnected" {
                            connection_status = "Disconnected";
                            if let Some(ref mut tray) = tray_icon {
                                let _ = tray.set_title(Some("Disconnected"));
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
