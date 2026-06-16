use std::collections::BTreeSet;

use egui_extras::{Column, TableBuilder};
use egui_plot::{Legend, Line, Plot, PlotPoints};

use slipstream_core::model::SignalMeta;
use slipstream_core::query::{DecimateRequest, FrameFilter, StatsRequest};
use slipstream_core::Session;

/// Top-level tabs. Each is a thin view over the *same* [`Session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Config,
    Analysis,
    Graph,
}

/// Thin egui view over a [`Session`]. Holds only UI state; all data comes from
/// core query calls.
pub struct App {
    session: Session,
    signals: Vec<SignalMeta>,
    /// Which tab is currently shown.
    tab: Tab,
    // --- Graph tab state ---------------------------------------------------
    selected: BTreeSet<String>,
    t_start: f64,
    t_end: f64,
    // --- Analysis tab filter inputs ----------------------------------------
    /// Hex CAN id text (e.g. `100`, `0x200`); empty = no id constraint.
    filter_id: String,
    /// Inclusive time-range bounds as text; empty = open bound.
    filter_t_start: String,
    filter_t_end: String,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, session: Session) -> Self {
        let signals = session.available_signals();
        let t_end = session.duration();
        Self {
            session,
            signals,
            tab: Tab::Config,
            selected: BTreeSet::new(),
            t_start: 0.0,
            t_end,
            filter_id: String::new(),
            filter_t_start: String::new(),
            filter_t_end: String::new(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Disjoint field borrows so panel closures don't capture all of `self`.
        let App {
            session,
            signals,
            tab,
            selected,
            t_start,
            t_end,
            filter_id,
            filter_t_start,
            filter_t_end,
        } = self;

        // --- Tab bar -------------------------------------------------------
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(tab, Tab::Config, "Config");
                ui.selectable_value(tab, Tab::Analysis, "Analysis");
                ui.selectable_value(tab, Tab::Graph, "Graph");
            });
        });

        match tab {
            Tab::Config => config_tab(ctx, session, signals),
            Tab::Analysis => {
                analysis_tab(ctx, session, filter_id, filter_t_start, filter_t_end)
            }
            Tab::Graph => graph_tab(ctx, session, signals, selected, *t_start, *t_end),
        }
    }
}

/// Config tab — loaded-state summary and (placeholder) open buttons.
fn config_tab(ctx: &egui::Context, session: &Session, signals: &[SignalMeta]) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Config");
        ui.separator();

        ui.horizontal(|ui| {
            // Placeholders: the real ingest/dialog pipeline is P0/P1 (no rfd yet).
            let _ = ui.button("Open log… (TODO)");
            let _ = ui.button("Open DBC… (TODO)");
        });
        ui.separator();

        egui::Grid::new("config_state")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label("Frames");
                ui.monospace(session.frame_count().to_string());
                ui.end_row();

                ui.label("Duration");
                ui.monospace(format!("{:.3} s", session.duration()));
                ui.end_row();

                ui.label("Signals");
                ui.monospace(signals.len().to_string());
                ui.end_row();

                ui.label("DBC loaded");
                // No DBC ⇒ no decodable signals; the heuristic is good enough here.
                ui.monospace(if signals.is_empty() { "no" } else { "yes" });
                ui.end_row();
            });
    });
}

/// Analysis tab — filterable virtualized frame table plus per-id cycle stats.
fn analysis_tab(
    ctx: &egui::Context,
    session: &Session,
    filter_id: &mut String,
    filter_t_start: &mut String,
    filter_t_end: &mut String,
) {
    // Build a FrameFilter from the (lenient) text inputs. Unparseable fields are
    // simply treated as "no constraint", so a default filter matches all rows.
    let mut filter = FrameFilter::default();
    if let Some(id) = parse_hex_id(filter_id) {
        filter.can_ids.push(id);
    }
    filter.t_start = filter_t_start.trim().parse::<f64>().ok();
    filter.t_end = filter_t_end.trim().parse::<f64>().ok();

    let active = !filter.can_ids.is_empty() || filter.t_start.is_some() || filter.t_end.is_some();

    // --- Cycle stats (bottom strip) ----------------------------------------
    egui::TopBottomPanel::bottom("cycle_stats")
        .resizable(true)
        .default_height(200.0)
        .show(ctx, |ui| {
            ui.heading("Cycle stats");
            let stats = session.all_cycle_stats();
            TableBuilder::new(ui)
                .id_salt("cycle_stats_table")
                .striped(true)
                .resizable(true)
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::remainder())
                .header(20.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("id");
                    });
                    header.col(|ui| {
                        ui.strong("count");
                    });
                    header.col(|ui| {
                        ui.strong("mean_dt");
                    });
                    header.col(|ui| {
                        ui.strong("jitter");
                    });
                })
                .body(|mut body| {
                    for cs in &stats {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.monospace(format!("0x{:X}", cs.can_id));
                            });
                            row.col(|ui| {
                                ui.monospace(cs.count.to_string());
                            });
                            row.col(|ui| {
                                ui.monospace(format!("{:.6}", cs.mean_dt));
                            });
                            row.col(|ui| {
                                ui.monospace(format!("{:.6}", cs.jitter));
                            });
                        });
                    }
                });
        });

    // --- Filter + frame table ----------------------------------------------
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Frames");
        ui.horizontal(|ui| {
            ui.label("CAN id (hex):");
            ui.add(egui::TextEdit::singleline(filter_id).desired_width(80.0));
            ui.separator();
            ui.label("t ≥");
            ui.add(egui::TextEdit::singleline(filter_t_start).desired_width(64.0));
            ui.label("t ≤");
            ui.add(egui::TextEdit::singleline(filter_t_end).desired_width(64.0));
        });
        ui.separator();

        // Drive the count/window off the filter; fall back to all rows when no
        // constraint is set. The total is the row count for the table.
        let total = if active {
            session.filtered_count(&filter) as usize
        } else {
            session.frame_count() as usize
        };
        ui.label(format!("matching frames: {total}"));

        TableBuilder::new(ui)
            .id_salt("frame_table")
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
                    let i = row.index();
                    // Fetch exactly the one visible row through the view-driven
                    // window API, so only screen-sized data crosses the boundary.
                    let r = if active {
                        session.filtered_rows(&filter, i as u64, 1).rows.into_iter().next()
                    } else {
                        session.frame_row(i as u64)
                    };
                    if let Some(r) = r {
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
}

/// Graph tab — signal tree, decimated plot, and a per-signal stats strip.
fn graph_tab(
    ctx: &egui::Context,
    session: &Session,
    signals: &[SignalMeta],
    selected: &mut BTreeSet<String>,
    t_start: f64,
    t_end: f64,
) {
    // --- Signal tree -------------------------------------------------------
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

    // --- Plot --------------------------------------------------------------
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
                    t_start,
                    t_end,
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
                        t_start,
                        t_end,
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

/// Parse a hex CAN id from lenient user text (`100`, `0x100`, `0X100`, with
/// surrounding whitespace). Returns `None` for empty/invalid input.
fn parse_hex_id(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u32::from_str_radix(hex, 16).ok()
}
