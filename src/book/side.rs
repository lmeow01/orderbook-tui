//! One side of the book as a flat sorted array.
//!
//! Levels are kept sorted so the best price sits at the end of the `Vec`.
//! Almost all book activity happens near the top, so inserts and removes
//! there shift only a handful of elements, and nothing allocates once the
//! reserved capacity covers the book's depth.

use super::price::Ticks;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub px: Ticks,
    pub qty: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bid,
    Ask,
}

#[derive(Debug, Clone)]
pub struct Side {
    kind: Kind,
    /// Sorted ascending by `key()`, so the best level is last.
    levels: Vec<Level>,
}

impl Side {
    pub fn new(kind: Kind, capacity: usize) -> Self {
        Self {
            kind,
            levels: Vec::with_capacity(capacity),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Sort key: higher is better on both sides.
    #[inline]
    fn key(&self, px: Ticks) -> Ticks {
        match self.kind {
            Kind::Bid => px,
            Kind::Ask => -px,
        }
    }

    /// Sets the resting quantity at `px`. A non-positive quantity removes the level.
    #[inline]
    pub fn set(&mut self, px: Ticks, qty: f64) {
        let k = self.key(px);
        // Fast path: the update is at, or better than, the current best.
        if let Some(best) = self.levels.last_mut() {
            let bk = match self.kind {
                Kind::Bid => best.px,
                Kind::Ask => -best.px,
            };
            if k == bk {
                if qty > 0.0 {
                    best.qty = qty;
                } else {
                    self.levels.pop();
                }
                return;
            }
            if k > bk {
                if qty > 0.0 {
                    self.levels.push(Level { px, qty });
                }
                return;
            }
        }
        let kind = self.kind;
        let idx = self.levels.partition_point(|l| match kind {
            Kind::Bid => l.px < k,
            Kind::Ask => -l.px < k,
        });
        match self.levels.get_mut(idx) {
            Some(l) if l.px == px => {
                if qty > 0.0 {
                    l.qty = qty;
                } else {
                    self.levels.remove(idx);
                }
            }
            _ => {
                if qty > 0.0 {
                    self.levels.insert(idx, Level { px, qty });
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.levels.clear();
    }

    #[inline]
    pub fn best(&self) -> Option<Level> {
        self.levels.last().copied()
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// Iterates from the best price outward.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &Level> + '_ {
        self.levels.iter().rev()
    }

    /// Drops levels at or through `px` (bids `>= px`, asks `<= px`).
    /// Returns how many were dropped.
    pub fn trim_through(&mut self, px: Ticks) -> usize {
        let k = self.key(px);
        let mut n = 0;
        while let Some(l) = self.levels.last() {
            if self.key(l.px) < k {
                break;
            }
            self.levels.pop();
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bids_best_is_highest() {
        let mut s = Side::new(Kind::Bid, 8);
        s.set(100, 1.0);
        s.set(102, 2.0);
        s.set(101, 3.0);
        let px: Vec<_> = s.iter().map(|l| l.px).collect();
        assert_eq!(px, [102, 101, 100]);
        s.set(102, 0.0);
        assert_eq!(s.best().unwrap().px, 101);
        s.set(100, 5.0);
        assert_eq!(s.iter().last().unwrap().qty, 5.0);
    }

    #[test]
    fn asks_best_is_lowest() {
        let mut s = Side::new(Kind::Ask, 8);
        s.set(105, 1.0);
        s.set(103, 2.0);
        s.set(104, 3.0);
        let px: Vec<_> = s.iter().map(|l| l.px).collect();
        assert_eq!(px, [103, 104, 105]);
        assert_eq!(s.trim_through(104), 2);
        assert_eq!(s.best().unwrap().px, 105);
    }

    #[test]
    fn deleting_missing_level_is_noop() {
        let mut s = Side::new(Kind::Bid, 8);
        s.set(100, 0.0);
        assert!(s.is_empty());
        s.set(100, 1.0);
        s.set(99, 0.0);
        assert_eq!(s.len(), 1);
    }
}
