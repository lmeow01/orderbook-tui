# ob

A real-time order book DOM ladder for the terminal. It is a single binary that connects straight to each exchange's public WebSocket. There are no API keys, no accounts and no other services.

```
ob binance-futures BTCUSDT
```

## Install

You need a Rust toolchain, version 1.88 or newer. If you don't have one, install it with [rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then install `ob` straight from GitHub:

```sh
cargo install --git https://github.com/lmeow01/orderbook-tui --locked
```

This builds an optimised binary and puts it in `~/.cargo/bin`. rustup adds that directory to your `PATH`. If `ob` isn't found afterwards, open a new terminal or add it yourself:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Run the same command again to update. Remove it with `cargo uninstall orderbook-tui`.

### From a clone

```sh
git clone https://github.com/lmeow01/orderbook-tui
cd orderbook-tui
cargo install --path . --locked     # installs to ~/.cargo/bin
# or build without installing:
cargo build --release               # binary at target/release/ob
```

To install somewhere else on your `PATH`, add `--root`. For example, `--root ~/.local` puts the binary in `~/.local/bin`.

It has been tested on Linux. It should also work on macOS and on Windows terminals, because it only uses cross-platform crates (crossterm, rustls).

## Usage

```
ob [OPTIONS] <EXCHANGE> <SYMBOL>    open the live ladder
ob exchanges                        list supported exchanges
ob symbols <EXCHANGE> [FILTER]      list an exchange's symbols, tick sizes and status
```

Examples:

```sh
ob binance-futures BTCUSDT          # "binance" also works
ob bybit ETHUSDT --group 10         # 10-tick price groups
ob coinbase BTC-USD
ob symbols bybit sol                # symbols containing "sol", prefix matches first
ob bybit BTCUSDT --headless         # print the top of book once a second, no TUI
```

Symbols use each exchange's own format: `BTCUSDT` on Binance and Bybit, and `BTC-USD` on Coinbase. Lower case and separators such as `btc/usdt` are normalised for you. When a symbol isn't found, `ob` suggests close matches.

| Option | Default | Meaning |
|---|---|---|
| `-g`, `--group <TICKS>` | `0` | Price group in ticks. `0` picks one automatically |
| `--fps <N>` | `30` | Maximum redraws per second |
| `--big <K>` | `3` | Highlight a level as big at this multiple of the median visible size |
| `--headless` | off | Print the top of book once a second instead of drawing the TUI |
| `--depth <N>` | `5` | Rows per side in headless mode |

## Exchanges

| Exchange | Market | Book | Trades | Mark |
|---|---|---|---|---|
| `binance-futures` | USD-M futures | REST snapshot + `@depth@100ms` (`/public`) | `@aggTrade` (`/market`) | `@markPrice@1s` |
| `bybit` | v5 linear perps | `orderbook.200` | `publicTrade` | `tickers` |
| `coinbase` | Advanced Trade spot | `level2` (full book) | `market_trades` | none (spot) |

## Reading the ladder

```
 bybit  BTCUSDT   group 0.5                                       ● live  24 msg/s  9 ms
      cum          bid     price      ask                                   cum
                         82995.0    1.866 ███████████▊                    9.698
                         82994.5                                          7.832
                         82994.0    5.257 █████████████████████████████   7.832
              spread 0.1 · 0.01bp ▲ 82993.9   0.005 buy  ·  mark 82995.42
    4.954 ██████████    4.954   82993.5
```

- **Rows:** asks are above the middle row and bids below it. Each row is one price group, and rows are evenly spaced, so a price level with no size still gets a row.
- **Bars:** length is relative to the largest visible level.
- **Large levels:** a level is drawn bold and bright when its size is at least 3× the median visible size (`--big`). The largest level on each side is shown inverted.
- **Recent changes:** a size cell flashes briefly when its size changes.
- **Last trade:** the price row of the last trade is underlined. The middle row shows the last trade price with its tick direction, its size and aggressor side, the spread and the mark price.
- **Header:** connection state, messages per second, feed latency (local clock minus exchange event time) and the reconnect count.

Keys:

| Key | Action |
|---|---|
| `+` / `-` | Larger or smaller price group (1-2-5 steps) |
| `]` / `[` | Group ×10 or ÷10 |
| `a` | Back to the automatic group |
| `↑` / `↓` | Shift the ladder up or down |
| `c` | Re-centre |
| `r` | Reconnect make-before-break (open and sync a new connection, then swap) |
| `R` | Drop the connection now (the last book stays on screen as stale while it resyncs) |
| `q` | Quit |

`--group 0` (the default) picks the smallest group at which about three quarters of the ladder rows have size. `--headless` prints the top of the book once a second instead of drawing the TUI. Logs go to `~/.local/state/ob/ob.log`; set `OB_LOG=debug` for more detail.

## Static build

```
cargo build --release --target x86_64-unknown-linux-musl   # fully static, if the target is installed
```

A normal release build links only libc.

## Design

```
feed thread (tokio, single-threaded)                    main thread (ratatui)

  sockets ─► Venue::on_text ─► Book ─► LadderView ──triple buffer──► draw ≤ --fps
     ▲                                                                   │
  Supervisor: sessions, backoff, watchdog, rotation ◄──── Control ───────┘
                                                (group, rows, reconnect atomics)
```

**Book (`src/book`)**
- Prices are integer ticks. Exchange decimal strings are parsed directly into ticks, never through `f64`, so `"82996.3"` and `"82996.30"` are the same level.
- Each side is a sorted `Vec` with the best level at the end, so the frequent updates near the top move almost nothing.
- Capacity is reserved up front, so nothing allocates after warm-up.
- Grouping walks outward from the best price and writes straight into the fixed-size view rows.

**Parsing (`src/exchange`)**
- Envelopes are walked by a small JSON field scanner that doesn't allocate (`json.rs`). serde_json allocates whenever it skips or captures nested values.
- Flat leaf objects and `[price, qty]` arrays use serde with borrowed `&str` fields.
- Level arrays are applied by a seed visitor, so no intermediate `Vec` is built.
- `tests/zero_alloc.rs` uses a counting global allocator to show that parsing, applying, updating the tape and grouping allocate nothing, for all three exchanges.

**Feed (`src/feed`)**
- Every update is applied as it arrives. Only drawing is limited by the frame rate.
- Each venue has its own sequence check:
  - Binance: `pu` must equal the previous `u`;
  - Bybit: `u` must increase by exactly 1;
  - Coinbase: `sequence_num` must be contiguous.
- Any gap, close or stretch of silence resyncs the book.
- Retries back off exponentially with jitter, from 250 ms up to 10 s.
- **Planned rotations** (Binance's 24 h limit, the `r` key) sync a second connection before swapping it in.
- **Unexpected drops** keep the last book on screen, marked stale.

**Feed → UI.** A lock-free triple buffer passes a fixed-size `LadderView` from the feed to the UI. The UI never blocks the feed.

## Tests and benchmarks

```
cargo test --release   # unit tests, BTreeMap property test, recorded-traffic replays, zero-alloc proof
cargo bench            # side updates, grouping, full Bybit delta parse+apply
```

The replay tests run recorded Binance and Bybit traffic (`tests/fixtures`) through the connectors and compare the resulting book against an independent `serde_json::Value` + `BTreeMap` rebuild. They also remove one message and expect a resync.

## License

MIT. See [LICENSE](LICENSE).
