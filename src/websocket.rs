use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::state::{AppState, PriceUpdate};

#[derive(Debug, Clone, Serialize)]
struct MarketSubscription {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    msg_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "event_type")]
pub enum WsMessage {
    #[serde(rename = "book")]
    Book(BookMessage),
    #[serde(rename = "price_change")]
    PriceChange(PriceChangeMessage),
    #[serde(rename = "last_trade_price")]
    LastTradePrice(()),
    #[serde(other)]
    Unknown,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct BookMessage {
    pub asset_id: String,
    pub market: String,
    pub bids: Vec<OrderLevel>,
    pub asks: Vec<OrderLevel>,
    pub timestamp: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OrderLevel {
    pub price: String,
    pub size: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChangeMessage {
    pub market: String,
    pub price_changes: Vec<PriceChange>,
    pub timestamp: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChange {
    pub asset_id: String,
    pub price: String,
    pub size: String,
    pub side: String,
    pub best_bid: String,
    pub best_ask: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct LastTradePriceMessage {
    pub asset_id: String,
    pub market: String,
    pub price: String,
    pub side: String,
    pub size: String,
    pub timestamp: String,
}

pub struct WebSocketManager {
    ws_url: String,
    state: Arc<AppState>,
}

impl WebSocketManager {
    pub fn new(ws_url: String, state: Arc<AppState>) -> Self {
        Self { ws_url, state }
    }

    pub async fn connect_and_subscribe(&self, asset_ids: Vec<String>) -> Result<()> {
        let url = format!("{}/ws/market", self.ws_url);

        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to WebSocket")?;

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to market channel
        let subscription = MarketSubscription {
            assets_ids: asset_ids.clone(),
            msg_type: "market".to_string(),
        };

        let sub_msg = serde_json::to_string(&subscription)?;
        write.send(Message::Text(sub_msg)).await?;

        // Create ping task
        let (ping_tx, mut ping_rx) = mpsc::channel::<()>(1);

        // Spawn ping task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                if ping_tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        let state = self.state.clone();

        // Create reconnect check interval
        let mut reconnect_check = tokio::time::interval(tokio::time::Duration::from_secs(1));
        reconnect_check.tick().await; // consume first tick

        loop {
            tokio::select! {
                Some(msg) = read.next() => {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if text == "PONG" {
                                continue;
                            }

                            // Build token mapping dynamically for each message to ensure we have latest markets
                            let token_mapping = self.build_token_mapping();

                            if let Err(e) = self.handle_message(&text, &token_mapping, &state).await {
                                self.state.add_status(format!("Error handling message: {}", e));
                            }
                        }
                        Ok(Message::Close(_)) => {
                            break;
                        }
                        Err(e) => {
                            self.state.add_status(format!("WebSocket error: {}", e));
                            break;
                        }
                        _ => {}
                    }
                }
                Some(_) = ping_rx.recv() => {
                    if let Err(e) = write.send(Message::Text("PING".to_string())).await {
                        self.state.add_status(format!("Ping failed: {}", e));
                        break;
                    }
                }
                _ = reconnect_check.tick() => {
                    // Check if we need to reconnect (new 15-minute interval with new markets)
                    if self.state.should_reconnect() {
                        self.state.add_status("[EVENT] Reconnecting for new interval...".to_string());
                        return Ok(());
                    }
                }
            }
        }

        Ok(())
    }

    /// Build token mapping from current tracked markets
    /// This is called for each message to ensure we always have the latest market data
    fn build_token_mapping(&self) -> HashMap<String, String> {
        let tracked = self.state.tracked_markets.read();
        let mut map = HashMap::new();
        for (slug, info) in tracked.iter() {
            map.insert(info.up_token_id.clone(), slug.clone());
            map.insert(info.down_token_id.clone(), slug.clone());
        }
        map
    }

    async fn handle_message(
        &self,
        text: &str,
        token_mapping: &HashMap<String, String>,
        state: &Arc<AppState>,
    ) -> Result<()> {
        // Try to parse as a known message type
        let msg: WsMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(_) => {
                tracing::trace!("Unknown message format: {}", text);
                return Ok(());
            }
        };

        match msg {
            WsMessage::Book(book) => {
                self.handle_book_message(&book, token_mapping, state)
                    .await?;
            }
            WsMessage::PriceChange(pc) => {
                self.handle_price_change(&pc, token_mapping, state).await?;
            }
            WsMessage::LastTradePrice(_) => {
                // Last trade price messages are informational only
                // Actual trades are managed by the trading module
            }
            WsMessage::Unknown => {}
        }

        Ok(())
    }

    async fn handle_book_message(
        &self,
        book: &BookMessage,
        token_mapping: &HashMap<String, String>,
        state: &Arc<AppState>,
    ) -> Result<()> {
        let market_slug = match token_mapping.get(&book.asset_id) {
            Some(s) => s,
            None => {
                // Token not in our mapping - might be old data
                return Ok(());
            }
        };

        // Get best ask (lowest ask price)
        let best_ask = book
            .asks
            .iter()
            .filter_map(|a| a.price.parse::<f64>().ok())
            .min_by(|a, b| a.partial_cmp(b).unwrap());

        // Get best bid (highest bid price)
        let best_bid = book
            .bids
            .iter()
            .filter_map(|b| b.price.parse::<f64>().ok())
            .max_by(|a, b| a.partial_cmp(b).unwrap());

        // Determine if this is Up or Down token
        let tracked = state.tracked_markets.read();
        if let Some(info) = tracked.get(market_slug) {
            let is_up = book.asset_id == info.up_token_id;
            drop(tracked);

            let update = PriceUpdate {
                market_slug: market_slug.clone(),
                is_up,
                best_ask,
                best_bid,
                timestamp: chrono::Utc::now(),
            };

            state.update_price(update.clone());
        }

        Ok(())
    }

    async fn handle_price_change(
        &self,
        pc: &PriceChangeMessage,
        token_mapping: &HashMap<String, String>,
        state: &Arc<AppState>,
    ) -> Result<()> {
        for change in &pc.price_changes {
            let market_slug = match token_mapping.get(&change.asset_id) {
                Some(s) => s,
                None => continue,
            };

            let best_ask = change.best_ask.parse::<f64>().ok();
            let best_bid = change.best_bid.parse::<f64>().ok();

            // Determine if this is Up or Down token
            let tracked = state.tracked_markets.read();
            if let Some(info) = tracked.get(market_slug) {
                let is_up = change.asset_id == info.up_token_id;
                drop(tracked);

                let update = PriceUpdate {
                    market_slug: market_slug.clone(),
                    is_up,
                    best_ask,
                    best_bid,
                    timestamp: chrono::Utc::now(),
                };

                state.update_price(update);
            }
        }

        Ok(())
    }
}
