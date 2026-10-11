//! Connection supervisor.
//!
//! Up to two sessions exist at once:
//! - `active` drives the screen;
//! - `pending` is a new connection syncing in the background.
//!
//! A pending session replaces the active one only once its book is synced,
//! so planned rotations and manual reconnects never blank the ladder
//! (make-before-break).
//!
//! When the active session dies unexpectedly (gap, close, silence), its last
//! book stays on screen marked stale while the next session connects with
//! jittered exponential backoff.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{Error as WsError, Message, Utf8Bytes};
use triple_buffer::Input;

use super::ws::{self, Rx, Tx};
use super::{Control, FeedConfig};
use crate::book::{Book, group};
use crate::exchange::{Step, Tape, Venue};
use crate::view::{FeedState, LadderView, MAX_ROWS};

const TICK: Duration = Duration::from_millis(100);
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);
const STABLE_AFTER: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

struct Session {
    id: u64,
    venue: Venue,
    book: Book,
    tx: Tx,
    rx: Rx,
    /// Second socket for venues that split trades from the book.
    aux: Option<(Tx, Rx)>,
    opened: Instant,
    last_rx: Instant,
    last_ping: Instant,
    snapshot: Option<JoinHandle<Result<String>>>,
}

enum SessEvent {
    Text(Utf8Bytes),
    Snapshot(Result<String>),
    Closed(String),
    Other,
}

enum Ev {
    Active(SessEvent),
    Pending(SessEvent),
    Connected(Result<Box<Session>>),
    Control,
    Tick,
}

async fn open(cfg: Arc<FeedConfig>, http: reqwest::Client, id: u64) -> Result<Box<Session>> {
    let venue = Venue::new(cfg.exchange, &cfg.symbol, cfg.scale);
    let urls = venue.urls();
    let subscribe = venue.subscribe();
    let ((tx, rx), aux) = tokio::try_join!(ws::connect(&urls[0], &subscribe), async {
        match urls.get(1) {
            Some(u) => ws::connect(u, &[]).await.map(Some),
            None => Ok(None),
        }
    })?;
    // Fetch the REST snapshot only after the socket is up, so every delta
    // after the snapshot is already queued on the socket.
    let snapshot = venue.snapshot_url().map(|url| {
        tokio::spawn(async move {
            let r = http.get(&url).send().await?.error_for_status()?;
            r.text().await.context("reading snapshot")
        })
    });
    let now = Instant::now();
    tracing::info!(id, urls = ?urls, "connected");
    Ok(Box::new(Session {
        id,
        venue,
        book: Book::new(cfg.scale),
        tx,
        rx,
        aux,
        opened: now,
        last_rx: now,
        last_ping: now,
        snapshot,
    }))
}

async fn next_aux(aux: &mut Option<(Tx, Rx)>) -> Option<Result<Message, WsError>> {
    match aux {
        Some((_, rx)) => rx.next().await,
        None => std::future::pending().await,
    }
}

async fn next_event(s: &mut Session) -> SessEvent {
    let msg = tokio::select! {
        biased;
        r = async { s.snapshot.as_mut().unwrap().await }, if s.snapshot.is_some() => {
            s.snapshot = None;
            return SessEvent::Snapshot(r.unwrap_or_else(|e| Err(e.into())));
        }
        m = s.rx.next() => m,
        m = next_aux(&mut s.aux) => m,
    };
    match msg {
        Some(Ok(Message::Text(t))) => SessEvent::Text(t),
        Some(Ok(Message::Close(f))) => SessEvent::Closed(format!("closed by server {f:?}")),
        Some(Ok(_)) => SessEvent::Other,
        Some(Err(e)) => SessEvent::Closed(e.to_string()),
        None => SessEvent::Closed("connection ended".into()),
    }
}

async fn slot_event(slot: &mut Option<Session>) -> SessEvent {
    match slot {
        Some(s) => next_event(s).await,
        None => std::future::pending().await,
    }
}

