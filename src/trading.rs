use anyhow::{Result, anyhow};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client as SdkClient, Config as SdkConfig};

use crate::config::TradingConfig;
use crate::state::{AppState, MarketTagState, TradeRecord, TradeStatus};

/// Simplified CLOB client for executing trades using the official SDK
pub struct ClobClient {
    sdk_client: Option<SdkClient<Authenticated<Normal>>>,
    private_key: String,
    proxy_address: String,
    signature_type: u8,
}

#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub success: bool,
    pub error_msg: Option<String>,
    pub order_id: Option<String>,
}

impl ClobClient {
    pub fn new(config: &TradingConfig) -> Self {
        Self {
            sdk_client: None,
            private_key: config.private_key.clone(),
            proxy_address: config.proxy_address.clone(),
            signature_type: config.signature_type,
        }
    }

    /// Initialize API credentials using the SDK
    pub async fn initialize(&mut self) -> Result<()> {
        let signer = LocalSigner::from_str(&self.private_key)
            .map_err(|e| anyhow!("Invalid private key: {}", e))?
            .with_chain_id(Some(POLYGON));

        let sdk_config = SdkConfig::builder().use_server_time(true).build();

        let mut auth_builder = SdkClient::new("https://clob.polymarket.com", sdk_config)?
            .authentication_builder(&signer);

        // Set signature type based on config
        let sig_type = match self.signature_type {
            1 => SignatureType::Proxy,
            2 => SignatureType::GnosisSafe,
            _ => SignatureType::Eoa,
        };
        auth_builder = auth_builder.signature_type(sig_type);

        // If proxy address is provided, use it as funder
        if !self.proxy_address.is_empty() {
            let funder = self
                .proxy_address
                .parse()
                .map_err(|e| anyhow!("Invalid proxy address: {}", e))?;
            auth_builder = auth_builder.funder(funder);
        } else {
            // If no proxy address is provided, but we are using Proxy or GnosisSafe,
            // the SDK will auto-derive the funder address.
            // For EOA, funder is the signer address.
        }

        let client = auth_builder
            .authenticate()
            .await
            .map_err(|e| anyhow!("Authentication failed: {}", e))?;

        self.sdk_client = Some(client);
        Ok(())
    }

    /// Get the authenticated address
    pub fn address(&self) -> Option<String> {
        self.sdk_client.as_ref().map(|c| c.address().to_string())
    }

    /// Place a market buy order using the SDK
    pub async fn place_market_order(
        &self,
        state: &Arc<AppState>,
        token_id: &str,
        amount: f64,
        side: &str,
    ) -> Result<OrderResponse> {
        let client = self
            .sdk_client
            .as_ref()
            .ok_or_else(|| anyhow!("SDK client not initialized"))?;

        let amount_dec = Decimal::from_f64(amount).ok_or_else(|| anyhow!("Invalid amount"))?;

        let side_enum = if side == "BUY" { Side::Buy } else { Side::Sell };

        // Build the market order
        let order_builder = client
            .market_order()
            .token_id(token_id)
            .amount(Amount::usdc(amount_dec)?)
            .side(side_enum)
            .order_type(OrderType::FAK);

        let signable_order = order_builder
            .build()
            .await
            .map_err(|e| anyhow!("Failed to build market order: {}", e))?;

        // Log deep debug info for signature
        state.add_status(format!(
            "[TRADE] Signing Order: Maker={}, Signer={}, Token={}, MakerAmt={}, TakerAmt={}",
            signable_order.order.maker,
            signable_order.order.signer,
            signable_order.order.tokenId,
            signable_order.order.makerAmount,
            signable_order.order.takerAmount
        ));

        // Sign the order
        let signer = LocalSigner::from_str(&self.private_key)
            .map_err(|e| anyhow!("Invalid private key: {}", e))?
            .with_chain_id(Some(POLYGON));

        // Use the SDK's sign method which correctly handles the signature type
        let signed_order = client
            .sign(&signer, signable_order)
            .await
            .map_err(|e| anyhow!("Failed to sign order: {}", e))?;

        state.add_status(format!(
            "[TRADE] Signed Order: Owner={}, Sig={}",
            signed_order.owner, signed_order.signature
        ));

        // Post the order
        let response = client.post_order(signed_order).await.map_err(|e| {
            let err_str = e.to_string().replace('\n', " ");
            anyhow!("Failed to post order: {}", err_str)
        })?;

        Ok(OrderResponse {
            success: response.success,
            error_msg: response.error_msg,
            order_id: Some(response.order_id),
        })
    }
}

/// Trading strategy monitor - handles the 5-minute window logic
pub struct TradingMonitor {
    config: TradingConfig,
    clob_client: Option<ClobClient>,
}

