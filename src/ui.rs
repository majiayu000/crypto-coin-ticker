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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

/// Silence window before the tray shows a disconnected status.
///
/// Must be at least twice `update_interval_secs` so intentional emission
/// throttling cannot look like a dead connection when the interval is >30s.
pub(crate) fn disconnect_silence_timeout(update_interval_secs: u64) -> Duration {
    Duration::from_secs(update_interval_secs.saturating_mul(2).max(30))
}

/// Idle scheduling for the tray event loop.
///
/// A disconnect deadline still in the future waits until that instant. Once
/// `now` has reached it, wait for a real event instead: tao 0.34 treats a past
/// `WaitUntil` as a repeating poll on macOS and a non-blocking resume on Linux.
///
/// A future `WaitUntil` also never wakes tao 0.34 on Linux, because the gtk
/// iteration has no timeout. The disconnect sleeper posts `DisconnectDeadline`
/// for that case. Switching to `Wait` once the deadline is due still keeps a
/// past `WaitUntil` from spinning.
pub(crate) fn idle_control_flow(
    last_price_update: Instant,
    disconnect_after: Duration,
    now: Instant,
) -> ControlFlow {
    let deadline = last_price_update + disconnect_after;
    if now < deadline {
        ControlFlow::WaitUntil(deadline)
    } else {
        ControlFlow::Wait
    }
}

enum UserEvent {
    Price(PriceUpdate),
    PriceChannelClosed,
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    /// The silence window elapsed. Wakes the tao loop; the one-shot title flip
    /// stays on `MainEventsCleared` so a price queued in the same wake is applied first.
    DisconnectDeadline,
}

/// Latest silence deadline shared with the single disconnect sleeper.
///
/// `generation` advances only when the deadline changes, so the sleeper posts
/// one event per price time and then waits to be re-armed.
struct DisconnectDeadlineArm {
    state: Mutex<DisconnectDeadlineState>,
    wake: Condvar,
}

struct DisconnectDeadlineState {
    deadline: Instant,
    generation: u64,
}

impl DisconnectDeadlineArm {
    fn new(deadline: Instant) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(DisconnectDeadlineState {
                deadline,
                generation: 1,
            }),
            wake: Condvar::new(),
        })
    }

    /// Point the sleeper at `deadline`. The same deadline leaves it alone.
    fn rearm(&self, deadline: Instant) {
        let mut state = lock_deadline(&self.state);
        if state.deadline == deadline {
            return;
        }
        state.deadline = deadline;
        state.generation = state.generation.wrapping_add(1);
        self.wake.notify_one();
    }
}

