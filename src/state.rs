use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;

use crate::gamma_api::ParsedMarketInfo;

/// Tracks trades per asset within the current 5-minute window
#[derive(Debug, Clone)]
pub struct AssetTradeWindow {
    pub trade_count: u32,
    pub window_start: i64,
}

impl AssetTradeWindow {
    pub fn new(window_start: i64) -> Self {
        Self {
            trade_count: 0,
            window_start,
        }
    }
}

/// Maximum number of price history entries to keep
const MAX_PRICE_HISTORY: usize = 1000;

/// Maximum number of trades to keep (0 = unlimited)
/// Trades are evaluated at each interval end, so memory usage is bounded by
/// how long evaluation takes plus any pending trades.
const MAX_TRADES: usize = 0; // 0 = no limit

#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub market_slug: String,
    pub is_up: bool,
    pub best_ask: Option<f64>,
    pub best_bid: Option<f64>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MarketPrices {
    pub up_best_ask: Option<f64>,
    pub up_best_bid: Option<f64>,
    pub down_best_ask: Option<f64>,
    pub down_best_bid: Option<f64>,
    pub last_update: DateTime<Utc>,
}

impl Default for MarketPrices {
    fn default() -> Self {
        Self {
            up_best_ask: None,
            up_best_bid: None,
            down_best_ask: None,
            down_best_bid: None,
            last_update: Utc::now(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PriceHistoryEntry {
    pub up_best_ask: Option<f64>,
    pub down_best_ask: Option<f64>,
    pub timestamp: DateTime<Utc>,
}
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TradeRecord {
    pub timestamp: DateTime<Utc>,
    pub market_slug: String,
    pub asset: String,
    pub side: String, // "UP" or "DOWN"
    pub amount: f64,
    pub price: f64,
    pub status: TradeStatus,
    pub order_id: Option<String>,
    pub interval_ts: i64,
    pub result: Option<String>, // "WIN", "LOSS", or None
}

#[derive(Debug, Clone, PartialEq)]
pub enum TradeStatus {
    Pending,
    Success,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TradingStats {
    pub trade_count: u32,
    pub win_count: u32,
    pub loss_count: u32,
    pub strike_rate: f64,
    pub pnl: f64,
}

impl Default for TradingStats {
    fn default() -> Self {
        Self {
            trade_count: 0,
            win_count: 0,
            loss_count: 0,
            strike_rate: 0.0,
            pnl: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TagState {
    pub tagged: bool,
    pub tagged_at: Option<DateTime<Utc>>,
}

impl Default for TagState {
    fn default() -> Self {
        Self {
            tagged: false,
            tagged_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketTagState {
    pub up: TagState,
    pub down: TagState,
}

impl Default for MarketTagState {
    fn default() -> Self {
        Self {
            up: TagState::default(),
            down: TagState::default(),
        }
    }
}

pub struct AppState {
    /// Currently tracked markets with their info from gamma API
    pub tracked_markets: RwLock<HashMap<String, ParsedMarketInfo>>,

    /// Current prices for each market
    pub current_prices: RwLock<HashMap<String, MarketPrices>>,

    /// Price history for future trading decisions
    pub price_history: RwLock<HashMap<String, VecDeque<PriceHistoryEntry>>>,

    /// Recent trades
    pub trades: RwLock<VecDeque<TradeRecord>>,

    /// Status messages for the UI
    pub status_messages: RwLock<VecDeque<String>>,

    /// Connection status
    pub ws_connected: RwLock<bool>,

    /// Next interval timestamp
    pub next_interval: RwLock<i64>,

    /// Signal to reconnect websocket
    pub ws_reconnect_needed: RwLock<bool>,

    /// Tag states for each market (tracking which sides have been tagged)
    pub tag_states: RwLock<HashMap<String, MarketTagState>>,

    /// Trading enabled flag
    pub trading_enabled: RwLock<bool>,

    /// Trade count tracking per asset for rate limiting
    pub asset_trade_windows: RwLock<HashMap<String, AssetTradeWindow>>,
}

impl AppState {
    pub fn new() -> Self {
        // Truncate status.log on startup
        let _ = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open("status.log");

        Self {
            tracked_markets: RwLock::new(HashMap::new()),
            current_prices: RwLock::new(HashMap::new()),
            price_history: RwLock::new(HashMap::new()),
            trades: RwLock::new(VecDeque::new()),
            status_messages: RwLock::new(VecDeque::new()),
            ws_connected: RwLock::new(false),
            next_interval: RwLock::new(0),
            ws_reconnect_needed: RwLock::new(false),
            tag_states: RwLock::new(HashMap::new()),
            trading_enabled: RwLock::new(false),
            asset_trade_windows: RwLock::new(HashMap::new()),
        }
    }

    pub fn update_price(&self, update: PriceUpdate) {
        let mut prices = self.current_prices.write();
        let entry = prices.entry(update.market_slug.clone()).or_default();

        if update.is_up {
            if update.best_ask.is_some() {
                entry.up_best_ask = update.best_ask;
            }
            if update.best_bid.is_some() {
                entry.up_best_bid = update.best_bid;
            }
        } else {
            if update.best_ask.is_some() {
                entry.down_best_ask = update.best_ask;
            }
            if update.best_bid.is_some() {
                entry.down_best_bid = update.best_bid;
            }
        }
        entry.last_update = update.timestamp;

        // Record to history
        let history_entry = PriceHistoryEntry {
            up_best_ask: entry.up_best_ask,
            down_best_ask: entry.down_best_ask,
            timestamp: update.timestamp,
        };

        drop(prices);

        let mut history = self.price_history.write();
        let market_history = history
            .entry(update.market_slug)
            .or_insert_with(VecDeque::new);
        market_history.push_back(history_entry);

        // Trim history if too large
        while market_history.len() > MAX_PRICE_HISTORY {
            market_history.pop_front();
        }
    }

    pub fn add_trade(&self, trade: TradeRecord) {
        let mut trades = self.trades.write();
        trades.push_front(trade);

        // Only trim if MAX_TRADES is set (non-zero)
        if MAX_TRADES > 0 {
            // Only pop trades that have already been evaluated (have a result).
            // This prevents losing unevaluated trades when we hit the MAX_TRADES limit.
            while trades.len() > MAX_TRADES {
                // Find the oldest evaluated trade from the back
                if let Some(idx) = trades.iter().rposition(|t| t.result.is_some()) {
                    trades.remove(idx);
                } else {
                    // No evaluated trades to remove - break to avoid unbounded growth
                    // This shouldn't happen in normal operation since trades are
                    // evaluated before new intervals start
                    break;
                }
            }
        }
    }

    pub fn update_trade_status(&self, order_id: &str, status: TradeStatus) {
        let mut trades = self.trades.write();
        if let Some(trade) = trades
            .iter_mut()
            .find(|t| t.order_id.as_ref().map(|id| id.as_str()) == Some(order_id))
        {
            trade.status = status;
        }
    }

    pub fn set_latest_trade_failed(&self, market_slug: &str, asset: &str, side: &str) {
        let mut trades = self.trades.write();
        if let Some(trade) = trades.iter_mut().find(|t| {
            t.market_slug == market_slug
                && t.asset == asset
                && t.side == side
                && t.status == TradeStatus::Pending
        }) {
            trade.status = TradeStatus::Failed;
        }
    }

    /// Set order_id on the most recent pending trade matching the given criteria
    pub fn set_trade_order_id(&self, market_slug: &str, asset: &str, side: &str, order_id: String) {
        let mut trades = self.trades.write();
        if let Some(trade) = trades.iter_mut().find(|t| {
            t.market_slug == market_slug
                && t.asset == asset
                && t.side == side
                && t.status == TradeStatus::Pending
                && t.order_id.is_none()
        }) {
            trade.order_id = Some(order_id);
        }
    }

    /// Update price on the most recent pending trade matching the given criteria (for retries)
    pub fn update_trade_price(&self, market_slug: &str, asset: &str, side: &str, new_price: f64) {
        let mut trades = self.trades.write();
        if let Some(trade) = trades.iter_mut().find(|t| {
            t.market_slug == market_slug
                && t.asset == asset
                && t.side == side
                && t.status == TradeStatus::Pending
        }) {
            trade.price = new_price;
        }
    }

    /// Set result on trades matching the given criteria
    pub fn set_trade_result(&self, market_slug: &str, asset: &str, side: &str, result: String) {
        let mut trades = self.trades.write();
        // Find the most recent successful trade with no result set yet
        if let Some(trade) = trades.iter_mut().rev().find(|t| {
            t.market_slug == market_slug
                && t.asset == asset
                && t.side == side
                && t.status == TradeStatus::Success
                && t.result.is_none()
        }) {
            trade.result = Some(result);
        }
    }

    pub fn get_trading_stats(&self) -> TradingStats {
        let trades = self.trades.read();

        let successful_trades: Vec<_> = trades
            .iter()
            .filter(|t| t.status == TradeStatus::Success)
            .collect();

        let trade_count = successful_trades.len() as u32;

        let mut win_count = 0;
        let mut loss_count = 0;
        let mut pnl = 0.0;

        for trade in &successful_trades {
            if let Some(ref result) = trade.result {
                if result == "WIN" {
                    win_count += 1;
                    // P&L calculation for winning trades: profit = bet_amount * ((0.99 / entry_price) - 1)
                    pnl += trade.amount * ((0.99 / trade.price) - 1.0);
                } else if result == "LOSS" {
                    loss_count += 1;
                    // P&L calculation for losing trades: loss = -bet_amount
                    pnl -= trade.amount;
                }
            }
        }

        let strike_rate = if win_count + loss_count > 0 {
            (win_count as f64 / (win_count + loss_count) as f64) * 100.0
        } else {
            0.0
        };

        TradingStats {
            trade_count,
            win_count,
            loss_count,
            strike_rate,
            pnl,
        }
    }

    pub fn add_status(&self, message: String) {
        let timestamped = format!("[{}] {}", Utc::now().format("%H:%M:%S"), message);

        // Log to file
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("status.log")
            .and_then(|mut file| {
                let clean_msg = timestamped.trim_end();
                writeln!(file, "{}", clean_msg)
            });

        let mut status = self.status_messages.write();
        status.push_front(timestamped);

        while status.len() > 50 {
            status.pop_back();
        }
    }

    pub fn set_ws_connected(&self, connected: bool) {
        *self.ws_connected.write() = connected;
    }

    pub fn set_next_interval(&self, timestamp: i64) {
        *self.next_interval.write() = timestamp;
    }

    pub fn request_ws_reconnect(&self) {
        *self.ws_reconnect_needed.write() = true;
    }

    pub fn reset_tag_states(&self) {
        let mut tags = self.tag_states.write();
        tags.clear();
    }

    pub fn should_reconnect(&self) -> bool {
        let mut flag = self.ws_reconnect_needed.write();
        let value = *flag;
        *flag = false;
        value
    }

    pub fn add_market(&self, slug: String, info: ParsedMarketInfo) {
        let mut markets = self.tracked_markets.write();
        let token_ids_changed = if let Some(existing) = markets.get(&slug) {
            existing.up_token_id != info.up_token_id || existing.down_token_id != info.down_token_id
        } else {
            true // New market
        };
        markets.insert(slug.clone(), info);
        drop(markets);

        // Reset prices only if token IDs changed (new 5-min interval) or this is a new market
        // This ensures we don't show stale data from previous interval's tokens
        // but avoids flickering during normal metadata updates
        if token_ids_changed {
            self.current_prices
                .write()
                .insert(slug, MarketPrices::default());
        }
    }

    pub fn get_all_token_ids(&self) -> Vec<String> {
        let tracked = self.tracked_markets.read();
        let mut ids = Vec::new();
        for info in tracked.values() {
            ids.push(info.up_token_id.clone());
            ids.push(info.down_token_id.clone());
        }
        ids
    }

    /// Check if an asset can trade (returns true if under the limit)
    pub fn can_trade_asset(&self, asset: &str, max_trades: u32, next_interval_ts: i64) -> bool {
        let mut windows = self.asset_trade_windows.write();

        // Get or create the window for this asset
        let window = windows
            .entry(asset.to_string())
            .or_insert_with(|| AssetTradeWindow::new(next_interval_ts));

        // If we're in a new window, reset the count
        if window.window_start != next_interval_ts {
            window.window_start = next_interval_ts;
            window.trade_count = 0;
        }

        // Check if we're under the limit
        window.trade_count < max_trades
    }

    /// Atomically check if an asset can trade AND increment if it can.
    /// Returns true if the trade is allowed (was under limit and has been incremented).
    /// Returns false if the trade limit was reached.
    /// This prevents race conditions where multiple trades could pass the check
    /// before any of them increment the counter.
    pub fn try_trade_asset(&self, asset: &str, max_trades: u32, next_interval_ts: i64) -> bool {
        let mut windows = self.asset_trade_windows.write();

        // Get or create the window for this asset
        let window = windows
            .entry(asset.to_string())
            .or_insert_with(|| AssetTradeWindow::new(next_interval_ts));

        // If we're in a new window, reset the count
        if window.window_start != next_interval_ts {
            window.window_start = next_interval_ts;
            window.trade_count = 0;
        }

        // Check if we're under the limit and increment atomically
        if window.trade_count < max_trades {
            window.trade_count += 1;
            true
        } else {
            false
        }
    }

    /// Increment the trade count for an asset
    pub fn increment_asset_trades(&self, asset: &str, next_interval_ts: i64) {
        let mut windows = self.asset_trade_windows.write();

        let window = windows
            .entry(asset.to_string())
            .or_insert_with(|| AssetTradeWindow::new(next_interval_ts));

        // Ensure we're in the correct window
        if window.window_start != next_interval_ts {
            window.window_start = next_interval_ts;
            window.trade_count = 0;
        }

        window.trade_count += 1;
    }

    /// Get current trade count for an asset
    pub fn get_asset_trade_count(&self, asset: &str, next_interval_ts: i64) -> u32 {
        let windows = self.asset_trade_windows.read();

        if let Some(window) = windows.get(asset) {
            if window.window_start == next_interval_ts {
                return window.trade_count;
            }
        }

        0
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
