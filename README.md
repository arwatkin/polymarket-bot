# Polymarket 5-Minute Tracker

A Rust-based trading bot that tracks and trades Polymarket's 5-minute up/down markets using a contrarian mean-reversion strategy.

## Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/polymarket-tracker.git
cd polymarket-tracker

# Build release version
cargo run
```

## Configuration

1. Copy the example configuration:

   ```bash
   cp Config.toml.example Config.toml
   ```

2. Edit `Config.toml` with your settings:

   ```toml
   [[markets]]
   slug = "btc-updown-5m"   # Market slug to track

   [api]
   gamma_base_url = "https://gamma-api.polymarket.com"
   websocket_url = "wss://ws-subscriptions-clob.polymarket.com"

   [trading]
   enabled = true                          # Set to true to enable live trading (false = paper trading)
   private_key = "YOUR_PRIVATE_KEY"        # Your Polymarket private key
   proxy_address = "YOUR_PROXY_ADDRESS"    # Your Polymarket proxy wallet if applicable
   signature_type = 1                      # 0=EOA, 1=POLY_PROXY, 2=GNOSIS_SAFE
   bet_amount = 1                          # Amount in USD per trade (1 = $1)
   tag_threshold = 0.75                    # Monitor when price drops below this
   execute_threshold = 0.80                # Execute when price recovers above this
   monitoring_window_seconds = 120         # Start monitoring when 2 minutes reamin
   max_asset_trades = 1                    # Max trades per asset per window
   max_retries = 5                         # Retry attempts on failed trades
   ```

## Trading Strategy

### Overview

This bot implements a **contrarian mean-reversion strategy** for Polymarket's 5-minute up/down markets:

```
Price drops below 0.75 (tag_threshold) → Begin tracking reversion
Price recovers above 0.80 (execute_threshold) → Execute trade
```

### Logic

1. **Monitoring Phase**:
   - Subscribe to WebSocket price feeds for configured markets to track real-time probability prices

2. **Tag Condition**:
   - When `price < tag_threshold` (default 0.75) within the remaining window, the market is tagged for that side (Yes or No)

3. **Entry Condition**:
   - After tagging, wait for price to recover above `execute_threshold` (default 0.80) to enter the trade.

4. **Execution**:
   - Place a trade at the current market price for the side that is tagged.

5. **Risk Management**:
   - `max_asset_trades = 1`: Only one trade per asset per window prevents overtrading
   - `max_retries = 5`: Handles transient failures gracefully
   - `bet_amount`: Fixed position sizing (default $1)

## Disclaimer

This software is for educational purposes only. Trading cryptocurrency markets and prediction markets carries substantial risk of loss. Past performance does not guarantee future results. Use at your own risk.
