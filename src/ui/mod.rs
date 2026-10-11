//! Terminal UI: reads the latest `LadderView` each frame and draws the ladder.

pub mod ladder;
pub mod theme;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use triple_buffer::Output;

use crate::book::group::{Row, step_group};
use crate::book::{Kind, PriceScale, Ticks};
use crate::feed::Control;
use crate::view::{LadderView, MAX_ROWS};

const FLASH: Duration = Duration::from_millis(350);

struct Flash {
    kind: Kind,
    px: Ticks,
    at: Instant,
}

/// UI-side state that outlives a frame.
pub struct Ui {
    pub exchange: &'static str,
    pub symbol: String,
    pub scale: PriceScale,
    pub big_k: f64,
    /// Decimals shown for sizes.
    pub qty_dec: usize,
    /// Positive shows more asks, negative more bids.
    scroll: i32,
    prev_bids: Vec<Row>,
    prev_asks: Vec<Row>,
    prev_group: Ticks,
    flashes: Vec<Flash>,
}

impl Ui {
    pub fn new(
        exchange: &'static str,
        symbol: String,
        scale: PriceScale,
        qty_dec: usize,
        big_k: f64,
    ) -> Self {
        Self {
            exchange,
            symbol,
            scale,
            big_k,
            qty_dec,
            scroll: 0,
            prev_bids: Vec::with_capacity(MAX_ROWS),
            prev_asks: Vec::with_capacity(MAX_ROWS),
            prev_group: 0,
            flashes: Vec::with_capacity(64),
        }
    }

    /// Splits `body` rows (minus the middle row) between asks and bids.
    fn split(&self, body: u16) -> (usize, usize) {
        let n = body.saturating_sub(1) as i32;
        let half = n / 2;
        let asks = (half + self.scroll).clamp(0, n);
        (asks as usize, (n - asks) as usize)
    }

    fn rows_wanted(&self, height: u16) -> usize {
        let (a, b) = self.split(height.saturating_sub(3));
        a.max(b).clamp(1, MAX_ROWS)
    }

    fn flash(&self, kind: Kind, px: Ticks, now: Instant) -> Option<&Flash> {
        self.flashes
            .iter()
            .find(|f| f.kind == kind && f.px == px && now - f.at < FLASH)
    }

    fn flashing(&self, now: Instant) -> bool {
        self.flashes.iter().any(|f| now - f.at < FLASH)
    }

    /// Records which rows changed size since the previous view.
    fn observe(&mut self, v: &LadderView, now: Instant) {
        self.flashes.retain(|f| now - f.at < FLASH);
        let rows = v.rows;
        if v.group != self.prev_group || !v.has_book {
            self.prev_group = v.group;
            self.prev_bids.clear();
            self.prev_asks.clear();
            self.flashes.clear();
        } else {
            diff(
                Kind::Bid,
                &self.prev_bids,
                &v.bids[..rows],
                now,
                &mut self.flashes,
            );
            diff(
                Kind::Ask,
                &self.prev_asks,
                &v.asks[..rows],
                now,
                &mut self.flashes,
            );
        }
        self.prev_bids.clear();
        self.prev_bids.extend_from_slice(&v.bids[..rows]);
        self.prev_asks.clear();
        self.prev_asks.extend_from_slice(&v.asks[..rows]);
    }
}

/// Flags rows whose size changed. Both slices run outward from the best
/// price, so a single merge pass pairs them up.
fn diff(kind: Kind, prev: &[Row], cur: &[Row], now: Instant, out: &mut Vec<Flash>) {
    let outward = |a: Ticks, b: Ticks| match kind {
        Kind::Bid => b.cmp(&a),
        Kind::Ask => a.cmp(&b),
    };
    let (mut i, mut j) = (0, 0);
    while i < prev.len() && j < cur.len() {
        match outward(prev[i].px, cur[j].px) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let (p, c) = (prev[i], cur[j]);
                if p.qty != c.qty && c.qty > 0.0 {
                    out.retain(|f| !(f.kind == kind && f.px == c.px));
                    out.push(Flash {
                        kind,
                        px: c.px,
                        at: now,
                    });
                }
                i += 1;
                j += 1;
            }
        }
    }
}

