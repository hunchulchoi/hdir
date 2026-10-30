//! hdir: Mdir-style dual-panel TUI file manager.

pub mod app;
pub mod panel;
pub mod term;
pub mod ui;

pub const APP_NAME: &str = "hdir";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
