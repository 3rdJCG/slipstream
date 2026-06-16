use std::collections::BTreeSet;

use egui_extras::{Column, TableBuilder};
use egui_plot::{Legend, Line, Plot, PlotPoints};

use slipstream_core::model::SignalMeta;
use slipstream_core::query::{DecimateRequest, StatsRequest};
use slipstream_core::Session;

/// Thin egui view over a [`Session`]. Holds only UI state; all data comes from
/// core query calls.
pub struct App {
    session: Session,
    signals: Vec<SignalMeta>,
    selected: BTreeSet<String>,
    t_start: f64,
    t_end: f64,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, session: Session) -> Self {
        let signals = session.available_signals();
        let t_end = session.duration();
        Self {
            session,
            signals,
            selected: BTreeSet::new(),
            t_start: 0.0,
            t_end,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Disjoint field borrows so panel closures don't capture all of `self`.
        let App {
            session,
            signals,
            selected,
            t_start,
            t_end,
        } = self;

        // --- Toolbar -------------------------------------------------------
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Placeholder: the real ingest pipeline is P0.
                let _ = ui.button("Open log… (TODO)");
                ui.separator();
                ui.label(format!("frames: {}", session.frame_count()));
                ui.separator();
                ui.label(format!("duration: {:.1}s", session.duration()));
                ui.separator();
                ui.label("[demo data]");
            });
        });

        // --- Signal tree ---------------------------------------------------
        egui::SidePanel::left("signals")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("Signals");
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for s in signals.iter() {
                        let mut on = selected.contains(&s.name);
                        let label = if s.unit.is_empty() {
                            format!("{} ({})", s.name, s.message)
                        } else {
                            format!("{} [{}] ({})", s.name, s.unit, s.message)
                        };
                        if ui.checkbox(&mut on, label).changed() {
                            if on {
                                selected.insert(s.name.clone());
                            } else {
                                selected.remove(&s.name);
                            }
                        }
                    }
                });
            });

        // --- Raw frame table (virtualized) ---------------------------------
        egui::TopBottomPanel::bottom("frames")
            .resizable(true)
            .default_height(240.0)
            .show(ctx, |ui| {
                ui.heading("Frames");
                let total = session.frame_count() as usize;
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .column(Column::auto())
                    .column(Column::auto())
                    .column(Column::auto())
                    .column(Column::auto())
                    .column(Column::remainder())
                    .header(20.0, |mut header| {
                        header.col(|ui| {
                            ui.strong("#");
                        });
                        header.col(|ui| {
                            ui.strong("time");
                        });
                        header.col(|ui| {
                            ui.strong("ch");
                        });
                        header.col(|ui| {
                            ui.strong("id");
                        });
                        header.col(|ui| {
                            ui.strong("data");
                        });
                    })
                    .body(|body| {
                        body.rows(18.0, total, |mut row| {
                            let idx = row.index() as u64;
                            if let Some(r) = session.frame_row(idx) {
                                row.col(|ui| {
                                    ui.monospace(r.index.to_string());
                                });
                                row.col(|ui| {
                                    ui.monospace(format!("{:.4}", r.timestamp));
                                });
                                row.col(|ui| {
                                    ui.monospace(r.channel.to_string());
                                });
                                row.col(|ui| {
                                    ui.monospace(format!("0x{:X}", r.can_id));
                                });
                                row.col(|ui| {
                                    ui.monospace(r.data.join(" "));
                                });
                            }
                        });
                    });
            });

        // --- Plot ----------------------------------------------------------
        egui::CentralPanel::default().show(ctx, |ui| {
            if selected.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("Select one or more signals on the left to plot.");
                });
                return;
            }

            // Stats strip for the selected signals over the current window.
            ui.horizontal_wrapped(|ui| {
                for name in selected.iter() {
                    let req = StatsRequest {
                        signal: name.clone(),
                        t_start: *t_start,
                        t_end: *t_end,
                    };
                    if let Ok(st) = session.signal_stats(&req) {
                        ui.label(format!(
                            "{}: min {:.1} / max {:.1} / mean {:.1} (n={})",
                            st.signal, st.min, st.max, st.mean, st.count
                        ));
                        ui.separator();
                    }
                }
            });

            // Decimate to the plot's pixel width — only screen-sized data crosses
            // the core boundary, regardless of how big the log is.
            let px = ui.available_width().max(1.0) as u32;
            Plot::new("signal_plot")
                .legend(Legend::default())
                .show(ui, |pui| {
                    for name in selected.iter() {
                        let req = DecimateRequest {
                            signal: name.clone(),
                            t_start: *t_start,
                            t_end: *t_end,
                            px_width: px,
                        };
                        if let Ok(series) = session.decimate(&req) {
                            let pts: PlotPoints =
                                series.bins.iter().map(|b| [b.t, b.v_max]).collect();
                            pui.line(Line::new(pts).name(name));
                        }
                    }
                });
        });
    }
}
