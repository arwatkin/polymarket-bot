mod config;
mod gamma_api;
mod state;
mod trading;
mod ui;
mod websocket;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use config::Config;
use gamma_api::GammaApiClient;
use state::AppState;
use trading::TradingMonitor;
use websocket::WebSocketManager;

#[tokio::main]
async fn main() -> Result<()> {
    // Load config
    let config = Config::load("Config.toml").unwrap_or_else(|e| {
        tracing::warn!("Failed to load Config.toml: {}. Using defaults.", e);
        Config::default()
    });

    // Initialize state
    let state = Arc::new(AppState::new());
    state.add_status("[EVENT] Starting Polymarket Trader...".to_string());

    // Create gamma API client
    let gamma_client = Arc::new(GammaApiClient::new(config.api.gamma_base_url.clone()));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create shutdown channel
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Spawn market refresh task
    let refresh_state = state.clone();
    let refresh_gamma = gamma_client.clone();
    let refresh_config = config.clone();
    let refresh_shutdown = shutdown_tx.clone();

    tokio::spawn(async move {
        if let Err(e) = run_market_refresh(refresh_state, refresh_gamma, refresh_config).await {
            tracing::error!("Market refresh error: {}", e);
        }
        let _ = refresh_shutdown.send(()).await;
    });

    // Spawn WebSocket task
    let ws_state = state.clone();
    let ws_config = config.clone();

    tokio::spawn(async move {
        loop {
            let asset_ids = ws_state.get_all_token_ids();
            if asset_ids.is_empty() {
                ws_state.add_status("[EVENT] Waiting for market data...".to_string());
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            ws_state.add_status(format!(
                "[EVENT] Connecting to WebSocket with {} tokens...",
                asset_ids.len()
            ));
            ws_state.set_ws_connected(true);

            let ws_manager =
                WebSocketManager::new(ws_config.api.websocket_url.clone(), ws_state.clone());

            if let Err(e) = ws_manager.connect_and_subscribe(asset_ids).await {
                ws_state.add_status(format!("WebSocket error: {}", e));
                ws_state.set_ws_connected(false);
            }

            ws_state.set_ws_connected(false);

            // Check if we have valid tokens before trying to reconnect
            let token_count = ws_state.get_all_token_ids().len();
            if token_count == 0 {
                ws_state.add_status("[EVENT] No tokens to subscribe to, waiting...".to_string());
                tokio::time::sleep(Duration::from_secs(2)).await;
            } else {
                ws_state.add_status(
                    "[EVENT] WebSocket disconnected, reconnecting in 3s...".to_string(),
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    });

    // Spawn trading monitor task if trading is configured
    if let Some(trading_config) = config.trading.clone() {
        let trading_state = state.clone();

        tokio::spawn(async move {
            let mut monitor = TradingMonitor::new(trading_config.clone());
            let mut authenticated = false;

            loop {
                // Wait a bit for market data to be available
                tokio::time::sleep(Duration::from_millis(500)).await;

                let next_interval = *trading_state.next_interval.read();
                if next_interval == 0 {
                    continue;
                }

                // Ensure we are authenticated AFTER ALL markets are loaded
                if !authenticated {
                    let market_count = trading_state.tracked_markets.read().len();
                    if market_count >= config.markets.len() && market_count > 0 {
                        if let Err(e) = monitor.initialize(&trading_state).await {
                            trading_state.add_status(format!("Trading init error: {}", e));
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        authenticated = true;

                        if trading_config.enabled {
                            trading_state.add_status("[EVENT] Live Trading ENABLED".to_string());
                            *trading_state.trading_enabled.write() = true;
                        } else {
                            trading_state
                                .add_status("[EVENT] Trading in SIMULATION mode".to_string());
                        }
                    } else {
                        continue;
                    }
                }

                // Get all tracked markets
                let market_slugs: Vec<String> = {
                    let markets = trading_state.tracked_markets.read();
                    markets.keys().cloned().collect()
                };

                // Monitor each market
                for market_slug in market_slugs {
                    monitor
                        .monitor_market(&trading_state, &market_slug, next_interval)
                        .await;
                }

                // Small delay to avoid tight loop
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    } else {
        state.add_status("Trading not configured".to_string());
    }

    // Main UI loop - no throttling, instant updates
    loop {
        // Draw UI
        terminal.draw(|f| ui::draw(f, &state))?;

        // Check for events with no timeout for instant updates
        if event::poll(Duration::from_millis(0))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => {
                            state.add_status("Manual refresh requested...".to_string());
                        }
                        _ => {}
                    }
                }
            }
        }

        // Check for shutdown signal
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
    }

    // Cleanup
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

async fn run_market_refresh(
    state: Arc<AppState>,
    gamma_client: Arc<GammaApiClient>,
    config: Config,
) -> Result<()> {
    let mut is_first_run = true;
    loop {
        // 1. Disconnect WebSocket first on rollover (but not on first run)
        if !is_first_run {
            state.set_ws_connected(false);
            state.request_ws_reconnect();

            // Evaluate trades for the interval that just ended
            // We do this BEFORE clearing markets and resetting tag states
            let next_ts = *state.next_interval.read();
            if next_ts > 0 {
                TradingMonitor::evaluate_trades_at_interval_end(&state, next_ts).await;
            }
        }

        // 2. Get current 5-minute interval timestamp
        let current_ts = GammaApiClient::get_current_5min_timestamp();
        let next_ts = GammaApiClient::get_next_5min_timestamp();

        state.set_next_interval(next_ts);

        // 3. Fetch markets
        state.add_status("[EVENT] Waiting for new markets to be available...".to_string());
        state.add_status(format!(
            "[EVENT] Fetching markets for interval ending at {}",
            current_ts
        ));

        // Reset tag states for new interval
        state.reset_tag_states();

        // Fetch each configured market
        for market_config in &config.markets {
            let event_slug = GammaApiClient::build_event_slug(&market_config.slug, current_ts);

            match gamma_client.fetch_event(&event_slug).await {
                Ok(event) => match GammaApiClient::parse_market_info(&event) {
                    Ok(info) => {
                        state.add_status(format!(
                            "[EVENT] Loaded: {}",
                            ui::extract_asset_name(&info.ticker)
                        ));
                        state.add_market(market_config.slug.clone(), info);
                    }
                    Err(e) => {
                        state.add_status(format!("[EVENT] Parse error for {}: {}", event_slug, e));
                    }
                },
                Err(e) => {
                    state.add_status(format!("[EVENT] Fetch error for {}: {}", event_slug, e));
                }
            }
        }

        // Calculate time until next interval
        let time_until_next = GammaApiClient::time_until_next_interval();
        let wait_duration = time_until_next.to_std().unwrap_or(Duration::from_secs(60));

        state.add_status(format!(
            "[EVENT] Next refresh in {}:{:02}",
            wait_duration.as_secs() / 60,
            wait_duration.as_secs() % 60
        ));

        // Wait until next interval
        tokio::time::sleep(wait_duration).await;

        // Wait additional time for Polymarket to create the new market
        // This ensures markets are available before we reconnect the WebSocket
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Signal websocket to reconnect with new tokens
        // This happens AFTER the wait to ensure markets exist on Polymarket's side
        state.request_ws_reconnect();
        is_first_run = false;
    }
}
