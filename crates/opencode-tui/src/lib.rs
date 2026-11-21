// Terminal UI: a pure `App` reducer (state + actions -> effects), ratatui views over it, and a runtime that
// feeds terminal events in and runs the session engine in-process.
pub mod app;
pub mod backend;
pub mod catalog;
mod commands;
pub mod config;
pub mod dialog;
mod glyphs;
pub mod sound;
pub mod editor;
pub mod export;
pub mod fuzzy;
pub mod keybind;
pub mod markdown;
pub mod panels;
pub mod question;
pub mod runtime;
pub mod stash;
pub mod state;
pub mod stats;
pub mod terminal;
pub mod text;
pub mod theme;
pub mod tips;
pub mod transcript;
pub mod view;

pub use opencode_client;
pub use runtime::{TuiOptions, run, run_remote, smoke, smoke_with};
