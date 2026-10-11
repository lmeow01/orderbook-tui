use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob::book::group::{self, Row};
use ob::book::{Book, Kind, PriceScale};
use ob::exchange::{Exchange, Tape, Venue};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A 2,000-level book (1,000 per side) around 2500.00, in cents.
fn seeded_book() -> Book {
    let mut b = Book::new(PriceScale::from_tick("0.01").unwrap());
    for i in 1..=1000 {
        b.set(Kind::Bid, 250_000 - i, 1.0 + (i % 7) as f64);
        b.set(Kind::Ask, 250_000 + i, 1.0 + (i % 5) as f64);
    }
    b
}

fn bench_set(c: &mut Criterion) {
    let mut g = c.benchmark_group("side_set");
    let mut rng = Rng(42);
    // Mostly near the top of book, like real traffic.
    let ops: Vec<(Kind, i64, f64)> = (0..10_000)
        .map(|_| {
            let r = rng.next();
            let depth = if r % 10 < 8 { r % 20 } else { r % 1000 } as i64 + 1;
            let qty = if r.is_multiple_of(4) { 0.0 } else { (r % 100) as f64 };
            if r.is_multiple_of(2) {
                (Kind::Bid, 250_000 - depth, qty)
            } else {
                (Kind::Ask, 250_000 + depth, qty)
            }
        })
        .collect();
    g.throughput(Throughput::Elements(ops.len() as u64));
    g.bench_function("10k_updates_near_top", |bch| {
        let mut book = seeded_book();
        bch.iter(|| {
            for &(k, p, q) in &ops {
                book.set(k, p, q);
            }
            black_box(book.best_bid());
        })
    });
    g.finish();
}

fn bench_group(c: &mut Criterion) {
    let book = seeded_book();
    let mut rows = vec![Row::default(); 60];
    let mut g = c.benchmark_group("group_fill");
    for gsize in [1, 10, 100] {
        g.bench_function(format!("60_rows_g{gsize}"), |bch| {
            bch.iter(|| {
                group::fill(&book.bids, gsize, &mut rows);
                group::fill(&book.asks, gsize, &mut rows);
                black_box(rows[0]);
            })
        });
    }
    g.finish();
}

fn bench_venue(c: &mut Criterion) {
    let scale = PriceScale::from_tick("0.01").unwrap();
    let mut rng = Rng(7);
    let mut pairs = |bid: bool| {
        let v: Vec<_> = (0..10)
            .map(|_| {
                let r = rng.next();
                let off = (r % 300) as i64 + 1;
                let px = if bid { 250_000 - off } else { 250_000 + off };
                format!(
                    r#"["{}.{:02}","{}.{:03}"]"#,
                    px / 100,
                    px % 100,
                    r % 10,
                    r % 1000
                )
            })
            .collect();
        format!("[{}]", v.join(","))
    };
    let mut msgs = Vec::new();
    for u in 2..1_002u64 {
        msgs.push(format!(
            r#"{{"topic":"orderbook.200.ETHUSDT","type":"delta","ts":{u},"data":{{"s":"ETHUSDT","b":{},"a":{},"u":{u},"seq":{u}}},"cts":1}}"#,
            pairs(true),
            pairs(false)
        ));
    }
    let snap = format!(
        r#"{{"topic":"orderbook.200.ETHUSDT","type":"snapshot","ts":1,"data":{{"s":"ETHUSDT","b":{},"a":{},"u":1,"seq":1}}}}"#,
        pairs(true),
        pairs(false)
    );
    let mut g = c.benchmark_group("venue");
    g.throughput(Throughput::Elements(msgs.len() as u64));
    g.bench_function("bybit_1k_deltas_20_levels", |bch| {
        let mut venue = Venue::new(Exchange::Bybit, "ETHUSDT", scale);
        let mut book = Book::new(scale);
        let mut tape = Tape::default();
        bch.iter(|| {
            venue.on_text(&snap, &mut book, &mut tape);
            for m in &msgs {
                black_box(venue.on_text(m, &mut book, &mut tape));
            }
        })
    });
    g.finish();
}

criterion_group!(benches, bench_set, bench_group, bench_venue);
criterion_main!(benches);
