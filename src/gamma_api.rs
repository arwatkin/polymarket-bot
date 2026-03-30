use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

/// Interval duration in seconds (5 minutes = 300 seconds)
const INTERVAL_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaEventResponse {
    pub id: String,
    pub ticker: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    #[serde(rename = "resolutionSource")]
    pub resolution_source: Option<String>,
    #[serde(rename = "startDate")]
    pub start_date: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub markets: Vec<GammaMarket>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaMarket {
    pub id: String,
    pub question: String,
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    pub slug: String,
    pub outcomes: String,
    #[serde(rename = "outcomePrices")]
    pub outcome_prices: String,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: String,
    #[serde(rename = "bestBid")]
    pub best_bid: Option<f64>,
    #[serde(rename = "bestAsk")]
    pub best_ask: Option<f64>,
    #[serde(rename = "lastTradePrice")]
    pub last_trade_price: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ParsedMarketInfo {
    pub ticker: String,
    pub title: String,
    pub condition_id: String,
    pub up_token_id: String,
    pub down_token_id: String,
    pub up_price: f64,
    pub down_price: f64,
    pub up_best_ask: Option<f64>,
    pub down_best_ask: Option<f64>,
}

pub struct GammaApiClient {
    client: reqwest::Client,
    base_url: String,
}

impl GammaApiClient {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }

    /// Calculate the next 5-minute interval timestamp
    pub fn get_next_5min_timestamp() -> i64 {
        let now = Utc::now();
        let minutes = now.minute();
        let seconds = now.second();

        // Calculate minutes until next 5-minute boundary
        let minutes_to_next = 5 - (minutes % 5);

        // If we're exactly on a boundary, get the next one
        let minutes_to_add = if minutes % 5 == 0 && seconds == 0 {
            5
        } else {
            minutes_to_next as i64
        };

        let next_interval = now + chrono::Duration::minutes(minutes_to_add)
            - chrono::Duration::seconds(seconds as i64)
            - chrono::Duration::nanoseconds(now.nanosecond() as i64);

        next_interval.timestamp()
    }

    /// Calculate the current 5-minute interval timestamp (the START of the interval we're currently in)
    pub fn get_current_5min_timestamp() -> i64 {
        let now = Utc::now();
        let ts = now.timestamp();

        // 300 seconds = 5 minutes
        // Round down to get the start of current interval
        (ts / INTERVAL_SECONDS) * INTERVAL_SECONDS
    }

    /// Build the full slug with timestamp
    pub fn build_event_slug(base_slug: &str, timestamp: i64) -> String {
        format!("{}-{}", base_slug, timestamp)
    }

    /// Fetch event data from gamma API
    pub async fn fetch_event(&self, event_slug: &str) -> Result<GammaEventResponse> {
        let url = format!("{}/events/slug/{}", self.base_url, event_slug);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send request to gamma API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gamma API error {}: {}", status, body);
        }

        let event: GammaEventResponse = response
            .json()
            .await
            .context("Failed to parse gamma API response")?;

        Ok(event)
    }

    /// Parse the event response into a more usable format
    pub fn parse_market_info(event: &GammaEventResponse) -> Result<ParsedMarketInfo> {
        let market = event.markets.first().context("No markets found in event")?;

        // Parse outcomes - should be ["Up", "Down"]
        let outcomes: Vec<String> =
            serde_json::from_str(&market.outcomes).context("Failed to parse outcomes")?;

        // Parse outcome prices
        let prices: Vec<String> = serde_json::from_str(&market.outcome_prices)
            .context("Failed to parse outcome prices")?;

        // Parse CLOB token IDs
        let token_ids: Vec<String> = serde_json::from_str(&market.clob_token_ids)
            .context("Failed to parse CLOB token IDs")?;

        if outcomes.len() != 2 || prices.len() != 2 || token_ids.len() != 2 {
            anyhow::bail!("Expected 2 outcomes, prices, and token IDs");
        }

        // Find Up and Down indices
        let up_idx = outcomes
            .iter()
            .position(|o| o == "Up")
            .context("Could not find 'Up' outcome")?;
        let down_idx = outcomes
            .iter()
            .position(|o| o == "Down")
            .context("Could not find 'Down' outcome")?;

        let up_price: f64 = prices[up_idx].parse().context("Failed to parse Up price")?;
        let down_price: f64 = prices[down_idx]
            .parse()
            .context("Failed to parse Down price")?;

        Ok(ParsedMarketInfo {
            ticker: event.ticker.clone(),
            title: event.title.clone(),
            condition_id: market.condition_id.clone(),
            up_token_id: token_ids[up_idx].clone(),
            down_token_id: token_ids[down_idx].clone(),
            up_price,
            down_price,
            up_best_ask: if up_idx == 0 { market.best_ask } else { None },
            down_best_ask: if down_idx == 0 { market.best_ask } else { None },
        })
    }

    /// Get time until next 5-minute interval
    pub fn time_until_next_interval() -> chrono::Duration {
        let now = Utc::now();
        let next_ts = Self::get_next_5min_timestamp();
        let next_dt = DateTime::from_timestamp(next_ts, 0).unwrap();
        next_dt - now
    }
}
