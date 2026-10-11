//! Property test: `Side` must behave exactly like a `BTreeMap` model under
//! any sequence of sets and deletes.

use std::collections::BTreeMap;

use ob::book::{Kind, Side};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Set(i64, u32),
    Delete(i64),
}

fn op() -> impl Strategy<Value = Op> {
    // A narrow price range forces plenty of updates to existing levels.
    prop_oneof![
        3 => (900i64..1100, 1u32..1000).prop_map(|(p, q)| Op::Set(p, q)),
        1 => (900i64..1100).prop_map(Op::Delete),
    ]
}

fn check(kind: Kind, ops: &[Op]) {
    let mut side = Side::new(kind, 4); // small, so growth is exercised too
    let mut model = BTreeMap::new();
    for op in ops {
        match *op {
            Op::Set(p, q) => {
                side.set(p, q as f64);
                model.insert(p, q as f64);
            }
            Op::Delete(p) => {
                side.set(p, 0.0);
                model.remove(&p);
            }
        }
        let got: Vec<_> = side.iter().map(|l| (l.px, l.qty)).collect();
        let want: Vec<_> = match kind {
            Kind::Bid => model.iter().rev().map(|(&p, &q)| (p, q)).collect(),
            Kind::Ask => model.iter().map(|(&p, &q)| (p, q)).collect(),
        };
        assert_eq!(got, want);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn bids_match_model(ops in prop::collection::vec(op(), 1..300)) {
        check(Kind::Bid, &ops);
    }

    #[test]
    fn asks_match_model(ops in prop::collection::vec(op(), 1..300)) {
        check(Kind::Ask, &ops);
    }
}
