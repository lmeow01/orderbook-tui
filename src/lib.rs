//! `ob`: a real-time order book DOM ladder for the terminal.
//!
//! - `book`: fixed-point, allocation-free order book engine
//! - `exchange`: venue protocols (Binance futures, Bybit, Coinbase)
//! - `feed`: socket supervisor on its own thread
//! - `view`: the lock-free snapshot passed from feed to UI
//! - `ui`: ratatui ladder

pub mod book;
pub mod cli;
pub mod exchange;
pub mod feed;
pub mod ui;
pub mod view;