impl TradingMonitor {
    pub fn new(config: TradingConfig) -> Self {
        let clob_client = if config.enabled && !config.private_key.is_empty() {
            Some(ClobClient::new(&config))
        } else {
            None
        };

        Self {
            config,
            clob_client,
        }
    }

    /// Initialize the CLOB client if trading is enabled
    pub async fn initialize(&mut self, state: &Arc<AppState>) -> Result<()> {
        if let Some(ref mut client) = self.clob_client {
            match client.initialize().await {
                Ok(_) => {
                    if let Some(addr) = client.address() {
                        state.add_status(format!(
                            "[AUTH] Successfully authenticated with Polymarket CLOB. Address: {}",
                            addr
                        ));
                    } else {
                        state.add_status(
                            "[AUTH] Successfully authenticated with Polymarket CLOB.".to_string(),
                        );
                    }
                }
                Err(e) => {
                    state.add_status(format!("[AUTH] Authentication failed: {}", e));
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Monitor prices and execute strategy logic for a single market
    pub async fn monitor_market(
        &mut self,
        state: &Arc<AppState>,
        market_slug: &str,
        next_interval_ts: i64,
    ) {
        let now = chrono::Utc::now().timestamp();
        let time_until_interval = next_interval_ts - now;

        let in_monitoring_window = time_until_interval > 0
            && time_until_interval <= self.config.monitoring_window_seconds as i64;

        // Only execute if we're in the monitoring window
        if !in_monitoring_window {
            return;
        }

        // Extract asset name for rate limit checking
        let asset = self.extract_asset_name(market_slug);

        // Get current prices (clone and drop lock immediately)
        let (up_price, down_price, last_update) = {
            let prices = state.current_prices.read();
            let market_prices = match prices.get(market_slug) {
                Some(p) => p.clone(),
                None => return,
            };
            let up = market_prices.up_best_ask.unwrap_or(0.0);
            let down = market_prices.down_best_ask.unwrap_or(0.0);
            (up, down, market_prices.last_update)
        }; // Lock is dropped here

        // Ensure we have a fresh price update for this interval before tagging
        // This prevents using stale prices from a previous interval or before WS connects
        let price_is_fresh = last_update.timestamp() >= (next_interval_ts - 300);

        // Get tag states and check/update them
        let (
            up_should_tag,
            up_should_execute,
            down_should_tag,
            down_should_execute,
            up_token_id,
            down_token_id,
        ) = {
            let mut tag_states = state.tag_states.write();
            let market_tag = tag_states
                .entry(market_slug.to_string())
                .or_insert_with(MarketTagState::default);

            // Check if we can still trade this asset before allowing tags
            let can_trade_asset =
                state.can_trade_asset(&asset, self.config.max_asset_trades, next_interval_ts);

            let up_should_tag = !market_tag.up.tagged
                && price_is_fresh
                && up_price < self.config.tag_threshold
                && up_price > 0.0
                && can_trade_asset;
            let up_should_execute =
                market_tag.up.tagged && up_price >= self.config.execute_threshold;
            let down_should_tag = !market_tag.down.tagged
                && price_is_fresh
                && down_price < self.config.tag_threshold
                && down_price > 0.0
                && can_trade_asset;
            let down_should_execute =
                market_tag.down.tagged && down_price >= self.config.execute_threshold;

            // Update tag states
            if up_should_tag {
                market_tag.up.tagged = true;
                market_tag.up.tagged_at = Some(chrono::Utc::now());
            }
            if up_should_execute {
                market_tag.up.tagged = false;
                market_tag.up.tagged_at = None;
            }
            if down_should_tag {
                market_tag.down.tagged = true;
                market_tag.down.tagged_at = Some(chrono::Utc::now());
            }
            if down_should_execute {
                market_tag.down.tagged = false;
                market_tag.down.tagged_at = None;
            }

            // Get token IDs while we have the lock
            let up_token = self.get_up_token_id(state, market_slug);
            let down_token = self.get_down_token_id(state, market_slug);

            (
                up_should_tag,
                up_should_execute,
                down_should_tag,
                down_should_execute,
                up_token,
                down_token,
            )
        }; // Lock is dropped here

        // Now handle actions (these can await safely)
        if up_should_tag {
            state.add_status(format!("[EVENT] {} UP tagged at {:.3}", asset, up_price));
        }

        if up_should_execute {
            state.add_status(format!(
                "[TRADE] {} UP execute signal! Price: {:.3}",
                asset, up_price
            ));

            if let Some(token_id) = up_token_id {
                self.execute_trade(state, market_slug, &token_id, "UP", up_price)
                    .await;
            }
        }

        if down_should_tag {
            state.add_status(format!(
                "[EVENT] {} DOWN tagged at {:.3}",
                asset, down_price
            ));
        }

        if down_should_execute {
            state.add_status(format!(
                "[TRADE] {} DOWN execute signal! Price: {:.3}",
                asset, down_price
            ));

            if let Some(token_id) = down_token_id {
                self.execute_trade(state, market_slug, &token_id, "DOWN", down_price)
                    .await;
            }
        }
    }

    /// Execute a trade via the CLOB API with retry logic
    async fn execute_trade(
        &self,
        state: &Arc<AppState>,
        market_slug: &str,
        token_id: &str,
        side: &str,
        price: f64,
    ) {
        self.execute_trade_with_retries(state, market_slug, token_id, side, price, 0)
            .await;
    }

    /// Internal method that handles trade execution with retry logic
    async fn execute_trade_with_retries(
        &self,
        state: &Arc<AppState>,
        market_slug: &str,
        token_id: &str,
        side: &str,
        original_price: f64,
        retry_count: u32,
    ) {
        let asset = self.extract_asset_name(market_slug);
        let next_interval_ts = *state.next_interval.read();

        // Get current market price (for retries, use fresh price not original)
        let current_price = if retry_count > 0 {
            let prices = state.current_prices.read();
            let market_prices = match prices.get(market_slug) {
                Some(p) => p.clone(),
                None => {
                    state.add_status(format!(
                        "[TRADE] Cannot retry {} {} - no current price available",
                        asset, side
                    ));
                    return;
                }
            };
            if side == "UP" {
                market_prices.up_best_ask.unwrap_or(original_price)
            } else {
                market_prices.down_best_ask.unwrap_or(original_price)
            }
        } else {
            original_price
        };

        // Check and increment rate limiting atomically (only on first attempt, not retries)
        // This prevents race conditions where multiple trades could pass the check
        // before any of them increment the counter
        if retry_count == 0 {
            if !state.try_trade_asset(&asset, self.config.max_asset_trades, next_interval_ts) {
                let current_count = state.get_asset_trade_count(&asset, next_interval_ts);
                state.add_status(format!(
                    "[TRADE] Entry limit reached for {} ({}/{} trades this window) - skipping {} trade",
                    asset, current_count, self.config.max_asset_trades, side
                ));
                return;
            }
        }

        // Check if trading is actually enabled
        if !self.config.enabled {
            let retry_msg = if retry_count > 0 {
                format!(" (Retry {}/{})", retry_count, self.config.max_retries)
            } else {
                String::new()
            };

            state.add_status(format!(
                "[TRADE] Simulated trade{}: {} {} @ {:.3} for ${:.2}",
                retry_msg, market_slug, side, current_price, self.config.bet_amount
            ));

            // Add simulated trade record
            let trade = TradeRecord {
                timestamp: chrono::Utc::now(),
                market_slug: market_slug.to_string(),
                asset: asset.clone(),
                side: side.to_string(),
                amount: self.config.bet_amount,
                price: current_price,
                status: TradeStatus::Success,
                order_id: Some(format!("SIM_{}", chrono::Utc::now().timestamp())),
                interval_ts: next_interval_ts,
                result: None,
            };
            state.add_trade(trade);

            // Increment asset trade count for simulated trades too
            if retry_count == 0 {
                state.increment_asset_trades(&asset, next_interval_ts);
            }
            return;
        }

        // Real trade execution
        if let Some(ref client) = self.clob_client {
            let retry_msg = if retry_count > 0 {
                format!(" (Retry {}/{})", retry_count, self.config.max_retries)
            } else {
                String::new()
            };

            state.add_status(format!(
                "[TRADE] Executing{}: {} {} @ {:.3} for ${:.2} (Token: {})",
                retry_msg, asset, side, current_price, self.config.bet_amount, token_id
            ));

            // Create pending trade record (only on first attempt)
            if retry_count == 0 {
                let trade = TradeRecord {
                    timestamp: chrono::Utc::now(),
                    market_slug: market_slug.to_string(),
                    asset: asset.clone(),
                    side: side.to_string(),
                    amount: self.config.bet_amount,
                    price: current_price,
                    status: TradeStatus::Pending,
                    order_id: None,
                    interval_ts: next_interval_ts,
                    result: None,
                };
                state.add_trade(trade);
            } else {
                // Update the price in the pending trade record for retries
                state.update_trade_price(market_slug, &asset, side, current_price);
            }

            // Execute the order
            match client
                .place_market_order(state, token_id, self.config.bet_amount, "BUY")
                .await
            {
                Ok(response) => {
                    if response.success {
                        state.add_status(format!(
                            "[TRADE] Success! Order ID: {}{}",
                            response.order_id.as_ref().unwrap_or(&"unknown".to_string()),
                            if retry_count > 0 {
                                format!(" (after {} retries)", retry_count)
                            } else {
                                String::new()
                            }
                        ));

                        // Asset trade count was already incremented atomically in try_trade_asset
                        // at the start of the trade, so no need to increment again here

                        if let Some(order_id) = response.order_id.clone() {
                            state.set_trade_order_id(market_slug, &asset, side, order_id.clone());
                            state.update_trade_status(&order_id, TradeStatus::Success);
                        }
                    } else {
                        let error = response
                            .error_msg
                            .unwrap_or_else(|| "Unknown error".to_string())
                            .replace('\n', " ");

                        // Check if we should retry
                        let should_retry = retry_count < self.config.max_retries;

                        if should_retry {
                            state.add_status(format!(
                                "[TRADE] Failed to fill: {} - Retrying at market price (Attempt {}/{})",
                                error, retry_count + 1, self.config.max_retries
                            ));

                            // Retry with new market price
                            Box::pin(self.execute_trade_with_retries(
                                state,
                                market_slug,
                                token_id,
                                side,
                                original_price,
                                retry_count + 1,
                            ))
                            .await;
                        } else {
                            state.add_status(format!(
                                "[TRADE] Max retries reached ({}) - Skipping {} {} trade. Last error: {}",
                                self.config.max_retries, asset, side, error
                            ));

                            // Mark as failed (no need to decrement since we never incremented)
                            state.set_latest_trade_failed(market_slug, &asset, side);
                        }
                    }
                }
                Err(e) => {
                    // Check if we should retry on errors too
                    let should_retry = retry_count < self.config.max_retries;

                    if should_retry {
                        state.add_status(format!(
                            "[TRADE] Error: {} - Retrying at market price (Attempt {}/{})",
                            e,
                            retry_count + 1,
                            self.config.max_retries
                        ));

                        // Retry with new market price
                        Box::pin(self.execute_trade_with_retries(
                            state,
                            market_slug,
                            token_id,
                            side,
                            original_price,
                            retry_count + 1,
                        ))
                        .await;
                    } else {
                        state.add_status(format!(
                            "[TRADE] Max retries reached ({}) - Skipping {} {} trade. Last error: {}",
                            self.config.max_retries, asset, side, e
                        ));

                        // Mark as failed (no need to decrement since we never incremented)
                        state.set_latest_trade_failed(market_slug, &asset, side);
                    }
                }
            }
        }
    }

    /// Get UP token ID for a market
    fn get_up_token_id(&self, state: &Arc<AppState>, market_slug: &str) -> Option<String> {
        let markets = state.tracked_markets.read();
        markets.get(market_slug).map(|m| m.up_token_id.clone())
    }

    /// Get DOWN token ID for a market
    fn get_down_token_id(&self, state: &Arc<AppState>, market_slug: &str) -> Option<String> {
        let markets = state.tracked_markets.read();
        markets.get(market_slug).map(|m| m.down_token_id.clone())
    }

    /// Evaluate all trades for the interval
    pub async fn evaluate_trades_at_interval_end(state: &Arc<AppState>, interval_ts: i64) {
        let mut evaluated_count = 0;

        // Get all trades for this interval that need evaluation
        let trades_to_evaluate: Vec<(String, String, String)> = {
            let trades = state.trades.read();
            trades
                .iter()
                .filter(|t| {
                    t.interval_ts == interval_ts
                        && t.status == TradeStatus::Success
                        && t.result.is_none()
                })
                .map(|t| (t.market_slug.clone(), t.asset.clone(), t.side.clone()))
                .collect()
        };

        for (market_slug, asset, side) in trades_to_evaluate {
            // Get final prices for this market
            let final_price = {
                let prices = state.current_prices.read();
                let market_prices = match prices.get(&market_slug) {
                    Some(p) => p.clone(),
                    None => continue,
                };
                if side == "UP" {
                    market_prices.up_best_ask.unwrap_or(0.0)
                } else {
                    market_prices.down_best_ask.unwrap_or(0.0)
                }
            };

            let result = if final_price > 0.95 { "WIN" } else { "LOSS" };

            // Update the trade result - this acquires its own write lock
            state.set_trade_result(&market_slug, &asset, &side, result.to_string());
            evaluated_count += 1;

            state.add_status(format!(
                "[EVAL] {} {} trade result: {} (final price: {:.3})",
                asset, side, result, final_price
            ));
        }

        if evaluated_count > 0 {
            state.add_status(format!(
                "[EVAL] Evaluated {} trades for interval ending at {}",
                evaluated_count, interval_ts
            ));
        }
    }

    /// Extract asset name from market slug
    fn extract_asset_name(&self, market_slug: &str) -> String {
        market_slug
            .split('-')
            .next()
            .unwrap_or("???")
            .to_uppercase()
    }
}
