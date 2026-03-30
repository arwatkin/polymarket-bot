use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub markets: Vec<MarketConfig>,
    pub api: ApiConfig,
    pub trading: Option<TradingConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    pub gamma_base_url: String,
    pub websocket_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    pub enabled: bool,
    pub bet_amount: f64,
    pub private_key: String,
    pub proxy_address: String,
    pub signature_type: u8,
    pub tag_threshold: f64,
    pub execute_threshold: f64,
    pub monitoring_window_seconds: u64,
    pub max_asset_trades: u32,
    pub max_retries: u32,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            markets: vec![MarketConfig {
                slug: "btc-updown-5m".to_string(),
            }],
            api: ApiConfig {
                gamma_base_url: "https://gamma-api.polymarket.com".to_string(),
                websocket_url: "wss://ws-subscriptions-clob.polymarket.com".to_string(),
            },
            trading: None,
        }
    }
}
