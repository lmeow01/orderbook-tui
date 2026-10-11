//! Market data feed: runs on its own thread with a single-threaded tokio
//! runtime, owns every socket and book, and publishes `LadderView`s.

mod supervisor;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use triple_buffer::Input;

use crate::book::PriceScale;
use crate::exchange::Exchange;
use crate::view::LadderView;

pub struct FeedConfig {
    pub exchange: Exchange,
    pub symbol: String,
    pub scale: PriceScale,
}

/// Settings the UI changes at runtime. After changing one, call
/// `notify.notify_one()` so the feed republishes straight away.
pub struct Control {
    /// Group size in ticks; 0 picks one automatically.
    pub group: AtomicI64,
    /// Rows per side the UI can show.
    pub rows: AtomicUsize,
    /// Request a make-before-break reconnect.
    pub reconnect: AtomicBool,
    /// Drop the active session immediately, as if the connection died.
    pub drop_now: AtomicBool,
    pub notify: Notify,
}

impl Control {
    pub fn new(group: i64, rows: usize) -> Self {
        Self {
            group: AtomicI64::new(group),
            rows: AtomicUsize::new(rows),
            reconnect: AtomicBool::new(false),
            drop_now: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("ob/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?)
}

/// Starts the feed thread.
pub fn spawn(
    cfg: FeedConfig,
    ctl: Arc<Control>,
    out: Input<LadderView>,
) -> Result<std::thread::JoinHandle<()>> {
    let sup = supervisor::Supervisor::new(cfg, ctl, out)?;
    Ok(std::thread::Builder::new()
        .name("feed".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building feed runtime");
            rt.block_on(sup.run());
        })?)
}
