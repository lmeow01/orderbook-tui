//! DOM ladder rendering.
//!
//! ```text
//!  cum      bar         size │  price  │ size    bar         cum
//!                            │ 83001.0 │ 4.210   ████▌       12.8   asks
//!                            │ 83000.5 │ 8.590   █████████   8.59
//!        spread 0.5 · 0.6bp    ▲ 83000.0   0.015 buy · mark 83000.1
//!   2.10  ██▌         2.100  │ 83000.0 │                            bids
//! ```
//!
//! Bid bars grow left from the bid size column and ask bars grow right from
//! the ask size column. Bar length is relative to the largest visible row.

use std::fmt::Write as _;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::Ui;
use super::theme as t;
use crate::book::Kind;
use crate::book::group::Row;
use crate::view::{FeedState, LadderView};

const SIZE_W: u16 = 9;
const CUM_W: u16 = 9;
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// Column positions for one frame.
struct Cols {
    bid_cum: Option<u16>,
    bid_bar: u16,
    bid_size: u16,
    price: u16,
    price_w: u16,
    ask_size: u16,
    ask_bar: u16,
    ask_cum: Option<u16>,
    bar_w: u16,
}

impl Cols {
    fn new(area: Rect, price_w: u16) -> Self {
        let show_cum = area.width >= 2 * (SIZE_W + CUM_W + 2) + price_w + 16;
        let cum = if show_cum { CUM_W + 1 } else { 0 };
        let fixed = price_w + 2 * (SIZE_W + 1) + 2 * cum;
        let bar_w = area.width.saturating_sub(fixed) / 2;
        let x0 = area.x;
        let bid_cum = show_cum.then_some(x0);
        let bid_bar = x0 + cum;
        let bid_size = bid_bar + bar_w;
        let price = bid_size + SIZE_W + 1;
        let ask_size = price + price_w + 1;
        let ask_bar = ask_size + SIZE_W + 1;
        let ask_cum = show_cum.then_some(ask_bar + bar_w);
        Self {
            bid_cum,
            bid_bar,
            bid_size,
            price,
            price_w,
            ask_size,
            ask_bar,
            ask_cum,
            bar_w,
        }
    }
}

/// Formats a quantity into at most 8 characters with `dec` decimals, so a
/// right-aligned column lines up on the decimal point. Large values switch
/// to K/M/B, and very long ones drop decimals to fit.
pub fn fmt_qty(q: f64, dec: usize, out: &mut String) {
    out.clear();
    let _ = match q {
        q if q <= 0.0 => Ok(()),
        q if q >= 1e9 => write!(out, "{:.2}B", q / 1e9),
        q if q >= 1e6 => write!(out, "{:.2}M", q / 1e6),
        q if q >= 1e5 => write!(out, "{:.1}K", q / 1e3),
        q if q < 0.5 * 10f64.powi(-(dec as i32)) => {
            write!(out, "<{:.dec$}", 10f64.powi(-(dec as i32)))
        }
        q => {
            let int_digits = q.max(1.0).log10() as usize + 1;
            let d = dec.min(7usize.saturating_sub(int_digits));
            write!(out, "{q:.d$}")
        }
    };
}

fn put_right(buf: &mut Buffer, x: u16, y: u16, w: u16, s: &str, style: Style) {
    let len = s.chars().count() as u16;
    let pad = w.saturating_sub(len);
    buf.set_stringn(x + pad, y, s, w as usize, style);
}

fn put_left(buf: &mut Buffer, x: u16, y: u16, w: u16, s: &str, style: Style) {
    buf.set_stringn(x, y, s, w as usize, style);
}

fn fill_bg(buf: &mut Buffer, x: u16, y: u16, w: u16, bg: Color) {
    for i in 0..w {
        if let Some(c) = buf.cell_mut((x + i, y)) {
            c.set_bg(bg);
        }
    }
}

/// Draws a bar of `frac` (0..=1) of `w` cells. Right-anchored bars grow to
/// the left; they only have half-cell resolution since Unicode has no
/// right-aligned eighth blocks besides `▐`.
fn bar(buf: &mut Buffer, x: u16, y: u16, w: u16, frac: f64, color: Color, right_anchored: bool) {
    if w == 0 || frac <= 0.0 {
        return;
    }
    let style = Style::new().fg(color);
    let eighths = ((frac.min(1.0) * w as f64 * 8.0).round() as u16).max(1);
    let full = eighths / 8;
    let rem = eighths % 8;
    if right_anchored {
        let half = rem >= 4;
        let cells = full + half as u16;
        let start = x + w - cells;
        if half {
            buf.set_string(start, y, "▐", style);
        }
        for i in 0..full {
            buf.set_string(start + half as u16 + i, y, "█", style);
        }
    } else {
        for i in 0..full {
            buf.set_string(x + i, y, "█", style);
        }
        if rem > 0 && full < w {
            buf.set_string(x + full, y, EIGHTHS[rem as usize].to_string(), style);
        }
    }
}