fn lock_deadline(
    state: &Mutex<DisconnectDeadlineState>,
) -> std::sync::MutexGuard<'_, DisconnectDeadlineState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sleep until the current deadline, then call `fire` once.
///
/// A later [`DisconnectDeadlineArm::rearm`] abandons the old wait. `fire`
/// returns false when the event loop is gone and the sleeper should exit.
fn spawn_disconnect_sleeper(
    arm: Arc<DisconnectDeadlineArm>,
    mut fire: impl FnMut() -> bool + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut handled_generation = 0_u64;
        loop {
            let (deadline, generation) = {
                let mut state = lock_deadline(&arm.state);
                while state.generation == handled_generation {
                    state = arm
                        .wake
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                (state.deadline, state.generation)
            };

            loop {
                let state = lock_deadline(&arm.state);
                if state.generation != generation {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    drop(state);
                    if !fire() {
                        return;
                    }
                    handled_generation = generation;
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                let (state, _) = arm
                    .wake
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                drop(state);
            }
        }
    });
}

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

        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

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

        // Handlers replace the tray-icon channels. Once set, those receivers stay empty.
        let proxy = event_loop.create_proxy();
        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = menu_proxy.send_event(UserEvent::Menu(event));
        }));
        let tray_proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = tray_proxy.send_event(UserEvent::Tray(event));
        }));

        // The exchange keeps its bounded channel. This thread blocks on it and
        // forwards each accepted PriceUpdate, which wakes the tao loop between ticks.
        // should_emit_update and try_send already drop ticks before they get here.
        let price_proxy = proxy.clone();
        std::thread::spawn(move || {
            while let Ok(update) = price_rx.recv() {
                if price_proxy.send_event(UserEvent::Price(update)).is_err() {
                    return;
                }
            }
            let _ = price_proxy.send_event(UserEvent::PriceChannelClosed);
        });

        let mut last_price_update = Instant::now();
        let mut connection_status = "Connected";
        let configured_pairs = self.config.trading_pairs.clone();
        let disconnect_after = disconnect_silence_timeout(self.config.update_interval_secs);
        // Cached prices keyed by pair; cleared when connection_generation advances.
        // Do not clear on silence timeout — throttled same-generation pairs must survive.
        let mut latest_prices: HashMap<String, Price> = HashMap::new();
        let mut price_generation = 0_u64;

        // One sleeper, re-armed only when last_price_update changes. tao 0.34 on
        // Linux never wakes a future WaitUntil, so this post is what lets
        // MainEventsCleared flip the title to Disconnected.
        let deadline_arm = DisconnectDeadlineArm::new(last_price_update + disconnect_after);
        spawn_disconnect_sleeper(Arc::clone(&deadline_arm), move || {
            proxy.send_event(UserEvent::DisconnectDeadline).is_ok()
        });

        tracing::info!("Starting UI event loop");

        event_loop.run(move |event, _, control_flow| {
            match event {
                Event::UserEvent(UserEvent::Price(price_update)) => {
                    last_price_update = Instant::now();
                    connection_status = "Connected";
                    deadline_arm.rearm(last_price_update + disconnect_after);

                    apply_price_update(&mut latest_prices, &mut price_generation, &price_update);
                    let title = format_combined_title(&configured_pairs, &latest_prices);
                    if let Some(ref mut tray) = tray_icon {
                        tray.set_title(Some(&title));
                    }

                    tracing::debug!("Updated tray with: {}", title);
                }
                Event::UserEvent(UserEvent::PriceChannelClosed) => {
                    tracing::error!("Price update channel disconnected, shutting down UI");
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                Event::UserEvent(UserEvent::Menu(menu_event)) => {
                    tracing::debug!("Menu event received: {:?}", menu_event.id);
                    if menu_event.id == quit_item.id() {
                        tracing::info!("Quit requested by user");
                        tray_icon.take();
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                }
                Event::UserEvent(UserEvent::Tray(tray_event)) => {
                    tracing::debug!("Tray event received: {:?}", tray_event);
                }
                Event::UserEvent(UserEvent::DisconnectDeadline) => {
                    // The sleeper only posts this once the deadline is due. The
                    // one-shot title stays in MainEventsCleared, which runs after
                    // every user event already queued for this wake.
                }
                Event::MainEventsCleared => {
                    // User events in this wake are delivered first, so a price that
                    // arrived with the deadline has already refreshed last_price_update.
                    // `>=` matches the instant idle_control_flow switches to Wait.
                    if last_price_update.elapsed() >= disconnect_after
                        && connection_status != "Disconnected"
                    {
                        connection_status = "Disconnected";
                        if let Some(ref mut tray) = tray_icon {
                            tray.set_title(Some("Disconnected"));
                        }
                        tracing::warn!("No price updates received for {:?}", disconnect_after);
                    }
                }
                _ => {}
            }

            *control_flow = idle_control_flow(last_price_update, disconnect_after, Instant::now());
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

    #[test]
    fn disconnect_silence_timeout_scales_with_update_interval() {
        assert_eq!(disconnect_silence_timeout(1), Duration::from_secs(30));
        assert_eq!(disconnect_silence_timeout(30), Duration::from_secs(60));
        assert_eq!(disconnect_silence_timeout(60), Duration::from_secs(120));
    }

    #[test]
    fn idle_control_flow_waits_until_a_future_disconnect_deadline() {
        let last_price_update = Instant::now();
        let disconnect_after = Duration::from_secs(30);
        let now = last_price_update + Duration::from_secs(5);

        assert_eq!(
            idle_control_flow(last_price_update, disconnect_after, now),
            ControlFlow::WaitUntil(last_price_update + disconnect_after)
        );
    }

    #[test]
    fn idle_control_flow_waits_when_deadline_is_due_or_past() {
        let last_price_update = Instant::now();
        let disconnect_after = Duration::from_secs(30);
        let deadline = last_price_update + disconnect_after;

        assert_eq!(
            idle_control_flow(last_price_update, disconnect_after, deadline),
            ControlFlow::Wait
        );
        assert_eq!(
            idle_control_flow(
                last_price_update,
                disconnect_after,
                deadline + Duration::from_nanos(1)
            ),
            ControlFlow::Wait
        );
    }

    #[test]
    fn silence_timeout_does_not_require_cache_clear_for_same_generation() {
        // Documented contract: after a long throttle gap, the next same-generation
        // update must still see previously cached quiet pairs.
        let mut prices = HashMap::new();
        let mut generation = 0;
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "100", 1));
        apply_price_update(&mut prices, &mut generation, &update("ETH-USDT", "200", 1));
        // Simulate UI silence handling: status flip only — no cache clear.
        let _ = disconnect_silence_timeout(60);
        apply_price_update(&mut prices, &mut generation, &update("BTC-USDT", "101", 1));

        assert_eq!(
            format_combined_title(&["BTC-USDT".into(), "ETH-USDT".into()], &prices),
            "BTC-USDT: $101.00 | ETH-USDT: $200.00"
        );
    }

    #[test]
    fn disconnect_sleeper_fires_once_per_deadline_and_rearms() {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let arm = DisconnectDeadlineArm::new(started + Duration::from_secs(5));
        spawn_disconnect_sleeper(std::sync::Arc::clone(&arm), move || tx.send(()).is_ok());

        arm.rearm(Instant::now() + Duration::from_millis(40));
        rx.recv_timeout(Duration::from_secs(2))
            .expect("rearmed deadline wakes the sleeper");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "sleeper kept the replaced 5s deadline"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(80)).is_err(),
            "one deadline must not wake the loop twice"
        );

        arm.rearm(Instant::now() + Duration::from_millis(40));
        rx.recv_timeout(Duration::from_secs(2))
            .expect("a new price time arms another wake");
    }
}
