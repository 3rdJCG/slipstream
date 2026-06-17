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
    /// Fractional cadence tolerance for DBC-derived health rules (e.g. 0.3 = ±30%).
    health_tolerance: f64,
    /// Channels selected in the Analysis-tab filter; empty = all channels.
    channel_filter: BTreeSet<u8>,
    // --- Config tab state --------------------------------------------------
    /// Last load error (log or DBC), shown in the Config tab; `None` = no error.
    config_error: Option<String>,
    /// Last CSV-export result (success row count or error), shown in the
    /// Analysis/Graph tabs; `None` = nothing exported yet this session.
    export_status: Option<String>,
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
            health_tolerance: 0.3,
            channel_filter: BTreeSet::new(),
            config_error: None,
            export_status: None,
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
            health_tolerance,
            channel_filter,
            config_error,
            export_status,
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
            Tab::Config => config_tab(ctx, session, signals, t_end, config_error),
            Tab::Analysis => analysis_tab(
                ctx,
                session,
                filter_id,
                filter_t_start,
                filter_t_end,
                health_tolerance,
                channel_filter,
                export_status,
            ),
            Tab::Graph => graph_tab(
                ctx,
                session,
                signals,
                selected,
                *t_start,
                *t_end,
                export_status,
            ),
        }
    }
}

/// Config tab — loaded-state summary and file/DBC open buttons.
///
/// Loading mutates the shared [`Session`] in place; refreshing `signals`/`t_end`
/// here is what makes the change visible to the Analysis/Graph tabs immediately
/// (they read from the same `Session` and the refreshed view state).
fn config_tab(
    ctx: &egui::Context,
    session: &mut Session,
    signals: &mut Vec<SignalMeta>,
    t_end: &mut f64,
    config_error: &mut Option<String>,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Config");
        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("Open log…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CAN log", &["blf", "asc"])
                    .pick_file()
                {
                    // Additive: keep already-loaded logs and append this one.
                    match session.add_log(&path) {
                        Ok(_id) => {
                            // Reflect the new log in every tab: refresh the signal
                            // list (DBC unchanged but recomputed for consistency)
                            // and extend the plot window to the full duration.
                            *signals = session.available_signals();
                            *t_end = session.duration();
                            *config_error = None;
                        }
                        Err(e) => *config_error = Some(format!("Open log failed: {e}")),
                    }
                }
            }
            if ui.button("Open DBC…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("DBC", &["dbc"])
                    .pick_file()
                {
                    // Additive: append this DBC (all channels) alongside existing ones.
                    match session.add_dbc(&path, None) {
                        Ok(_id) => {
                            *signals = session.available_signals();
                            *config_error = None;
                        }
                        Err(e) => *config_error = Some(format!("Open DBC failed: {e}")),
                    }
                }
            }
        });

        if let Some(err) = config_error {
            ui.colored_label(egui::Color32::RED, err.as_str());
        }
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
            });

        // --- Loaded logs -------------------------------------------------
        ui.separator();
        ui.strong("Loaded logs");
        // Collect ids to remove after the row loop so we don't borrow `session`
        // immutably (list_logs) and mutably (remove_log) at the same time.
        let logs = session.list_logs();
        if logs.is_empty() {
            ui.label("No logs loaded.");
        }
        let mut log_to_remove: Option<u32> = None;
        for log in &logs {
            ui.horizontal(|ui| {
                if ui.button("✕").on_hover_text("Remove this log").clicked() {
                    log_to_remove = Some(log.id);
                }
                ui.monospace(basename(&log.path)).on_hover_text(&log.path);
                ui.label(format!("{} frames", log.frame_count));
                ui.label(format!("ch {}", channels_label(&log.channels)));
            });
        }
        if let Some(id) = log_to_remove {
            session.remove_log(id);
            *signals = session.available_signals();
            *t_end = session.duration();
        }

        // --- Loaded DBCs -------------------------------------------------
        ui.separator();
        ui.strong("Loaded DBCs");
        let dbcs = session.list_dbcs();
        if dbcs.is_empty() {
            ui.label("No DBCs loaded.");
        }
        let all_channels = session.channels();
        let mut dbc_to_remove: Option<u32> = None;
        // Channel reassignment to apply after the loop (avoids aliasing borrows).
        let mut dbc_set_channel: Option<(u32, Option<u8>)> = None;
        for dbc in &dbcs {
            ui.horizontal(|ui| {
                if ui.button("✕").on_hover_text("Remove this DBC").clicked() {
                    dbc_to_remove = Some(dbc.id);
                }
                ui.monospace(basename(&dbc.path)).on_hover_text(&dbc.path);
                ui.label(format!(
                    "{} msgs / {} sigs",
                    dbc.message_count, dbc.signal_count
                ));

                // Channel selector: "All" + each distinct log channel.
                let current = match dbc.channel {
                    None => "All".to_string(),
                    Some(ch) => format!("ch {ch}"),
                };
                ui.label("channel:");
                egui::ComboBox::from_id_salt(("dbc_channel", dbc.id))
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(dbc.channel.is_none(), "All")
                            .clicked()
                            && dbc.channel.is_some()
                        {
                            dbc_set_channel = Some((dbc.id, None));
                        }
                        for &ch in &all_channels {
                            if ui
                                .selectable_label(dbc.channel == Some(ch), format!("ch {ch}"))
                                .clicked()
                                && dbc.channel != Some(ch)
                            {
                                dbc_set_channel = Some((dbc.id, Some(ch)));
                            }
                        }
                    });
            });
        }
        if let Some((id, ch)) = dbc_set_channel {
            session.set_dbc_channel(id, ch);
            *signals = session.available_signals();
        }
        if let Some(id) = dbc_to_remove {
            session.remove_dbc(id);
            *signals = session.available_signals();
        }
    });
}

