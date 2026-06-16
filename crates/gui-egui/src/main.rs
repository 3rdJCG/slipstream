//! slipstream egui front-end.
//!
//! This is a *thin view*: it owns no analysis logic, only calls the
//! `slipstream_core::Session` query API and renders the results. When we
//! graduate to Tauri, this file is what gets reimplemented in TS/HTML — the
//! core and its query API carry over unchanged.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use std::path::Path;

use slipstream_core::Session;

fn main() -> eframe::Result {
    // Args: [log] [dbc]. With no log, falls back to synthetic demo data.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session = match (args.first(), args.get(1)) {
        (Some(log), Some(dbc)) => {
            Session::open_with_dbc(Path::new(log), Path::new(dbc)).unwrap_or_else(|e| {
                eprintln!("failed to open {log} with {dbc}: {e}; using demo data");
                Session::demo()
            })
        }
        (Some(log), None) => Session::open(Path::new(log)).unwrap_or_else(|e| {
            eprintln!("failed to open {log}: {e}; using demo data");
            Session::demo()
        }),
        _ => Session::demo(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "slipstream — CAN log analyzer",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, session)))),
    )
}