pub fn run(mut out: Output<LadderView>, ctl: Arc<Control>, mut ui: Ui, fps: u32) -> Result<()> {
    let mut term = ratatui::init();
    let res = event_loop(&mut term, &mut out, &ctl, &mut ui, fps);
    ratatui::restore();
    res
}

fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    out: &mut Output<LadderView>,
    ctl: &Control,
    ui: &mut Ui,
    fps: u32,
) -> Result<()> {
    let frame = Duration::from_secs_f64(1.0 / fps.clamp(1, 240) as f64);
    let mut next = Instant::now();
    let mut dirty = true;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    let ctrl_c =
                        k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
                    match k.code {
                        _ if ctrl_c => return Ok(()),
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('+' | '=') => regroup(ctl, out, true, 1),
                        KeyCode::Char('-' | '_') => regroup(ctl, out, false, 1),
                        // Three 1-2-5 steps is exactly ×10 or ÷10.
                        KeyCode::Char(']' | '}') => regroup(ctl, out, true, 3),
                        KeyCode::Char('[' | '{') => regroup(ctl, out, false, 3),
                        KeyCode::Char('a') => {
                            ctl.group.store(0, Ordering::Relaxed);
                            ctl.notify.notify_one();
                        }
                        KeyCode::Up | KeyCode::Char('k') => ui.scroll += 1,
                        KeyCode::Down | KeyCode::Char('j') => ui.scroll -= 1,
                        KeyCode::Char('c') => ui.scroll = 0,
                        KeyCode::Char('r') => {
                            ctl.reconnect.store(true, Ordering::Relaxed);
                            ctl.notify.notify_one();
                        }
                        KeyCode::Char('R') => {
                            ctl.drop_now.store(true, Ordering::Relaxed);
                            ctl.notify.notify_one();
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
            continue;
        }

        let now = Instant::now();
        next = now + frame;
        let size = term.size()?;
        let max_scroll = (size.height as i32 - 4) / 2;
        ui.scroll = ui.scroll.clamp(-max_scroll.max(0), max_scroll.max(0));
        let want = ui.rows_wanted(size.height);
        if ctl.rows.swap(want, Ordering::Relaxed) != want {
            ctl.notify.notify_one();
        }
        let updated = out.update();
        if updated {
            ui.observe(out.output_buffer(), now);
        }
        if updated || dirty || ui.flashing(now) {
            let v = out.output_buffer();
            term.draw(|f| {
                let area = f.area();
                ladder::draw(f.buffer_mut(), area, v, ui, now);
            })?;
            dirty = false;
        }
    }
}

/// Steps from `ctl.group` rather than the last drawn view, so presses
/// faster than the feed republishes still compound.
fn regroup(ctl: &Control, out: &Output<LadderView>, up: bool, steps: u32) {
    let mut g = match ctl.group.load(Ordering::Relaxed) {
        g if g > 0 => g,
        _ => out.output_buffer().group.max(1),
    };
    for _ in 0..steps {
        g = step_group(g, up);
    }
    ctl.group.store(g, Ordering::Relaxed);
    ctl.notify.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(px: Ticks, qty: f64) -> Row {
        Row { px, qty, cum: 0.0 }
    }

    #[test]
    fn diff_flags_changed_rows() {
        let now = Instant::now();
        let mut out = Vec::new();
        let prev = [r(100, 1.0), r(99, 2.0), r(98, 3.0)];
        let cur = [r(101, 5.0), r(100, 1.0), r(99, 2.5), r(98, 0.0)];
        diff(Kind::Bid, &prev, &cur, now, &mut out);
        let px: Vec<_> = out.iter().map(|f| f.px).collect();
        assert_eq!(px, [99]);

        out.clear();
        let prev = [r(10, 1.0), r(11, 1.0)];
        let cur = [r(11, 2.0), r(12, 1.0)];
        diff(Kind::Ask, &prev, &cur, now, &mut out);
        assert_eq!(out.iter().map(|f| f.px).collect::<Vec<_>>(), [11]);
    }

    #[test]
    fn split_respects_scroll() {
        let mut ui = Ui::new("x", "Y".into(), PriceScale::from_tick("1").unwrap(), 3, 3.0);
        assert_eq!(ui.split(21), (10, 10));
        ui.scroll = 3;
        assert_eq!(ui.split(21), (13, 7));
        ui.scroll = -50;
        assert_eq!(ui.split(21), (0, 20));
    }
}
