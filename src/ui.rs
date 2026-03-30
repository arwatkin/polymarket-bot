use chrono::Utc;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
};
use std::sync::Arc;

use crate::state::AppState;

pub fn draw(frame: &mut Frame, state: &Arc<AppState>) {
    // Main layout: split into left and right
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40), // Left side - market prices + status
            Constraint::Percentage(60), // Right side - trades + stats
        ])
        .split(frame.size());

    // Calculate how much space markets need
    let num_markets = state.tracked_markets.read().len().max(1);
    let header_height = 4;
    let market_height = 6;
    let markets_total_height = header_height + (num_markets * market_height) as u16;

    // Left side: markets on top (exact fit), status takes remaining
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(markets_total_height), // Markets - exact size needed
            Constraint::Min(5),                       // Status - takes all remaining
        ])
        .split(main_chunks[0]);

    // Draw left side
    draw_markets(frame, left_chunks[0], state);
    draw_status(frame, left_chunks[1], state);

    // Draw right side (full height trades)
    draw_trades(frame, main_chunks[1], state);
}

fn draw_markets(frame: &mut Frame, area: Rect, state: &Arc<AppState>) {
    let tracked = state.tracked_markets.read();
    let prices = state.current_prices.read();
    let ws_connected = *state.ws_connected.read();
    let next_interval = *state.next_interval.read();

    // Calculate time until next interval
    let time_remaining = if next_interval > 0 {
        let now = Utc::now().timestamp();
        let remaining = next_interval - now;
        if remaining > 0 {
            format!("{}:{:02}", remaining / 60, remaining % 60)
        } else {
            "Refreshing...".to_string()
        }
    } else {
        "Unknown".to_string()
    };

    // Connection status indicator
    let conn_status = if ws_connected {
        Span::styled("● CONNECTED", Style::default().fg(Color::Green))
    } else {
        Span::styled("● DISCONNECTED", Style::default().fg(Color::Red))
    };

    let header_text = vec![
        Line::from(vec![Span::styled(
            "POLYMARKET 5M CRYPTO TRADER",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::raw("Next Interval: "),
            Span::styled(&time_remaining, Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            conn_status,
        ]),
    ];

    // Create layout for header and market tables
    let num_markets = tracked.len().max(1);
    let mut constraints = vec![Constraint::Length(4)]; // Header
    for _ in 0..num_markets {
        constraints.push(Constraint::Length(6)); // Each market table
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // Draw header
    let header_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let header = Paragraph::new(header_text)
        .block(header_block)
        .wrap(Wrap { trim: true });

    frame.render_widget(header, chunks[0]);

    // Draw each market as a small table
    let mut market_idx = 1;
    let mut sorted_markets: Vec<_> = tracked.iter().collect();
    sorted_markets.sort_by_key(|(slug, _)| *slug);

    for (slug, info) in sorted_markets {
        if market_idx >= chunks.len() {
            break;
        }

        let market_prices = prices.get(slug);

        let up_ask = market_prices
            .and_then(|p| p.up_best_ask)
            .map(|p| format!("{:.3}", p))
            .unwrap_or_else(|| "-".to_string());

        let down_ask = market_prices
            .and_then(|p| p.down_best_ask)
            .map(|p| format!("{:.3}", p))
            .unwrap_or_else(|| "-".to_string());

        // Colorize based on prices
        let up_color = if market_prices.and_then(|p| p.up_best_ask).unwrap_or(0.0) > 0.5 {
            Color::Green
        } else {
            Color::White
        };

        let down_color = if market_prices.and_then(|p| p.down_best_ask).unwrap_or(0.0) > 0.5 {
            Color::Red
        } else {
            Color::White
        };

        let rows = vec![
            Row::new(vec![
                Cell::from("UP").style(Style::default().fg(Color::Green)),
                Cell::from(up_ask)
                    .style(Style::default().fg(up_color).add_modifier(Modifier::BOLD)),
            ]),
            Row::new(vec![
                Cell::from("DOWN").style(Style::default().fg(Color::Red)),
                Cell::from(down_ask)
                    .style(Style::default().fg(down_color).add_modifier(Modifier::BOLD)),
            ]),
        ];

        let title = extract_asset_name(&info.ticker);

        let table = Table::new(rows, [Constraint::Length(8), Constraint::Length(12)])
            .header(
                Row::new(vec![Cell::from("SIDE"), Cell::from("BUY")])
                    .style(Style::default().add_modifier(Modifier::BOLD))
                    .bottom_margin(0),
            )
            .block(
                Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Magenta)),
            );

        frame.render_widget(table, chunks[market_idx]);
        market_idx += 1;
    }

    // If no markets, show placeholder
    if tracked.is_empty() {
        let placeholder = Paragraph::new("Loading markets...")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
        frame.render_widget(placeholder, chunks[1]);
    }
}

fn draw_trades(frame: &mut Frame, area: Rect, state: &Arc<AppState>) {
    // Split into trades table (top) and stats table (bottom)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),   // Trades table - most space
            Constraint::Length(9), // Stats table - fixed height
        ])
        .split(area);

    // Draw trades table
    draw_trades_table(frame, chunks[0], state);

    // Draw stats table
    draw_stats_table(frame, chunks[1], state);
}