/// Visible-size statistics used to decide what counts as "big".
struct SizeStats {
    max: f64,
    big_at: f64,
    max_bid: f64,
    max_ask: f64,
}

fn size_stats(bids: &[Row], asks: &[Row], big_k: f64) -> SizeStats {
    let mut nz: [f64; 512] = [0.0; 512];
    let mut n = 0;
    let (mut max_bid, mut max_ask) = (0.0f64, 0.0f64);
    for r in bids {
        max_bid = max_bid.max(r.qty);
        if r.qty > 0.0 && n < nz.len() {
            nz[n] = r.qty;
            n += 1;
        }
    }
    for r in asks {
        max_ask = max_ask.max(r.qty);
        if r.qty > 0.0 && n < nz.len() {
            nz[n] = r.qty;
            n += 1;
        }
    }
    let median = if n == 0 {
        0.0
    } else {
        let s = &mut nz[..n];
        let mid = n / 2;
        s.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        s[mid]
    };
    SizeStats {
        max: max_bid.max(max_ask),
        big_at: median * big_k,
        max_bid,
        max_ask,
    }
}

pub fn draw(buf: &mut Buffer, area: Rect, v: &LadderView, ui: &Ui, now: Instant) {
    if area.height < 5 || area.width < 30 {
        put_left(
            buf,
            area.x,
            area.y,
            area.width,
            "terminal too small",
            Style::new().fg(t::DIM),
        );
        return;
    }
    let stale = v.state != FeedState::Live;
    header(buf, area, v, ui);
    footer(buf, area, v, ui);

    let body = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height - 3,
    };
    if !v.has_book {
        let msg = match v.state {
            FeedState::Connecting => format!("connecting to {} …", ui.exchange),
            FeedState::Syncing => "waiting for book snapshot …".to_string(),
            _ => format!("reconnecting: {}", v.reason),
        };
        let y = body.y + body.height / 2;
        let x = body.x + body.width.saturating_sub(msg.chars().count() as u16) / 2;
        put_left(buf, x, y, body.width, &msg, Style::new().fg(t::DIM));
        return;
    }

    let (asks_vis, bids_vis) = ui.split(body.height);
    let mid_y = body.y + asks_vis as u16;
    let asks = &v.asks[..v.rows.min(asks_vis)];
    let bids = &v.bids[..v.rows.min(bids_vis)];

    let scale = ui.scale;
    let mut s = String::with_capacity(24);
    let ref_px = v.best_ask.or(v.best_bid).map_or(0, |l| l.px);
    scale.write(ref_px, &mut s);
    let price_w = (s.len() as u16 + 4).max(10);
    let cols = Cols::new(body, price_w);

    // Column titles.
    let title = Style::new().fg(t::FAINT);
    if let Some(x) = cols.bid_cum {
        put_right(buf, x, area.y + 1, CUM_W, "cum", title);
    }
    put_right(buf, cols.bid_size, area.y + 1, SIZE_W, "bid", title);
    put_right(
        buf,
        cols.price,
        area.y + 1,
        cols.price_w - 2,
        "price",
        title,
    );
    put_right(buf, cols.ask_size, area.y + 1, SIZE_W, "ask", title);
    if let Some(x) = cols.ask_cum {
        put_right(buf, x, area.y + 1, CUM_W, "cum", title);
    }

    let stats = size_stats(bids, asks, ui.big_k);
    let last_px = v.tape.last_px;
    let g = v.group.max(1);

    for (i, r) in asks.iter().enumerate() {
        let y = mid_y - 1 - i as u16;
        let traded = last_px.is_some_and(|p| p <= r.px && p > r.px - g);
        row(
            buf,
            &cols,
            y,
            Kind::Ask,
            r,
            traded,
            &stats,
            ui,
            now,
            stale,
            &mut s,
        );
    }
    for (i, r) in bids.iter().enumerate() {
        let y = mid_y + 1 + i as u16;
        if y >= body.y + body.height {
            break;
        }
        let traded = last_px.is_some_and(|p| p >= r.px && p < r.px + g);
        row(
            buf,
            &cols,
            y,
            Kind::Bid,
            r,
            traded,
            &stats,
            ui,
            now,
            stale,
            &mut s,
        );
    }
    mid_row(buf, body, &cols, mid_y, v, ui, stale, &mut s);
}