async fn join_slot(slot: &mut Option<JoinHandle<Result<Box<Session>>>>) -> Result<Box<Session>> {
    match slot {
        Some(h) => h.await.unwrap_or_else(|e| Err(e.into())),
        None => std::future::pending().await,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub struct Supervisor {
    cfg: Arc<FeedConfig>,
    http: reqwest::Client,
    ctl: Arc<Control>,
    out: Input<LadderView>,

    active: Option<Session>,
    pending: Option<Session>,
    connecting: Option<JoinHandle<Result<Box<Session>>>>,
    /// Last book of a dead active session, shown while reconnecting.
    stale: Option<Book>,
    /// Open a pending session even though `active` is healthy.
    rotate: bool,
    next_id: u64,

    tape: Tape,
    /// Trades seen by a pending session while an active one owns the tape.
    scratch_tape: Tape,

    state: FeedState,
    reason: &'static str,
    attempts: u32,
    retry_at: Instant,
    reconnects: u32,

    msg_count: u32,
    msgs_per_sec: u32,
    rate_t0: Instant,
    latency: Option<i64>,
    version: u64,
    rng: u64,
}

impl Supervisor {
    pub fn new(cfg: FeedConfig, ctl: Arc<Control>, out: Input<LadderView>) -> Result<Self> {
        let http = super::http_client()?;
        let now = Instant::now();
        Ok(Self {
            cfg: Arc::new(cfg),
            http,
            ctl,
            out,
            active: None,
            pending: None,
            connecting: None,
            stale: None,
            rotate: false,
            next_id: 0,
            tape: Tape::default(),
            scratch_tape: Tape::default(),
            state: FeedState::Connecting,
            reason: "",
            attempts: 0,
            retry_at: now,
            reconnects: 0,
            msg_count: 0,
            msgs_per_sec: 0,
            rate_t0: now,
            latency: None,
            version: 0,
            rng: now_ms() as u64 | 1,
        })
    }

    pub async fn run(mut self) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.maybe_connect();
        self.publish();
        loop {
            let ev = tokio::select! {
                e = slot_event(&mut self.active) => Ev::Active(e),
                e = slot_event(&mut self.pending) => Ev::Pending(e),
                r = join_slot(&mut self.connecting) => Ev::Connected(r),
                _ = self.ctl.notify.notified() => Ev::Control,
                _ = tick.tick() => Ev::Tick,
            };
            match ev {
                Ev::Active(e) => self.on_active(e),
                Ev::Pending(e) => self.on_pending(e),
                Ev::Connected(r) => self.on_connected(r),
                Ev::Control => self.on_control(),
                Ev::Tick => self.on_tick().await,
            }
            self.maybe_connect();
        }
    }

    fn on_active(&mut self, e: SessEvent) {
        let Some(s) = self.active.as_mut() else {
            return;
        };
        s.last_rx = Instant::now();
        match e {
            SessEvent::Text(t) => {
                self.msg_count += 1;
                match s.venue.on_text(&t, &mut s.book, &mut self.tape) {
                    Step::Book => {
                        let ev = s.venue.event_ms();
                        if ev > 0 {
                            let lat = now_ms() - ev;
                            self.latency = Some(self.latency.map_or(lat, |l| (l * 4 + lat) / 5));
                        }
                        self.publish();
                    }
                    Step::Tape => self.publish(),
                    Step::Ignore => {}
                    Step::Resync(r) => self.fail_active(r),
                }
            }
            SessEvent::Closed(why) => {
                tracing::warn!(id = s.id, %why, "active session closed");
                self.fail_active("connection closed");
            }
            SessEvent::Snapshot(_) | SessEvent::Other => {}
        }
    }

    fn on_pending(&mut self, e: SessEvent) {
        let Some(s) = self.pending.as_mut() else {
            return;
        };
        s.last_rx = Instant::now();
        let has_active = self.active.is_some();
        let tape = if has_active {
            &mut self.scratch_tape
        } else {
            &mut self.tape
        };
        let step = match e {
            SessEvent::Text(t) => s.venue.on_text(&t, &mut s.book, tape),
            SessEvent::Snapshot(Ok(body)) => s.venue.on_snapshot(&body, &mut s.book),
            SessEvent::Snapshot(Err(err)) => {
                tracing::warn!(id = s.id, %err, "snapshot fetch failed");
                Step::Resync("snapshot fetch failed")
            }
            SessEvent::Closed(why) => {
                tracing::warn!(id = s.id, %why, "pending session closed");
                Step::Resync("connection closed")
            }
            SessEvent::Other => Step::Ignore,
        };
        match step {
            Step::Resync(r) => self.fail_pending(r),
            _ if s.venue.is_synced() => self.promote(),
            Step::Tape if !has_active => self.publish(),
            _ => {}
        }
    }

    fn on_connected(&mut self, r: Result<Box<Session>>) {
        self.connecting = None;
        match r {
            Ok(s) => {
                self.pending = Some(*s);
                if self.active.is_none() && self.stale.is_none() {
                    self.state = FeedState::Syncing;
                }
                self.publish();
            }
            Err(err) => {
                tracing::warn!(%err, "connect failed");
                self.reason = "connect failed";
                self.backoff();
                self.publish();
            }
        }
    }

    fn on_control(&mut self) {
        if self.ctl.drop_now.swap(false, Ordering::Relaxed) {
            tracing::info!("manual hard reconnect");
            self.attempts = 0;
            self.fail_active("dropped by user");
        }
        if self.ctl.reconnect.swap(false, Ordering::Relaxed) && self.active.is_some() {
            tracing::info!("manual reconnect");
            self.rotate = true;
            self.attempts = 0;
            self.retry_at = Instant::now();
        }
        self.publish();
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        let mut fail_a = None;
        let mut fail_p = None;
        if let Some(s) = self.active.as_mut() {
            if now - s.last_rx > s.venue.idle_timeout() {
                fail_a = Some("no data");
            } else if let Err(err) = ping(s, now).await {
                tracing::warn!(%err, "ping failed");
                fail_a = Some("ping failed");
            }
            if s.venue.max_age().is_some_and(|age| now - s.opened > age) && !self.rotate {
                tracing::info!(id = s.id, "rotating aged connection");
                self.rotate = true;
            }
        }
        if let Some(s) = self.pending.as_mut() {
            if now - s.last_rx > s.venue.idle_timeout() {
                fail_p = Some("no data");
            } else if now - s.opened > SYNC_TIMEOUT {
                fail_p = Some("sync timed out");
            } else if let Err(err) = ping(s, now).await {
                tracing::warn!(%err, "ping failed");
                fail_p = Some("ping failed");
            }
        }
        if let Some(r) = fail_a {
            self.fail_active(r);
        }
        if let Some(r) = fail_p {
            self.fail_pending(r);
        }

        let dt = now - self.rate_t0;
        if dt >= Duration::from_secs(1) {
            self.msgs_per_sec = (self.msg_count as f64 / dt.as_secs_f64()).round() as u32;
            self.msg_count = 0;
            self.rate_t0 = now;
            self.publish();
        } else if self.active.is_none() {
            self.publish(); // keep the retry countdown moving
        }
    }

    /// Starts a connection attempt if one is wanted and the backoff allows it.
    fn maybe_connect(&mut self) {
        let wanted = self.active.is_none() || self.rotate;
        if !wanted
            || self.pending.is_some()
            || self.connecting.is_some()
            || Instant::now() < self.retry_at
        {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        if self.active.is_none() && self.stale.is_none() {
            self.state = FeedState::Connecting;
        }
        self.connecting = Some(tokio::spawn(open(self.cfg.clone(), self.http.clone(), id)));
    }

    fn promote(&mut self) {
        let Some(s) = self.pending.take() else { return };
        tracing::info!(
            id = s.id,
            levels = s.book.bids.len() + s.book.asks.len(),
            "session live"
        );
        if let Some(old) = self.active.replace(s) {
            tracing::info!(id = old.id, "retired previous session");
        }
        self.stale = None;
        self.rotate = false;
        self.state = FeedState::Live;
        self.reason = "";
        self.publish();
    }

    fn fail_active(&mut self, reason: &'static str) {
        let Some(s) = self.active.take() else { return };
        tracing::warn!(id = s.id, reason, "active session failed");
        if s.opened.elapsed() > STABLE_AFTER {
            self.attempts = 0;
        }
        self.stale = Some(s.book);
        self.state = FeedState::Stale;
        self.reason = reason;
        self.reconnects += 1;
        self.backoff();
        self.publish();
    }

    fn fail_pending(&mut self, reason: &'static str) {
        let Some(s) = self.pending.take() else { return };
        tracing::warn!(id = s.id, reason, "pending session failed");
        self.reason = reason;
        self.backoff();
        self.publish();
    }

    /// Schedules the next attempt: immediate on the first failure, then
    /// exponential with equal jitter.
    fn backoff(&mut self) {
        let n = self.attempts;
        self.attempts = (n + 1).min(16);
        let delay = if n == 0 {
            Duration::ZERO
        } else {
            let exp = BACKOFF_BASE
                .saturating_mul(1 << (n - 1).min(10))
                .min(BACKOFF_CAP);
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            let jitter = (self.rng % 1000) as f64 / 1000.0;
            exp.mul_f64(0.5 + 0.5 * jitter)
        };
        self.retry_at = Instant::now() + delay;
    }

    fn publish(&mut self) {
        let book = self
            .active
            .as_ref()
            .map(|s| &s.book)
            .or(self.stale.as_ref());
        let rows = self.ctl.rows.load(Ordering::Relaxed).clamp(1, MAX_ROWS);
        self.version += 1;
        let v = self.out.input_buffer_mut();
        v.version = self.version;
        v.state = self.state;
        v.reason = self.reason;
        v.retry_in_ms = self
            .retry_at
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        v.tape = self.tape;
        v.msgs_per_sec = self.msgs_per_sec;
        v.latency_ms = self.latency;
        v.reconnects = self.reconnects;
        match book {
            Some(b) if !(b.bids.is_empty() && b.asks.is_empty()) => {
                let mut g = self.ctl.group.load(Ordering::Relaxed);
                if g <= 0 {
                    g = group::auto_group(&b.bids, &b.asks, rows.min(30), &mut v.bids);
                    self.ctl.group.store(g, Ordering::Relaxed);
                }
                v.group = g;
                group::fill(&b.bids, g, &mut v.bids[..rows]);
                group::fill(&b.asks, g, &mut v.asks[..rows]);
                v.rows = rows;
                v.has_book = true;
                v.best_bid = b.best_bid();
                v.best_ask = b.best_ask();
                v.bid_levels = b.bids.len();
                v.ask_levels = b.asks.len();
            }
            _ => {
                v.has_book = false;
                v.rows = 0;
            }
        }
        self.out.publish();
    }
}

async fn ping(s: &mut Session, now: Instant) -> Result<()> {
    if let Some((every, msg)) = s.venue.ping()
        && now - s.last_ping >= every
    {
        s.last_ping = now;
        s.tx.send(Message::text(msg)).await?;
    }
    Ok(())
}