fn draw_trades_table(frame: &mut Frame, area: Rect, state: &Arc<AppState>) {
    let trades = state.trades.read();

    // Filter for only successful trades
    let successful_trades: Vec<_> = trades
        .iter()
        .filter(|t| t.status == crate::state::TradeStatus::Success)
        .collect();

    if successful_trades.is_empty() {
        let placeholder = Paragraph::new(
            "No successful trades yet. Waiting for signals in the 3-minute window...",
        )
        .style(Style::default().fg(Color::DarkGray))
        .block(
            Block::default()
                .title(" Trades ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .wrap(Wrap { trim: true });
        frame.render_widget(placeholder, area);
        return;
    }

    // Calculate how many trades we can show based on available height
    // Each row takes 1 line, header takes 2 lines (1 for text + 1 margin), borders take 2 lines
    let max_trades_to_show = (area.height.saturating_sub(4)) as usize;

    // Create table rows - show only successful trades, up to the available space
    let rows: Vec<Row> = successful_trades
        .iter()
        .take(max_trades_to_show)
        .map(|trade| {
            let timestamp = trade.timestamp.format("%H:%M:%S").to_string();

            let side_color = if trade.side == "UP" {
                Color::Green
            } else {
                Color::Red
            };

            Row::new(vec![
                Cell::from(timestamp),
                Cell::from(trade.asset.clone()),
                Cell::from(trade.side.clone()).style(Style::default().fg(side_color)),
                Cell::from(format!("{:.3}", trade.price)),
                Cell::from(format!("${:.2}", trade.amount)),
                Cell::from("SUCCESS").style(Style::default().fg(Color::Green)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(16), // Timestamp
            Constraint::Percentage(16), // Asset
            Constraint::Percentage(16), // Side
            Constraint::Percentage(16), // Price
            Constraint::Percentage(16), // Amount
            Constraint::Percentage(20), // Status
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("Time"),
            Cell::from("Asset"),
            Cell::from("Side"),
            Cell::from("Price"),
            Cell::from("Amount"),
            Cell::from("Status"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .bottom_margin(1),
    )
    .block(
        Block::default()
            .title(" Trades ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
    );

    frame.render_widget(table, area);
}

fn draw_stats_table(frame: &mut Frame, area: Rect, state: &Arc<AppState>) {
    let stats = state.get_trading_stats();
    let trading_enabled = *state.trading_enabled.read();

    // Trading status indicator
    let status_line = if trading_enabled {
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(
                "LIVE TRADING",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled("SIMULATION", Style::default().fg(Color::Yellow)),
        ])
    };

    let content = vec![
        status_line,
        Line::from(format!("Trade Count:   {}", stats.trade_count)),
        Line::from(format!("Win Count:     {}", stats.win_count)),
        Line::from(format!("Loss Count:    {}", stats.loss_count)),
        Line::from(format!("Strike Rate:   {:.1}%", stats.strike_rate)),
        Line::from(format!("P&L:           ${:.2}", stats.pnl)),
    ];

    let paragraph = Paragraph::new(content).block(
        Block::default()
            .title(" Trading Stats ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, area: Rect, state: &Arc<AppState>) {
    let status = state.status_messages.read();

    // Calculate how many lines we can fit (area height minus 2 for borders)
    let max_lines = area.height.saturating_sub(2) as usize;

    let lines: Vec<Line> = status
        .iter()
        .take(max_lines)
        .map(|msg| {
            let color = if msg.contains("Error") || msg.contains("error") {
                Color::Red
            } else if msg.contains("Connected") || msg.contains("Subscribed") {
                Color::Green
            } else {
                Color::DarkGray
            };

            Line::from(Span::styled(msg.as_str(), Style::default().fg(color)))
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Status ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: true });

    frame.render_widget(paragraph, area);
}

/// Extract asset name from ticker (e.g., "btc-updown-5m-123456" -> "BTC")
pub fn extract_asset_name(ticker: &str) -> String {
    ticker.split('-').next().unwrap_or("???").to_uppercase()
}
