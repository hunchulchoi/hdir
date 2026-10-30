//! hdir: Mdir-style dual-panel TUI file manager.

pub mod app;
pub mod assoc;
pub mod dialog;
pub mod fs_ops;
pub mod job;
pub mod keymap;
pub mod loader;
pub mod panel;
pub mod platform;
pub mod preview;
pub mod remote;
pub mod servers;
pub mod term;
pub mod theme;
pub mod trash;
pub mod ui;
pub mod undo;

pub const APP_NAME: &str = "hdir";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
