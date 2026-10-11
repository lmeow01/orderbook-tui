use clap::{Args, Parser, Subcommand};

use crate::exchange::Exchange;

/// Real-time order book DOM ladder in your terminal.
#[derive(Parser, Debug)]
#[command(
    name = "ob",
    version,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
#[command(
    after_help = "Examples:\n  ob binance-futures BTCUSDT\n  ob bybit ETHUSDT --group 10\n  ob coinbase BTC-USD\n  ob symbols bybit sol\n  ob exchanges"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub book: BookArgs,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List the supported exchanges.
    Exchanges,
    /// List an exchange's symbols with their tick sizes.
    Symbols {
        #[arg(value_enum)]
        exchange: Exchange,
        /// Case-insensitive substring filter.
        filter: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct BookArgs {
    #[arg(value_enum, required = true)]
    pub exchange: Option<Exchange>,
    /// Symbol in the exchange's format, e.g. BTCUSDT or BTC-USD.
    #[arg(required = true)]
    pub symbol: Option<String>,
    /// Price group in ticks (0 = pick automatically).
    #[arg(short, long, default_value_t = 0)]
    pub group: i64,
    /// Redraws per second at most.
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// A level is highlighted as big at this multiple of the median visible size.
    #[arg(long, default_value_t = 3.0)]
    pub big: f64,
    /// No TUI: print the top of book once a second.
    #[arg(long)]
    pub headless: bool,
    /// Rows per side to print in headless mode.
    #[arg(long, default_value_t = 5)]
    pub depth: usize,
}
