//! Proves the steady-state hot path allocates nothing: parsing a message,
//! applying it to the book, updating the tape and grouping the ladder view.
//!
//! A counting global allocator wraps the system one. Each venue gets a
//! warm-up pass (so the book reaches its working size) and then a measured
//! pass of fresh, correctly sequenced messages.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ob::book::{Book, PriceScale, group};
use ob::exchange::{Exchange, Step, Tape, Venue};
use ob::view::LadderView;

struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const MID: i64 = 250_000; // 2500.00 in cents

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// `n` (price, qty) string pairs within 3.00 of the mid; one in five deletes.
    fn levels(&mut self, n: usize, bid: bool) -> Vec<(String, String)> {
        (0..n)
            .map(|_| {
                let r = self.next();
                let off = (r % 300) as i64 + 1;
                let px = if bid { MID - off } else { MID + off };
                let qty = if r.is_multiple_of(5) {
                    0.0
                } else {
                    (r % 10_000) as f64 / 1000.0
                };
                (format!("{}.{:02}", px / 100, px % 100), format!("{qty:.3}"))
            })
            .collect()
    }

    fn pairs(&mut self, n: usize, bid: bool) -> String {
        let v: Vec<_> = self
            .levels(n, bid)
            .iter()
            .map(|(p, q)| format!(r#"["{p}","{q}"]"#))
            .collect();
        format!("[{}]", v.join(","))
    }
}

/// Runs `warm` then `measured` through the venue and the ladder grouping,
/// returning allocations made during `measured`.
fn allocations(venue: &mut Venue, book: &mut Book, warm: &[String], measured: &[String]) -> usize {
    let mut tape = Tape::default();
    let mut view = Box::new(LadderView::default());
    let mut run = |msgs: &[String]| {
        for m in msgs {
            let step = venue.on_text(m, book, &mut tape);
            assert!(!matches!(step, Step::Resync(_)), "{step:?} on {m}");
            group::fill(&book.bids, 5, &mut view.bids[..100]);
            group::fill(&book.asks, 5, &mut view.asks[..100]);
        }
    };
    run(warm);
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    run(measured);
    COUNTING.store(false, Ordering::Relaxed);
    ALLOCS.load(Ordering::Relaxed)
}

fn bybit_pass(rng: &mut Rng) -> Vec<String> {
    // Each pass opens with a snapshot carrying u=1, which resets the sequence.
    let mut v = vec![format!(
        r#"{{"topic":"orderbook.200.ETHUSDT","type":"snapshot","ts":1,"data":{{"s":"ETHUSDT","b":{},"a":{},"u":1,"seq":1}}}}"#,
        rng.pairs(200, true),
        rng.pairs(200, false)
    )];
    for u in 2..2_002u64 {
        v.push(format!(
            r#"{{"topic":"orderbook.200.ETHUSDT","type":"delta","ts":{u},"data":{{"s":"ETHUSDT","b":{},"a":{},"u":{u},"seq":{u}}},"cts":1}}"#,
            rng.pairs(8, true),
            rng.pairs(8, false)
        ));
        if u % 10 == 0 {
            v.push(r#"{"topic":"publicTrade.ETHUSDT","type":"snapshot","ts":1,"data":[{"T":1,"s":"ETHUSDT","S":"Buy","v":"0.5","p":"2500.01","L":"PlusTick","i":"x","BT":false}]}"#.into());
            v.push(r#"{"topic":"tickers.ETHUSDT","type":"delta","ts":1,"data":{"symbol":"ETHUSDT","markPrice":"2500.12"}}"#.into());
        }
    }
    v
}

fn binance_pass(rng: &mut Rng, prev_u: &mut u64) -> Vec<String> {
    let mut v = Vec::new();
    for i in 0..2_000u64 {
        let (prev, u) = (*prev_u, *prev_u + 3);
        v.push(format!(
            r#"{{"stream":"ethusdt@depth@100ms","data":{{"e":"depthUpdate","E":{i},"T":1,"s":"ETHUSDT","U":{prev},"u":{u},"pu":{prev},"b":{},"a":{}}}}}"#,
            rng.pairs(10, true),
            rng.pairs(10, false)
        ));
        *prev_u = u;
        if i % 10 == 0 {
            v.push(r#"{"stream":"ethusdt@aggTrade","data":{"e":"aggTrade","E":1,"a":1,"s":"ETHUSDT","p":"2500.01","q":"0.5","nq":"0.5","f":1,"l":1,"T":1,"m":true}}"#.into());
            v.push(r#"{"stream":"ethusdt@markPrice@1s","data":{"e":"markPriceUpdate","E":1,"s":"ETHUSDT","p":"2500.12000000"}}"#.into());
        }
    }
    v
}

fn coinbase_l2(rng: &mut Rng, seq: &mut u64, kind: &str, n: usize) -> String {
    let mut ups = Vec::new();
    for (side, bid) in [("bid", true), ("offer", false)] {
        for (p, q) in rng.levels(n, bid) {
            ups.push(format!(
                r#"{{"side":"{side}","event_time":"2026-10-10T18:22:16.952232Z","price_level":"{p}","new_quantity":"{q}"}}"#
            ));
        }
    }
    *seq += 1;
    format!(
        r#"{{"channel":"l2_data","timestamp":"2026-10-10T18:22:17.320496592Z","sequence_num":{},"events":[{{"type":"{kind}","product_id":"ETH-USD","updates":[{}]}}]}}"#,
        *seq - 1,
        ups.join(",")
    )
}

fn coinbase_pass(rng: &mut Rng, seq: &mut u64) -> Vec<String> {
    let mut v = Vec::new();
    for i in 0..2_000 {
        v.push(coinbase_l2(rng, seq, "update", 4));
        if i % 10 == 0 {
            *seq += 1;
            v.push(format!(
                r#"{{"channel":"market_trades","timestamp":"x","sequence_num":{},"events":[{{"type":"update","trades":[{{"product_id":"ETH-USD","trade_id":"2","price":"2500.02","size":"0.1","time":"2026-10-10T18:22:15.4Z","side":"SELL"}},{{"product_id":"ETH-USD","trade_id":"1","price":"2500.01","size":"0.5","time":"2026-10-10T18:22:15.3Z","side":"BUY"}}]}}]}}"#,
                *seq - 1
            ));
        }
    }
    v
}

/// One test for every venue, so no other test thread allocates while counting.
#[test]
fn hot_paths_do_not_allocate() {
    let scale = PriceScale::from_tick("0.01").unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let (warm, measured) = (bybit_pass(&mut rng), bybit_pass(&mut rng));
    let mut venue = Venue::new(Exchange::Bybit, "ETHUSDT", scale);
    let mut book = Book::new(scale);
    assert_eq!(
        allocations(&mut venue, &mut book, &warm, &measured),
        0,
        "bybit"
    );

    let mut venue = Venue::new(Exchange::BinanceFutures, "ETHUSDT", scale);
    let mut book = Book::new(scale);
    let snap = format!(
        r#"{{"lastUpdateId":100,"bids":{},"asks":{}}}"#,
        rng.pairs(1000, true),
        rng.pairs(1000, false)
    );
    assert_eq!(venue.on_snapshot(&snap, &mut book), Step::Book);
    let mut u = 100;
    let (warm, measured) = (
        binance_pass(&mut rng, &mut u),
        binance_pass(&mut rng, &mut u),
    );
    assert_eq!(
        allocations(&mut venue, &mut book, &warm, &measured),
        0,
        "binance"
    );

    let mut venue = Venue::new(Exchange::Coinbase, "ETH-USD", scale);
    let mut book = Book::new(scale);
    let mut seq = 0;
    let snap = coinbase_l2(&mut rng, &mut seq, "snapshot", 1000);
    venue.on_text(&snap, &mut book, &mut Tape::default());
    let (warm, measured) = (
        coinbase_pass(&mut rng, &mut seq),
        coinbase_pass(&mut rng, &mut seq),
    );
    assert_eq!(
        allocations(&mut venue, &mut book, &warm, &measured),
        0,
        "coinbase"
    );
}