/// Last path component (basename); falls back to the whole string.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(path)
}

/// Compact channel list label, e.g. `1,2` (or `-` when empty).
fn channels_label(channels: &[u8]) -> String {
    if channels.is_empty() {
        "-".to_string()
    } else {
        channels
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Analysis tab — filterable virtualized frame table plus per-id cycle stats.
fn analysis_tab(
    ctx: &egui::Context,
    session: &Session,
    filter_id: &mut String,
    filter_t_start: &mut String,
    filter_t_end: &mut String,
    health_tolerance: &mut f64,
    channel_filter: &mut BTreeSet<u8>,
    export_status: &mut Option<String>,
) {
    // Build a FrameFilter from the (lenient) text inputs. Unparseable fields are
    // simply treated as "no constraint", so a default filter matches all rows.
    let mut filter = FrameFilter::default();
    if let Some(id) = parse_hex_id(filter_id) {
        filter.can_ids.push(id);
    }
    filter.t_start = filter_t_start.trim().parse::<f64>().ok();
    filter.t_end = filter_t_end.trim().parse::<f64>().ok();
    // Selected channels (empty = all). Feeds the core FrameFilter's channel field.
    filter.channels = channel_filter.iter().copied().collect();

    let active = !filter.can_ids.is_empty()
        || filter.t_start.is_some()
        || filter.t_end.is_some()
        || !filter.channels.is_empty();

    // --- Messages (bottom strip) -------------------------------------------
    // One per-id row, merging presence stats (count, mean_dt from
    // `message_stats`) with cadence jitter (from `all_cycle_stats`). Both are
    // sorted ascending by can_id; we index the cycle stats by id so ids with a
    // single frame (no cadence) still appear, with a blank jitter.
    egui::TopBottomPanel::bottom("message_stats")
        .resizable(true)
        .default_height(200.0)
        .show(ctx, |ui| {
            ui.heading("Messages");
            let msgs = session.message_stats();
            let cycles = session.all_cycle_stats();
            let jitter_of = |id: u32| -> Option<f64> {
                cycles
                    .iter()
                    .find(|cs| cs.can_id == id)
                    .map(|cs| cs.jitter)
            };
            TableBuilder::new(ui)
                .id_salt("message_stats_table")
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
                    for m in &msgs {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.monospace(format!("0x{:X}", m.can_id));
                            });
                            row.col(|ui| {
                                ui.monospace(m.count.to_string());
                            });
                            row.col(|ui| {
                                ui.monospace(match m.mean_dt {
                                    Some(dt) => format!("{dt:.6}"),
                                    None => "—".to_string(),
                                });
                            });
                            row.col(|ui| {
                                ui.monospace(match jitter_of(m.can_id) {
                                    Some(j) => format!("{j:.6}"),
                                    None => "—".to_string(),
                                });
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

        // Channel ON/OFF filter (none checked = all channels). Only mutates App
        // state (`channel_filter`); the selection is read back into `filter`
        // above on the next frame.
        let channels = session.channels();
        if !channels.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Channels:");
                for ch in channels {
                    let mut on = channel_filter.contains(&ch);
                    if ui.checkbox(&mut on, format!("ch {ch}")).changed() {
                        if on {
                            channel_filter.insert(ch);
                        } else {
                            channel_filter.remove(&ch);
                        }
                    }
                }
            });
        }
        ui.separator();

        // Drive the count/window off the filter; fall back to all rows when no
        // constraint is set. The total is the row count for the table.
        let total = if active {
            session.filtered_count(&filter) as usize
        } else {
            session.frame_count() as usize
        };
        ui.label(format!("matching frames: {total}"));

        // Export the currently-filtered frames to CSV via the core exporter.
        ui.horizontal(|ui| {
            if ui.button("Export frames (CSV)…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .save_file()
                {
                    *export_status = Some(match session.export_frames_csv(&filter, &path) {
                        Ok(n) => format!("Exported {n} frames to {}", basename(&path.display().to_string())),
                        Err(e) => format!("Export failed: {e}"),
                    });
                }
            }
            if let Some(status) = export_status.as_deref() {
                ui.label(status);
            }
        });

        // Render the health section (bounded content) BEFORE the frame table:
        // the frame table is virtualized and fills remaining height, so it must
        // be the last widget in the panel or it overlaps what follows.
        health_section(ui, session, health_tolerance);
        ui.separator();

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

/// Maximum number of violation rows the violations table will display, to keep
/// the per-frame rebuild cheap even when a log has very many violations.
const HEALTH_VIOLATION_LIMIT: usize = 500;

/// Frame-health section of the Analysis tab (collapsing). Thin view: it derives
/// rules from the DBC via [`Session::dbc_health_rules`] and runs
/// [`Session::health_report`] each frame (cheap enough for the demo), then shows
/// a per-rule summary plus a (capped) list of individual violations.
fn health_section(ui: &mut egui::Ui, session: &Session, health_tolerance: &mut f64) {
    egui::CollapsingHeader::new("Health (frame cadence)")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Tolerance (±):");
                ui.add(
                    egui::Slider::new(health_tolerance, 0.0..=1.0)
                        .fixed_decimals(2)
                        .clamping(egui::SliderClamping::Always),
                );
            });

            // Derive cadence rules from the DBC and run the report. Building +
            // running each frame is fine for the demo; revisit if it gets heavy.
            let rules = session.dbc_health_rules(*health_tolerance);
            if rules.rules.is_empty() {
                ui.label("Load a DBC in Config to derive cycle rules.");
                return;
            }
            let report = session.health_report(&rules);

            ui.horizontal(|ui| {
                ui.label(format!("rules: {}", report.rules.len()));
                ui.separator();
                ui.label(format!("total violations: {}", report.total_violations));
                ui.separator();
                ui.label(if report.all_ok { "all ok ✓" } else { "violations ✗" });
            });

            // --- Per-rule summary table ------------------------------------
            ui.add_space(4.0);
            ui.strong("Rules");
            TableBuilder::new(ui)
                .id_salt("health_rule_table")
                .striped(true)
                .resizable(true)
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::remainder())
                .header(20.0, |mut header| {
                    for h in ["id", "name", "expected_dt", "ok", "missing", "excessive", "no_data"]
                    {
                        header.col(|ui| {
                            ui.strong(h);
                        });
                    }
                })
                .body(|mut body| {
                    for rr in &report.rules {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.monospace(format!("0x{:X}", rr.can_id));
                            });
                            row.col(|ui| {
                                ui.monospace(&rr.name);
                            });
                            row.col(|ui| {
                                ui.monospace(format!("{:.6}", rr.expected_dt));
                            });
                            row.col(|ui| {
                                ui.monospace(if rr.ok { "✓" } else { "✗" });
                            });
                            row.col(|ui| {
                                ui.monospace(rr.missing.to_string());
                            });
                            row.col(|ui| {
                                ui.monospace(rr.excessive.to_string());
                            });
                            row.col(|ui| {
                                ui.monospace(if rr.no_data { "✗" } else { "—" });
                            });
                        });
                    }
                });

            // --- Violations table (capped) ---------------------------------
            ui.add_space(8.0);
            ui.strong("Violations");
            // Flatten the per-rule violations into one ordered list, then cap it.
            let all: Vec<&slipstream_core::health::Violation> =
                report.rules.iter().flat_map(|rr| rr.violations.iter()).collect();
            let shown = all.len().min(HEALTH_VIOLATION_LIMIT);
            if all.len() > HEALTH_VIOLATION_LIMIT {
                ui.label(format!(
                    "showing first {shown} of {} violations (truncated)",
                    all.len()
                ));
            } else {
                ui.label(format!("{} violations", all.len()));
            }
            TableBuilder::new(ui)
                .id_salt("health_violation_table")
                .striped(true)
                .resizable(true)
                // Bounded + shrink-to-content so this virtualized table sits
                // inside the collapsing section instead of trying to fill it.
                .auto_shrink([false, true])
                .max_scroll_height(180.0)
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::remainder())
                .header(20.0, |mut header| {
                    for h in ["id", "kind", "t_start", "t_end", "observed_dt", "expected_dt"] {
                        header.col(|ui| {
                            ui.strong(h);
                        });
                    }
                })
                .body(|body| {
                    body.rows(18.0, shown, |mut row| {
                        let v = all[row.index()];
                        row.col(|ui| {
                            ui.monospace(format!("0x{:X}", v.can_id));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:?}", v.kind));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.4}", v.t_start));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.4}", v.t_end));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.6}", v.observed_dt));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.6}", v.expected_dt));
                        });
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
    export_status: &mut Option<String>,
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

        // Export the first selected signal's decoded series to CSV. The button
        // is only reachable here (the panel returns early when nothing is
        // selected), so at least one signal is always available.
        ui.horizontal(|ui| {
            if ui.button("Export signal (CSV)…").clicked() {
                if let Some(name) = selected.iter().next() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV", &["csv"])
                        .save_file()
                    {
                        *export_status = Some(match session.export_signal_csv(name, &path) {
                            Ok(n) => format!(
                                "Exported {n} samples of {name} to {}",
                                basename(&path.display().to_string())
                            ),
                            Err(e) => format!("Export failed: {e}"),
                        });
                    }
                }
            }
            if let Some(status) = export_status.as_deref() {
                ui.label(status);
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
