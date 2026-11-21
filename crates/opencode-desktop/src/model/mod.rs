pub mod attention;
pub mod backend;
pub mod deeplink;
pub mod i18n;
pub mod i18n_supplement;
pub mod i18n_v2;
pub mod keymap;
pub mod menu;
pub mod models;
pub mod persist;
pub mod prompt;
pub mod question;
pub mod settings;
pub mod state;
pub mod theme;
pub mod time;
pub mod transcript;
// Compiled into `build.rs`; here only for its tests.
#[cfg(test)]
pub mod tsdict;
