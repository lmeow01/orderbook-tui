//! Groups levels into continuous price rows for the DOM ladder.
//!
//! Rows are contiguous `g`-tick buckets running outward from the best price,
//! so a price with no resting size still gets a row and the ladder reads as
//! an evenly spaced price scale. Bid buckets round down and ask buckets
//! round up, so a group never mixes both sides.

use super::{Kind, Side, Ticks};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Row {
    /// Bucket price in ticks: lower edge for bids, upper edge for asks.
    pub px: Ticks,
    pub qty: f64,
    /// Running total from the best row to this one, inclusive.
    pub cum: f64,
}

#[inline]
fn bucket(kind: Kind, px: Ticks, g: Ticks) -> Ticks {
    match kind {
        Kind::Bid => px.div_euclid(g),
        Kind::Ask => (px + g - 1).div_euclid(g),
    }
}

/// Fills `rows` with `g`-tick buckets starting at the best price. Returns the
/// number of rows written: `rows.len()`, or 0 when the side is empty.
pub fn fill(side: &Side, g: Ticks, rows: &mut [Row]) -> usize {
    let g = g.max(1);
    let Some(best) = side.best() else { return 0 };
    let kind = side.kind();
    let b0 = bucket(kind, best.px, g);
    let step = match kind {
        Kind::Bid => -1,
        Kind::Ask => 1,
    };
    for (i, r) in rows.iter_mut().enumerate() {
        *r = Row {
            px: (b0 + step * i as Ticks) * g,
            qty: 0.0,
            cum: 0.0,
        };
    }
    let n = rows.len() as Ticks;
    for l in side.iter() {
        let idx = (bucket(kind, l.px, g) - b0) * step;
        if idx >= n {
            break;
        }
        rows[idx as usize].qty += l.qty;
    }
    let mut cum = 0.0;
    for r in rows.iter_mut() {
        cum += r.qty;
        r.cum = cum;
    }
    rows.len()
}

/// The 1-2-5 ladder of group sizes used by the `+`/`-` keys.
pub fn step_group(g: Ticks, up: bool) -> Ticks {
    let mut mag = 1;
    while mag * 10 <= g {
        mag *= 10;
    }
    let steps = [mag, 2 * mag, 5 * mag, 10 * mag];
    if up {
        steps.into_iter().find(|&s| s > g).unwrap_or(10 * mag)
    } else if g <= 1 {
        1
    } else {
        steps
            .into_iter()
            .rev()
            .find(|&s| s < g)
            .unwrap_or(mag / 2)
            .max(1)
    }
}

/// Picks the smallest 1-2-5 group size at which at least three quarters of
/// the first `rows` buckets on each side hold size, so a sparse book (tiny
/// tick, wide spacing) doesn't open as a wall of empty rows.
pub fn auto_group(bids: &Side, asks: &Side, rows: usize, scratch: &mut [Row]) -> Ticks {
    let rows = rows.min(scratch.len()).max(1);
    let mut g = 1;
    for _ in 0..24 {
        let ok = [bids, asks].into_iter().all(|s| {
            let n = fill(s, g, &mut scratch[..rows]);
            n == 0 || scratch[..n].iter().filter(|r| r.qty > 0.0).count() * 4 >= n * 3
        });
        if ok {
            return g;
        }
        g = step_group(g, true);
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(kind: Kind, lv: &[(Ticks, f64)]) -> Side {
        let mut s = Side::new(kind, 16);
        for &(p, q) in lv {
            s.set(p, q);
        }
        s
    }

    #[test]
    fn bids_round_down_continuous() {
        let s = side(Kind::Bid, &[(1003, 1.0), (1001, 2.0), (998, 4.0)]);
        let mut rows = [Row::default(); 3];
        assert_eq!(fill(&s, 5, &mut rows), 3);
        assert_eq!(
            rows[0],
            Row {
                px: 1000,
                qty: 3.0,
                cum: 3.0
            }
        );
        assert_eq!(
            rows[1],
            Row {
                px: 995,
                qty: 4.0,
                cum: 7.0
            }
        );
        assert_eq!(
            rows[2],
            Row {
                px: 990,
                qty: 0.0,
                cum: 7.0
            }
        );
    }

    #[test]
    fn asks_round_up_with_gaps() {
        let s = side(Kind::Ask, &[(1004, 1.0), (1005, 2.0), (1011, 4.0)]);
        let mut rows = [Row::default(); 3];
        fill(&s, 5, &mut rows);
        assert_eq!(
            rows[0],
            Row {
                px: 1005,
                qty: 3.0,
                cum: 3.0
            }
        );
        assert_eq!(
            rows[1],
            Row {
                px: 1010,
                qty: 0.0,
                cum: 3.0
            }
        );
        assert_eq!(
            rows[2],
            Row {
                px: 1015,
                qty: 4.0,
                cum: 7.0
            }
        );
    }

    #[test]
    fn group_one_is_exact() {
        let s = side(Kind::Ask, &[(10, 1.0), (12, 1.0)]);
        let mut rows = [Row::default(); 3];
        fill(&s, 1, &mut rows);
        assert_eq!(
            rows.map(|r| (r.px, r.qty)),
            [(10, 1.0), (11, 0.0), (12, 1.0)]
        );
    }

    #[test]
    fn empty_side() {
        let s = Side::new(Kind::Bid, 1);
        assert_eq!(fill(&s, 1, &mut [Row::default(); 4]), 0);
    }

    #[test]
    fn steps() {
        assert_eq!(step_group(1, true), 2);
        assert_eq!(step_group(2, true), 5);
        assert_eq!(step_group(5, true), 10);
        assert_eq!(step_group(10, true), 20);
        assert_eq!(step_group(3, true), 5);
        assert_eq!(step_group(10, false), 5);
        assert_eq!(step_group(5, false), 2);
        assert_eq!(step_group(2, false), 1);
        assert_eq!(step_group(1, false), 1);
        assert_eq!(step_group(100, false), 50);
    }

    #[test]
    fn auto_group_widens_sparse_book() {
        let bids = side(
            Kind::Bid,
            &(0..20).map(|i| (10_000 - i * 10, 1.0)).collect::<Vec<_>>(),
        );
        let asks = side(
            Kind::Ask,
            &(0..20).map(|i| (10_010 + i * 10, 1.0)).collect::<Vec<_>>(),
        );
        let mut scratch = [Row::default(); 64];
        // Levels every 10 ticks fill only half of the 5-tick buckets.
        assert_eq!(auto_group(&bids, &asks, 10, &mut scratch), 10);
    }
}
