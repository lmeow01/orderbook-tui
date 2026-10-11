//! Replays recorded exchange traffic through the real connectors and checks
//! the resulting book against an independent reference built with
//! `serde_json::Value` and a `BTreeMap`.

use std::collections::BTreeMap;

use ob::book::{Book, PriceScale};
use ob::exchange::{Exchange, Step, Tape, Venue};
use serde_json::Value;

const BINANCE_SNAPSHOT: &str = include_str!("fixtures/binance_ethusdt_snapshot.json");
const BINANCE_DEPTH: &str = include_str!("fixtures/binance_ethusdt_depth.jsonl");
const BYBIT: &str = include_str!("fixtures/bybit_ethusdt.jsonl");

type Side = BTreeMap<i64, f64>;

/// Price to cents, independent of `PriceScale`.
fn cents(s: &str) -> i64 {
    (s.parse::<f64>().unwrap() * 100.0).round() as i64
}

fn apply_ref(side: &mut Side, levels: &Value) {
    for l in levels.as_array().unwrap() {
        let (p, q) = (
            cents(l[0].as_str().unwrap()),
            l[1].as_str().unwrap().parse::<f64>().unwrap(),
        );
        if q == 0.0 {
            side.remove(&p);
        } else {
            side.insert(p, q);
        }
    }
}

fn assert_same(book: &Book, bids: &Side, asks: &Side) {
    let got_b: Vec<_> = book.bids.iter().map(|l| (l.px, l.qty)).collect();
    let got_a: Vec<_> = book.asks.iter().map(|l| (l.px, l.qty)).collect();
    let want_b: Vec<_> = bids.iter().rev().map(|(&p, &q)| (p, q)).collect();
    let want_a: Vec<_> = asks.iter().map(|(&p, &q)| (p, q)).collect();
    assert_eq!(got_b.len(), want_b.len(), "bid count");
    assert_eq!(got_a.len(), want_a.len(), "ask count");
    assert_eq!(got_b, want_b);
    assert_eq!(got_a, want_a);
}

fn scale() -> PriceScale {
    PriceScale::from_tick("0.01").unwrap()
}

fn binance_lines() -> Vec<&'static str> {
    BINANCE_DEPTH.lines().filter(|l| !l.is_empty()).collect()
}

/// Replays Binance as the app would see it: events up to the snapshot's
/// timestamp are buffered, then the snapshot lands, then the rest stream live.
fn replay_binance(lines: &[&str]) -> (Venue, Book, Vec<Step>) {
    let snap: Value = serde_json::from_str(BINANCE_SNAPSHOT).unwrap();
    let snap_e = snap["E"].as_i64().unwrap();
    let mut venue = Venue::new(Exchange::BinanceFutures, "ETHUSDT", scale());
    let mut book = Book::new(scale());
    let mut tape = Tape::default();
    let mut steps = Vec::new();
    let mut snapped = false;
    for l in lines {
        let e = serde_json::from_str::<Value>(l).unwrap()["data"]["E"]
            .as_i64()
            .unwrap();
        if !snapped && e > snap_e {
            steps.push(venue.on_snapshot(BINANCE_SNAPSHOT, &mut book));
            snapped = true;
        }
        steps.push(venue.on_text(l, &mut book, &mut tape));
        assert!(!book.is_crossed());
    }
    if !snapped {
        steps.push(venue.on_snapshot(BINANCE_SNAPSHOT, &mut book));
    }
    (venue, book, steps)
}

#[test]
fn binance_matches_reference() {
    let lines = binance_lines();
    let (venue, book, steps) = replay_binance(&lines);
    assert!(venue.is_synced());
    assert!(
        !steps.iter().any(|s| matches!(s, Step::Resync(_))),
        "{steps:?}"
    );

    let snap: Value = serde_json::from_str(BINANCE_SNAPSHOT).unwrap();
    let lid = snap["lastUpdateId"].as_u64().unwrap();
    let (mut bids, mut asks) = (Side::new(), Side::new());
    apply_ref(&mut bids, &snap["bids"]);
    apply_ref(&mut asks, &snap["asks"]);
    let mut applied = 0;
    for l in &lines {
        let v: Value = serde_json::from_str(l).unwrap();
        let d = &v["data"];
        if d["u"].as_u64().unwrap() < lid {
            continue;
        }
        apply_ref(&mut bids, &d["b"]);
        apply_ref(&mut asks, &d["a"]);
        applied += 1;
    }
    assert!(
        applied > 10,
        "fixture should have live events after the snapshot"
    );
    assert_same(&book, &bids, &asks);
}

#[test]
fn binance_gap_triggers_resync() {
    let mut lines = binance_lines();
    lines.remove(lines.len() - 10);
    let (_, _, steps) = replay_binance(&lines);
    assert!(steps.contains(&Step::Resync("sequence gap")), "{steps:?}");
}

fn bybit_lines() -> Vec<&'static str> {
    BYBIT.lines().filter(|l| !l.is_empty()).collect()
}

#[test]
fn bybit_matches_reference() {
    let mut venue = Venue::new(Exchange::Bybit, "ETHUSDT", scale());
    let mut book = Book::new(scale());
    let mut tape = Tape::default();
    let (mut bids, mut asks) = (Side::new(), Side::new());
    let mut books = 0;
    for l in bybit_lines() {
        let step = venue.on_text(l, &mut book, &mut tape);
        assert!(!matches!(step, Step::Resync(_)), "{step:?} on {l}");
        assert!(!book.is_crossed());
        let v: Value = serde_json::from_str(l).unwrap();
        if v["topic"]
            .as_str()
            .is_some_and(|t| t.starts_with("orderbook"))
        {
            if v["type"] == "snapshot" {
                bids.clear();
                asks.clear();
            }
            apply_ref(&mut bids, &v["data"]["b"]);
            apply_ref(&mut asks, &v["data"]["a"]);
            books += 1;
        }
    }
    assert!(books > 20);
    assert!(tape.last_px.is_some(), "fixture has trades");
    assert!(!tape.mark.is_empty(), "fixture has a mark price");
    assert_same(&book, &bids, &asks);
}

#[test]
fn bybit_gap_triggers_resync() {
    let mut venue = Venue::new(Exchange::Bybit, "ETHUSDT", scale());
    let mut book = Book::new(scale());
    let mut tape = Tape::default();
    let lines = bybit_lines();
    let book_idx: Vec<_> = (0..lines.len())
        .filter(|&i| lines[i].contains("\"orderbook."))
        .collect();
    let skip = book_idx[book_idx.len() / 2];
    let steps: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != skip)
        .map(|(_, l)| venue.on_text(l, &mut book, &mut tape))
        .collect();
    assert!(steps.contains(&Step::Resync("sequence gap")));
}
