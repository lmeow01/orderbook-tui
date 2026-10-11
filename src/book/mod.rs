//! Order book engine: fixed-point prices, flat sorted sides, grouping.

pub mod group;
pub mod price;
pub mod side;

pub use price::{PriceScale, Ticks};
pub use side::{Kind, Level, Side};

/// Levels reserved per side up front. Deeper books still work, they just
/// grow the `Vec` once while the snapshot loads.
pub const DEFAULT_CAPACITY: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct Book {
    pub bids: Side,
    pub asks: Side,
    pub scale: PriceScale,
    /// Side that received the most recent update, used to decide which
    /// side wins when the book crosses.
    last: Kind,
}

impl Book {
    pub fn new(scale: PriceScale) -> Self {
        Self::with_capacity(scale, DEFAULT_CAPACITY)
    }

    pub fn with_capacity(scale: PriceScale, cap: usize) -> Self {
        Self {
            bids: Side::new(Kind::Bid, cap),
            asks: Side::new(Kind::Ask, cap),
            scale,
            last: Kind::Bid,
        }
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    #[inline]
    pub fn set(&mut self, kind: Kind, px: Ticks, qty: f64) {
        self.last = kind;
        match kind {
            Kind::Bid => self.bids.set(px, qty),
            Kind::Ask => self.asks.set(px, qty),
        }
    }

    /// Parses a price/quantity pair and applies it. Returns `false` on a
    /// malformed number.
    #[inline]
    pub fn set_str(&mut self, kind: Kind, px: &str, qty: &str) -> bool {
        match (self.scale.parse(px), price::parse_qty(qty)) {
            (Some(p), Some(q)) => {
                self.set(kind, p, q);
                true
            }
            _ => false,
        }
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.bids.best()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.asks.best()
    }

    pub fn is_crossed(&self) -> bool {
        matches!((self.bids.best(), self.asks.best()), (Some(b), Some(a)) if b.px >= a.px)
    }

    /// Resolves a crossed book by trusting the side updated most recently and
    /// dropping the stale levels on the other side. Call after a whole
    /// message is applied, since a message can cross briefly mid-way.
    /// Returns how many levels were dropped.
    pub fn uncross(&mut self) -> usize {
        let (Some(b), Some(a)) = (self.bids.best(), self.asks.best()) else {
            return 0;
        };
        if b.px < a.px {
            return 0;
        }
        match self.last {
            Kind::Bid => self.asks.trim_through(b.px),
            Kind::Ask => self.bids.trim_through(a.px),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncross_trusts_latest_side() {
        let mut b = Book::new(PriceScale::from_tick("1").unwrap());
        b.set(Kind::Bid, 100, 1.0);
        b.set(Kind::Ask, 101, 1.0);
        b.set(Kind::Ask, 102, 1.0);
        b.set(Kind::Bid, 101, 2.0); // new bid lifts the stale 101 ask
        assert!(b.is_crossed());
        assert_eq!(b.uncross(), 1);
        assert_eq!(b.best_ask().unwrap().px, 102);
        assert_eq!(b.best_bid().unwrap().px, 101);
    }
}
