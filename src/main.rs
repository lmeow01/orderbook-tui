use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use ob::book::PriceScale;
use ob::cli::{BookArgs, Cli, Command};
use ob::exchange::{self, Exchange};
use ob::feed::{self, Control, FeedConfig};
use ob::ui::{self, Ui, ladder::fmt_qty};
use ob::view::{FeedState, LadderView};

fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    init_logging();
    match cli.command {
        Some(Command::Exchanges) => exchanges(),
        Some(Command::Symbols { exchange, filter }) => symbols(exchange, filter),
        None => book(cli.book),
    }
}

/// Logs go to a file because the TUI owns the terminal. Set `OB_LOG=debug`
/// for more detail.
fn init_logging() {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ob");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ob.log"))
    else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("OB_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
}

fn block_on<F: std::future::Future>(f: F) -> Result<F::Output> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(f))
}

/// Lists every supported exchange with its aliases and market, taken from
/// the `Exchange` enum so the list can't drift from what the CLI accepts.
fn exchanges() -> Result<()> {
    let mut out = String::new();
    let _ = writeln!(out, "{:<16} {:<10} MARKET", "EXCHANGE", "ALIASES");
    for ex in Exchange::value_variants() {
        let Some(v) = ex.to_possible_value() else {
            continue;
        };
        let aliases = v.get_name_and_aliases().skip(1).collect::<Vec<_>>().join(", ");
        let market = v.get_help().map(|h| h.to_string()).unwrap_or_default();
        let _ = writeln!(out, "{:<16} {:<10} {market}", v.get_name(), aliases);
    }
    // Ignore broken pipes from `| head`.
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
    Ok(())
}

fn symbols(ex: Exchange, filter: Option<String>) -> Result<()> {
    let http = feed::http_client()?;
    let list = block_on(exchange::list_instruments(&http, ex))??;
    let f = filter.map(|f| f.to_ascii_uppercase());
    let mut rows: Vec<_> = list
        .iter()
        .filter(|i| {
            f.as_deref()
                .is_none_or(|f| i.symbol.to_ascii_uppercase().contains(f))
        })
        .collect();
    // Symbols that start with the filter first; the sort is stable, so
    // each group stays alphabetical.
    if let Some(f) = f.as_deref() {
        rows.sort_by_key(|i| !i.symbol.to_ascii_uppercase().starts_with(f));
    }
    let w = rows
        .iter()
        .map(|i| i.symbol.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<w$}  {:<8} {:<6} {:>12}  STATUS",
        "SYMBOL", "BASE", "QUOTE", "TICK"
    );
    for i in &rows {
        let _ = writeln!(
            out,
            "{:<w$}  {:<8} {:<6} {:>12}  {}",
            i.symbol, i.base, i.quote, i.tick, i.status
        );
    }
    let mut stdout = std::io::stdout().lock();
    // Ignore broken pipes from `| head`.
    let _ = stdout.write_all(out.as_bytes());
    if rows.is_empty() {
        bail!("no symbols match");
    }
    Ok(())
}

fn book(args: BookArgs) -> Result<()> {
    let ex = args.exchange.context("missing exchange")?;
    let symbol = ex.normalize_symbol(args.symbol.as_deref().context("missing symbol")?);
    let http = feed::http_client()?;
    let inst = block_on(exchange::find_instrument(&http, ex, &symbol))??;
    let scale = PriceScale::from_tick(&inst.tick)
        .with_context(|| format!("bad tick size {:?}", inst.tick))?;
    // Size decimals follow the lot step, capped so dust doesn't widen the column.
    let qty_dec = PriceScale::from_tick(&inst.lot)
        .map_or(3, |s| s.decimals() as usize)
        .min(5);
    tracing::info!(
        exchange = ex.name(),
        symbol,
        tick = inst.tick,
        lot = inst.lot,
        "starting"
    );

    let ctl = Arc::new(Control::new(
        args.group.max(0),
        if args.headless { args.depth.max(1) } else { 40 },
    ));
    let (input, output) = triple_buffer::triple_buffer(&LadderView::default());
    feed::spawn(
        FeedConfig {
            exchange: ex,
            symbol: symbol.clone(),
            scale,
        },
        ctl.clone(),
        input,
    )?;

    if args.headless {
        return headless(output, scale, qty_dec, args.depth.max(1));
    }
    ui::run(
        output,
        ctl,
        Ui::new(ex.name(), symbol, scale, qty_dec, args.big.max(1.0)),
        args.fps,
    )
}

/// Prints the top of book once a second, for scripting and sanity checks.
fn headless(
    mut out: triple_buffer::Output<LadderView>,
    scale: PriceScale,
    qty_dec: usize,
    depth: usize,
) -> Result<()> {
    let mut q = String::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let v = out.read();
        let mut line = format!("{:?} v{} {} msg/s", v.state, v.version, v.msgs_per_sec);
        if let Some(l) = v.latency_ms {
            let _ = write!(line, " {l}ms");
        }
        if v.state != FeedState::Live {
            let _ = write!(line, " ({})", v.reason);
        }
        if let (Some(b), Some(a)) = (v.best_bid, v.best_ask) {
            let _ = write!(
                line,
                " | bid {} x {} | ask {} x {} | crossed={}",
                scale.format(b.px),
                b.qty,
                scale.format(a.px),
                a.qty,
                b.px >= a.px
            );
        }
        if let Some(px) = v.tape.last_px {
            let _ = write!(line, " | last {}", scale.format(px));
        }
        if !v.tape.mark.is_empty() {
            let _ = write!(line, " mark {}", v.tape.mark.as_str());
        }
        println!("{line}");
        let n = v.rows.min(depth);
        for i in (0..n).rev() {
            fmt_qty(v.asks[i].qty, qty_dec, &mut q);
            println!("    ask {:>14} {:>10}", scale.format(v.asks[i].px), q);
        }
        for i in 0..n {
            fmt_qty(v.bids[i].qty, qty_dec, &mut q);
            println!("    bid {:>14} {:>10}", scale.format(v.bids[i].px), q);
        }
    }
}
