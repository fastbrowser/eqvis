// Prevents an extra console window from popping up on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod filter;
mod gui;
#[cfg(target_os = "windows")]
mod win_devices;

use audio::SharedState;
use gui::EqApp;

fn main() -> eframe::Result<()> {
    let state = SharedState::new();

    // Try to open the default input/output devices and start the real-time
    // filter chain. If no audio hardware is present (e.g. a headless CI
    // box, or a sandboxed environment) the app still launches so the UI
    // and response-curve preview can be used/inspected.
    // Audio engine is started from the UI after the user selects devices.
    let (engine, status) = (None, "audio: disabled".to_string());

    // GNOME/Wayland does not render OS-drawn titlebars at all, so on Linux the
    // app paints its own GNOME-style headerbar (with working window controls).
    // On Windows/macOS the real native titlebar is used instead.
    let decorated = cfg!(not(target_os = "linux"));
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([980.0, 620.0])
            .with_min_inner_size([720.0, 480.0])
            // Never draw a client-side custom chrome: either the OS titlebar
            // (Windows/macOS) or eqvis's own GNOME-style headerbar (Linux).
            .with_decorations(decorated),
        ..Default::default()
    };

    eframe::run_native(
        "eqvis",
        options,
        Box::new(move |_cc| Box::new(EqApp::new(state, engine, status))),
    )
}