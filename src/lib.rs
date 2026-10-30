//! hdir: Mdir-style dual-panel TUI file manager.

pub mod app;
pub mod assoc;
pub mod cli;
pub mod cmdline;
pub mod config;
pub mod dialog;
pub mod editor;
pub mod fs_ops;
pub mod job;
pub mod keymap;
pub mod launcher;
pub mod loader;
pub mod menu;
pub mod panel;
pub mod platform;
pub mod preview;
pub mod remote;
pub mod remote_ops;
pub mod screen;
pub mod servers;
pub mod session;
pub mod term;
#[cfg(test)]
pub mod testutil;
pub mod theme;
pub mod trash;
pub mod ui;
pub mod undo;
pub mod vfs;
pub mod viewer;

pub const APP_NAME: &str = "hdir";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
