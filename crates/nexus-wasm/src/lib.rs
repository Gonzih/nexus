use nexus_app::AmaiApp;
use soul_terminal_app::{AppConfig, SoulApp};
use soul_terminal_core::Theme;
use wasm_bindgen::prelude::*;

/// WASM entry point — called from JavaScript.
#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);

    log::info!("Nexus WASM initialized");

    let app = SoulApp::new(AppConfig {
        title: "Nexus".into(),
        width: 1200,
        height: 800,
    })
    .with_theme(Theme::dark());

    app.run(AmaiApp::new());
}