#[allow(clippy::too_many_arguments)]
fn row(
    buf: &mut Buffer,
    c: &Cols,
    y: u16,
    kind: Kind,
    r: &Row,
    traded: bool,
    st: &SizeStats,
    ui: &Ui,
    now: Instant,
    stale: bool,
    s: &mut String,
) {
    let has = r.qty > 0.0;
    let big = has && st.big_at > 0.0 && r.qty >= st.big_at;
    let side_max = match kind {
        Kind::Bid => st.max_bid,
        Kind::Ask => st.max_ask,
    };
    let largest = has && r.qty >= side_max && big;
    let (px_col, px_empty, bar_c, bar_big, flash_bg) = match kind {
        Kind::Bid => (
            t::BID,
            t::BID_PX_EMPTY,
            t::BID_BAR,
            t::BID_BAR_BIG,
            t::BID_FLASH,
        ),
        Kind::Ask => (
            t::ASK,
            t::ASK_PX_EMPTY,
            t::ASK_BAR,
            t::ASK_BAR_BIG,
            t::ASK_FLASH,
        ),
    };
    let fade = |c: Color| if stale { t::FAINT } else { c };

    // Price.
    s.clear();
    ui.scale.write(r.px, s);
    let mut px_style = Style::new().fg(fade(if has { px_col } else { px_empty }));
    if traded && !stale {
        px_style = px_style
            .fg(t::TEXT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    }
    put_right(buf, c.price, y, c.price_w - 2, s, px_style);

    // Size, bar, cumulative.
    let (size_x, bar_x, cum_x) = match kind {
        Kind::Bid => (c.bid_size, c.bid_bar, c.bid_cum),
        Kind::Ask => (c.ask_size, c.ask_bar, c.ask_cum),
    };
    if has {
        let flashing = !stale && ui.flash(kind, r.px, now).is_some();
        if flashing {
            fill_bg(buf, size_x, y, SIZE_W, flash_bg);
        }
        let mut st_size = Style::new().fg(fade(if big { px_col } else { t::TEXT }));
        if big {
            st_size = st_size.add_modifier(Modifier::BOLD);
        }
        if largest && !stale {
            fill_bg(buf, size_x, y, SIZE_W, px_col);
            st_size = Style::new()
                .fg(t::INK)
                .bg(px_col)
                .add_modifier(Modifier::BOLD);
        }
        fmt_qty(r.qty, ui.qty_dec, s);
        put_right(buf, size_x, y, SIZE_W, s, st_size);
        if st.max > 0.0 {
            let color = fade(if big { bar_big } else { bar_c });
            bar(
                buf,
                bar_x,
                y,
                c.bar_w.saturating_sub(1),
                r.qty / st.max,
                color,
                kind == Kind::Bid,
            );
        }
    }
    if let Some(x) = cum_x {
        fmt_qty(r.cum, ui.qty_dec, s);
        put_right(buf, x, y, CUM_W, s, Style::new().fg(fade(t::DIM)));
    }
}

#[allow(clippy::too_many_arguments)]
fn mid_row(
    buf: &mut Buffer,
    body: Rect,
    c: &Cols,
    y: u16,
    v: &LadderView,
    ui: &Ui,
    stale: bool,
    s: &mut String,
) {
    fill_bg(buf, body.x, y, body.width, t::MID_BG);
    let base = Style::new().bg(t::MID_BG);
    let dim = base.fg(t::DIM);

    // Spread on the bid side.
    if let (Some(b), Some(a)) = (v.best_bid, v.best_ask) {
        let spread = a.px - b.px;
        let mid = ui.scale.to_f64(a.px + b.px) / 2.0;
        let bps = if mid > 0.0 {
            ui.scale.to_f64(spread) / mid * 1e4
        } else {
            0.0
        };
        s.clear();
        s.push_str("spread ");
        ui.scale.write(spread, s);
        let _ = if bps < 1.0 {
            write!(s, " · {bps:.2}bp")
        } else {
            write!(s, " · {bps:.1}bp")
        };
        put_right(buf, body.x, y, c.price - body.x - 1, s, dim);
    }

    // Last trade in the price column.
    let tape = &v.tape;
    if let Some(px) = tape.last_px {
        let (arrow, color) = match tape.tick_dir {
            d if d > 0 => ("▲ ", t::BID),
            d if d < 0 => ("▼ ", t::ASK),
            _ => ("  ", t::TEXT),
        };
        s.clear();
        s.push_str(arrow);
        ui.scale.write(px, s);
        let color = if stale { t::DIM } else { color };
        put_right(
            buf,
            c.price,
            y,
            c.price_w - 2,
            s,
            base.fg(color).add_modifier(Modifier::BOLD),
        );

        // Last print size and mark on the ask side.
        let mut q = String::with_capacity(12);
        fmt_qty(tape.last_qty, ui.qty_dec, &mut q);
        s.clear();
        let _ = write!(s, " {q} {}", if tape.last_buy { "buy" } else { "sell" });
        if !tape.mark.is_empty() {
            let _ = write!(s, "  ·  mark {}", tape.mark.as_str());
        }
        let x = c.ask_size;
        put_left(buf, x, y, (body.x + body.width).saturating_sub(x), s, dim);
    } else if !tape.mark.is_empty() {
        s.clear();
        let _ = write!(s, " mark {}", tape.mark.as_str());
        put_left(
            buf,
            c.ask_size,
            y,
            (body.x + body.width).saturating_sub(c.ask_size),
            s,
            dim,
        );
    }
}

fn header(buf: &mut Buffer, area: Rect, v: &LadderView, ui: &Ui) {
    let y = area.y;
    let mut s = String::with_capacity(64);
    let _ = write!(s, " {}  ", ui.exchange);
    put_left(buf, area.x, y, area.width, &s, Style::new().fg(t::DIM));
    let x = area.x + s.chars().count() as u16;
    put_left(
        buf,
        x,
        y,
        area.width.saturating_sub(x),
        &ui.symbol,
        Style::new().fg(t::TEXT).add_modifier(Modifier::BOLD),
    );
    let x = x + ui.symbol.len() as u16;
    s.clear();
    s.push_str("   group ");
    ui.scale.write(v.group.max(1), &mut s);
    put_left(
        buf,
        x,
        y,
        area.width.saturating_sub(x),
        &s,
        Style::new().fg(t::DIM),
    );

    // Right side: state, rate, latency.
    s.clear();
    let (label, color) = match v.state {
        FeedState::Live => ("● live", t::BID),
        FeedState::Syncing => ("◌ syncing", t::AMBER),
        FeedState::Connecting => ("◌ connecting", t::AMBER),
        FeedState::Stale => ("✕ stale", t::ASK),
    };
    let mut right = String::with_capacity(64);
    if v.state == FeedState::Stale || (!v.reason.is_empty() && v.state != FeedState::Live) {
        let _ = write!(right, "{}  ", v.reason);
        if v.retry_in_ms > 0 {
            let _ = write!(right, "retry {:.1}s  ", v.retry_in_ms as f64 / 1000.0);
        }
    }
    let _ = write!(s, "{} msg/s", v.msgs_per_sec);
    if let Some(l) = v.latency_ms {
        let _ = write!(s, "  {l} ms");
    }
    if v.reconnects > 0 {
        let _ = write!(s, "  ↻{}", v.reconnects);
    }
    s.push(' ');
    let total = right.chars().count() + label.chars().count() + 2 + s.chars().count();
    let mut x = (area.x + area.width).saturating_sub(total as u16);
    put_left(
        buf,
        x,
        y,
        right.chars().count() as u16,
        &right,
        Style::new().fg(t::DIM),
    );
    x += right.chars().count() as u16;
    put_left(
        buf,
        x,
        y,
        label.chars().count() as u16,
        label,
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    );
    x += label.chars().count() as u16 + 2;
    put_left(
        buf,
        x,
        y,
        s.chars().count() as u16,
        &s,
        Style::new().fg(t::DIM),
    );
}

fn footer(buf: &mut Buffer, area: Rect, v: &LadderView, ui: &Ui) {
    let y = area.y + area.height - 1;
    let keys = " +/- group  [/] ×10  a auto   ↑↓ scroll   c centre   r reconnect   R resync   q quit";
    put_left(buf, area.x, y, area.width, keys, Style::new().fg(t::FAINT));
    let mut s = String::with_capacity(48);
    if v.has_book {
        let _ = write!(
            s,
            "{} bids · {} asks · big ≥{}× median ",
            v.bid_levels, v.ask_levels, ui.big_k
        );
    }
    put_right(buf, area.x, y, area.width, &s, Style::new().fg(t::FAINT));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qty_formats_fit() {
        let mut s = String::new();
        for (q, dec, want) in [
            (0.0, 3, ""),
            (0.00002389, 5, "0.00002"),
            (0.000001, 5, "<0.00001"),
            (0.0004, 3, "<0.001"),
            (11.374, 3, "11.374"),
            (0.5, 3, "0.500"),
            (1234.5, 3, "1234.500"),
            (12345.678, 3, "12345.68"),
            (123_456.0, 3, "123.5K"),
            (2_500_000.0, 0, "2.50M"),
            (42.0, 0, "42"),
        ] {
            fmt_qty(q, dec, &mut s);
            assert_eq!(s, want);
            assert!(s.len() <= 8);
        }
    }

    #[test]
    fn stats_pick_median() {
        let mk = |q: f64| Row {
            px: 0,
            qty: q,
            cum: 0.0,
        };
        let bids = [mk(1.0), mk(2.0), mk(0.0)];
        let asks = [mk(3.0), mk(30.0)];
        let st = size_stats(&bids, &asks, 3.0);
        assert_eq!(st.max, 30.0);
        assert_eq!(st.big_at, 9.0); // median 3 × 3
        assert_eq!((st.max_bid, st.max_ask), (2.0, 30.0));
    }
}
