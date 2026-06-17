use std::collections::BTreeSet;

use egui_extras::{Column, TableBuilder};
use egui_plot::{Legend, Line, Plot, PlotPoints, VLine};

use slipstream_core::health::{HealthReport, HealthRule, Tolerance};
use slipstream_core::model::SignalMeta;
use slipstream_core::predicate::Predicate;
use slipstream_core::query::{
    BusLoadPoint, CycleStats, DecimateRequest, DecimatedSeries, DiffStatus, FrameFilter,
    MessageStats, StatsRequest,
};
use slipstream_core::Session;

/// Memoized results of the (O(n)) Analysis/Graph queries. Each field is paired
/// with the key it was computed from; [`AnalysisCache::refresh`] recomputes a
/// field only when its key changed, so a 50 MB log isn't re-scanned on every
/// repaint. `data_epoch` bumps on any session mutation (see `config_tab`),
/// invalidating everything that depends on the loaded data.
#[derive(Default)]
struct AnalysisCache {
    // --- Per-id aggregates (message/cycle stats + unknown ids) -------------
    msgs: Vec<MessageStats>,
    cycles: Vec<CycleStats>,
    unknown: Vec<u32>,
    agg_epoch: u64,
    // --- Filtered store-row indices ----------------------------------------
    filtered: Vec<u64>,
    filter_active: bool,
    /// Key: (epoch, can_ids, t_start, t_end, channels).
    filter_key: (u64, Vec<u32>, Option<f64>, Option<f64>, Vec<u8>),
    // --- Health report ------------------------------------------------------
    health: Option<HealthReport>,
    /// Key: (epoch, tol_abs, tol_value bits, manual rules).
    health_key: (u64, bool, u64, Vec<(u32, f64)>),
    // --- Bus load -----------------------------------------------------------
    bus: Vec<BusLoadPoint>,
    /// Key: (epoch, bitrate, window in bits — `f64::to_bits`).
    bus_key: (u64, u32, u64),
    // --- Graph decimation ---------------------------------------------------
    graph: Vec<(String, DecimatedSeries)>,
    /// Key: (epoch, selected signal names, t_start bits, t_end bits, px_width,
    /// normalize). The x-range is rounded to ~1e-3 before hashing into the key
    /// so sub-millisecond pan jitter doesn't force a re-decode every frame.
    graph_key: (u64, Vec<String>, u64, u64, u32, bool),
}

impl AnalysisCache {
    /// Refresh the Analysis-tab caches whose keys changed. Called once near the
    /// top of `analysis_tab`; the per-section render code then reads the cached
    /// fields without re-querying the session.
    #[allow(clippy::too_many_arguments)]
    fn refresh(
        &mut self,
        session: &Session,
        data_epoch: u64,
        filter: &FrameFilter,
        filter_active: bool,
        tol_abs: bool,
        tol_value: f64,
        manual_rules: &[(u32, f64)],
        bitrate: u32,
        window: f64,
    ) {
        // Aggregates: recompute on any data change.
        if self.agg_epoch != data_epoch {
            self.msgs = session.message_stats();
            self.cycles = session.all_cycle_stats();
            self.unknown = session.unknown_frame_ids();
            self.agg_epoch = data_epoch;
        }

        // Filtered indices: only while a filter is active, and only when the
        // filter (or data) changed. Inactive → leave empty.
        self.filter_active = filter_active;
        if filter_active {
            let key = (
                data_epoch,
                filter.can_ids.clone(),
                filter.t_start,
                filter.t_end,
                filter.channels.clone(),
            );
            if self.filter_key != key {
                self.filtered = session.filtered_indices(filter);
                self.filter_key = key;
            }
        } else {
            self.filtered.clear();
        }

        // Health report: recompute when tolerance, manual rules, or data change.
        let health_key = (
            data_epoch,
            tol_abs,
            tol_value.to_bits(),
            manual_rules.to_vec(),
        );
        if self.health_key != health_key {
            let tolerance = if tol_abs {
                Tolerance::AbsSeconds(tol_value / 1000.0)
            } else {
                Tolerance::Percent(tol_value)
            };
            let mut rules = session.dbc_health_rules(tolerance);
            for &(can_id, expected_ms) in manual_rules {
                rules.rules.push(HealthRule {
                    can_id,
                    name: format!("manual 0x{can_id:X}"),
                    expected_dt: expected_ms / 1000.0,
                    tolerance,
                    gate: Predicate::Always,
                });
            }
            self.health = if rules.rules.is_empty() {
                None
            } else {
                Some(session.health_report(&rules))
            };
            self.health_key = health_key;
        }

        // Bus load: recompute when bitrate, window, or data change.
        let bus_key = (data_epoch, bitrate, window.to_bits());
        if self.bus_key != bus_key {
            self.bus = session.bus_load(bitrate, window);
            self.bus_key = bus_key;
        }
    }

    /// Refresh the Graph-tab decimation cache, re-decoding only when the
    /// selection, zoom window, pixel width, normalize toggle, or data change.
    #[allow(clippy::too_many_arguments)]
    fn refresh_graph(
        &mut self,
        session: &Session,
        data_epoch: u64,
        selected: &[String],
        t_start: f64,
        t_end: f64,
        px_width: u32,
        normalize: bool,
    ) {
        // Round the x-range to ~1e-3 s so tiny pan/zoom jitter (and the
        // one-frame-lag bounds capture) doesn't invalidate the cache constantly.
        let round = |v: f64| (v * 1000.0).round() as i64 as f64;
        let key = (
            data_epoch,
            selected.to_vec(),
            round(t_start).to_bits(),
            round(t_end).to_bits(),
            px_width,
            normalize,
        );
        if self.graph_key == key {
            return;
        }
        self.graph_key = key;
        self.graph.clear();
        for name in selected {
            let req = DecimateRequest {
                signal: name.clone(),
                t_start,
                t_end,
                px_width,
            };
            if let Ok(series) = session.decimate(&req) {
                self.graph.push((name.clone(), series));
            }
        }
    }
}

/// Top-level tabs. Each is a thin view over the *same* [`Session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Config,
    Analysis,
    Graph,
    Diff,
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
    /// Decoded-signal duration `[0, t_end]`; refreshed on load for the Config
    /// summary and as the default decimation window.
    t_end: f64,
    /// Visible x-range of the plot, captured last frame and used to re-decimate
    /// at the current zoom level (`None` = use the full `[0, duration]` window).
    plot_x_range: Option<(f64, f64)>,
    /// Last plot-cursor position `(t, v)` in data coordinates, for the readout.
    cursor: Option<(f64, f64)>,
    /// When set, each signal is scaled to 0..1 by its own decimated min/max
    /// (a stand-in for true multi-Y-axis plotting, which egui_plot lacks).
    plot_normalize: bool,
    // --- Analysis tab filter inputs ----------------------------------------
    /// Hex CAN id text (e.g. `100`, `0x200`); empty = no id constraint.
    filter_id: String,
    /// Inclusive time-range bounds as text; empty = open bound.
    filter_t_start: String,
    filter_t_end: String,
    /// When `true`, the health tolerance is absolute milliseconds; otherwise it
    /// is a fraction (percent) of the expected cadence.
    health_tol_abs: bool,
    /// Tolerance value: a fraction (e.g. `0.3` = ±30%) when `health_tol_abs` is
    /// false, or milliseconds when it is true.
    health_tol_value: f64,
    /// Manual health rules `(can_id, expected_ms)` added in the Analysis tab for
    /// frames the DBC doesn't declare a cycle time for. They reuse the tolerance
    /// above and an `Always` gate.
    manual_rules: Vec<(u32, f64)>,
    /// Hex CAN id text for the "add manual rule" form.
    new_rule_id: String,
    /// Expected-period-in-ms text for the "add manual rule" form.
    new_rule_ms: String,
    /// Channels selected in the Analysis-tab filter; empty = all channels.
    channel_filter: BTreeSet<u8>,
    /// Bus-load nominal bitrate in bits/s (Analysis tab).
    bus_bitrate: u32,
    /// Bus-load aggregation window in seconds (Analysis tab).
    bus_window: f64,
    // --- Config tab state --------------------------------------------------
    /// Last load error (log or DBC), shown in the Config tab; `None` = no error.
    config_error: Option<String>,
    /// Last CSV-export result (success row count or error), shown in the
    /// Analysis/Graph tabs; `None` = nothing exported yet this session.
    export_status: Option<String>,
    // --- Diff tab state ----------------------------------------------------
    /// Selected log id for side A of the diff (`None` = not yet chosen).
    diff_a: Option<u32>,
    /// Selected log id for side B of the diff (`None` = not yet chosen).
    diff_b: Option<u32>,
    // --- Memoization -------------------------------------------------------
    /// Bumped on every successful session mutation (in `config_tab`). Used as
    /// part of each cache key so the Analysis/Graph queries recompute when the
    /// loaded data changes.
    data_epoch: u64,
    /// Memoized Analysis/Graph query results (see [`AnalysisCache`]).
    acache: AnalysisCache,
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
            t_end,
            plot_x_range: None,
            cursor: None,
            plot_normalize: false,
            filter_id: String::new(),
            filter_t_start: String::new(),
            filter_t_end: String::new(),
            health_tol_abs: false,
            health_tol_value: 0.3,
            manual_rules: Vec::new(),
            new_rule_id: String::new(),
            new_rule_ms: String::new(),
            channel_filter: BTreeSet::new(),
            bus_bitrate: 500_000,
            bus_window: 1.0,
            config_error: None,
            export_status: None,
            diff_a: None,
            diff_b: None,
            data_epoch: 1,
            acache: AnalysisCache::default(),
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
            t_end,
            plot_x_range,
            cursor,
            plot_normalize,
            filter_id,
            filter_t_start,
            filter_t_end,
            health_tol_abs,
            health_tol_value,
            manual_rules,
            new_rule_id,
            new_rule_ms,
            channel_filter,
            bus_bitrate,
            bus_window,
            config_error,
            export_status,
            diff_a,
            diff_b,
            data_epoch,
            acache,
        } = self;

        // --- Tab bar -------------------------------------------------------
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(tab, Tab::Config, "Config");
                ui.selectable_value(tab, Tab::Analysis, "Analysis");
                ui.selectable_value(tab, Tab::Graph, "Graph");
                ui.selectable_value(tab, Tab::Diff, "Diff");
            });
        });

        match tab {
            Tab::Config => config_tab(ctx, session, signals, t_end, config_error, data_epoch),
            Tab::Analysis => analysis_tab(
                ctx,
                session,
                *data_epoch,
                acache,
                filter_id,
                filter_t_start,
                filter_t_end,
                health_tol_abs,
                health_tol_value,
                manual_rules,
                new_rule_id,
                new_rule_ms,
                channel_filter,
                bus_bitrate,
                bus_window,
                export_status,
            ),
            Tab::Graph => graph_tab(
                ctx,
                session,
                *data_epoch,
                acache,
                signals,
                selected,
                plot_x_range,
                cursor,
                plot_normalize,
                export_status,
            ),
            Tab::Diff => diff_tab(ctx, session, diff_a, diff_b),
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
    data_epoch: &mut u64,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Config");
        ui.separator();

        // Empty state: when nothing is loaded (no logs and no DBCs), nudge the
        // user toward the first actions. Demo runs have logs/DBCs so this stays hidden.
        if session.list_logs().is_empty() && session.list_dbcs().is_empty() {
            ui.label("Open a log and a DBC to begin, or Open project…");
            ui.separator();
        }

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
                            *data_epoch += 1;
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
                            *data_epoch += 1;
                        }
                        Err(e) => *config_error = Some(format!("Open DBC failed: {e}")),
                    }
                }
            }
            ui.separator();
            if ui.button("Save project…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("slipstream project", &["json"])
                    .save_file()
                {
                    match session.save_project(&path) {
                        Ok(()) => {
                            *config_error =
                                Some(format!("Saved project to {}", basename(&path.display().to_string())));
                        }
                        Err(e) => *config_error = Some(format!("Save project failed: {e}")),
                    }
                }
            }
            if ui.button("Open project…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("slipstream project", &["json"])
                    .pick_file()
                {
                    // Replace the whole session with the loaded one, then refresh
                    // the derived view state so every tab reflects the new setup.
                    match Session::load_project(&path) {
                        Ok(loaded) => {
                            *session = loaded;
                            *signals = session.available_signals();
                            *t_end = session.duration();
                            *config_error = None;
                            *data_epoch += 1;
                        }
                        Err(e) => *config_error = Some(format!("Open project failed: {e}")),
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
            *data_epoch += 1;
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
            *data_epoch += 1;
        }
        if let Some(id) = dbc_to_remove {
            session.remove_dbc(id);
            *signals = session.available_signals();
            *data_epoch += 1;
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
#[allow(clippy::too_many_arguments)]
fn analysis_tab(
    ctx: &egui::Context,
    session: &Session,
    data_epoch: u64,
    acache: &mut AnalysisCache,
    filter_id: &mut String,
    filter_t_start: &mut String,
    filter_t_end: &mut String,
    health_tol_abs: &mut bool,
    health_tol_value: &mut f64,
    manual_rules: &mut Vec<(u32, f64)>,
    new_rule_id: &mut String,
    new_rule_ms: &mut String,
    channel_filter: &mut BTreeSet<u8>,
    bus_bitrate: &mut u32,
    bus_window: &mut f64,
    export_status: &mut Option<String>,
) {
    // Empty state: with no frames loaded, skip rendering the (virtualized) frame
    // and message tables entirely and show a single prominent hint. The tab stays
    // usable — switching tabs and loading a log in Config still works.
    if session.frame_count() == 0 {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Frames");
            ui.separator();
            ui.centered_and_justified(|ui| {
                ui.label("No frames. Load a BLF/ASC log in the Config tab.");
            });
        });
        return;
    }

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

    // Refresh all Analysis caches ONCE; each recomputes only when its inputs
    // (or `data_epoch`) changed. Everything below renders from `acache` instead
    // of re-scanning the (potentially millions of) frames on every repaint.
    acache.refresh(
        session,
        data_epoch,
        &filter,
        active,
        *health_tol_abs,
        *health_tol_value,
        manual_rules,
        *bus_bitrate,
        *bus_window,
    );

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
            let msgs = &acache.msgs;
            let cycles = &acache.cycles;
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
                    for m in msgs {
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
        // constraint is set. The total is the row count for the table. When a
        // filter is active this reads the memoized index list (no rescan).
        let total = if active {
            acache.filtered.len()
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
        health_section(
            ui,
            acache.health.as_ref(),
            &acache.unknown,
            health_tol_abs,
            health_tol_value,
            manual_rules,
            new_rule_id,
            new_rule_ms,
        );
        ui.separator();

        // Bus load is also bounded content; keep it BEFORE the frame table so the
        // virtualized table stays last and doesn't overlap it.
        bus_load_section(ui, &acache.bus, bus_bitrate, bus_window);
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
                    let k = row.index();
                    // Map visible row → store row via the memoized filtered index
                    // list, then fetch it O(1). No per-row rescan of the store.
                    let r = if active {
                        acache.filtered.get(k).and_then(|&i| session.frame_row(i))
                    } else {
                        session.frame_row(k as u64)
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

/// Frame-health section of the Analysis tab (collapsing). Thin view: it renders
/// the tolerance/manual-rule controls (which only mutate App state — the
/// [`AnalysisCache`] picks the changes up next frame) and displays the memoized
/// [`HealthReport`] passed in, plus a (capped) list of individual violations. It
/// does NOT run the health query itself anymore.
#[allow(clippy::too_many_arguments)]
fn health_section(
    ui: &mut egui::Ui,
    report: Option<&HealthReport>,
    unknown: &[u32],
    health_tol_abs: &mut bool,
    health_tol_value: &mut f64,
    manual_rules: &mut Vec<(u32, f64)>,
    new_rule_id: &mut String,
    new_rule_ms: &mut String,
) {
    egui::CollapsingHeader::new("Health (frame cadence)")
        .default_open(false)
        .show(ui, |ui| {
            // --- Tolerance mode + value -----------------------------------
            ui.horizontal(|ui| {
                ui.label("Tolerance:");
                egui::ComboBox::from_id_salt("health_tol_mode")
                    .selected_text(if *health_tol_abs { "Abs (ms)" } else { "Percent" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(health_tol_abs, false, "Percent");
                        ui.selectable_value(health_tol_abs, true, "Abs (ms)");
                    });
                // Percent edits the fraction directly; Abs edits milliseconds.
                if *health_tol_abs {
                    ui.add(
                        egui::DragValue::new(health_tol_value)
                            .speed(1.0)
                            .range(0.0..=f64::MAX)
                            .suffix(" ms"),
                    );
                } else {
                    ui.add(
                        egui::DragValue::new(health_tol_value)
                            .speed(0.01)
                            .range(0.0..=1.0)
                            .fixed_decimals(2),
                    );
                }
            });

            // --- Manual rules (DBC-independent) ---------------------------
            ui.add_space(4.0);
            ui.strong("Manual rules");
            ui.horizontal(|ui| {
                ui.label("CAN id (hex):");
                ui.add(egui::TextEdit::singleline(new_rule_id).desired_width(80.0));
                ui.label("period (ms):");
                ui.add(egui::TextEdit::singleline(new_rule_ms).desired_width(64.0));
                if ui.button("Add").clicked() {
                    if let (Some(id), Ok(ms)) =
                        (parse_hex_id(new_rule_id), new_rule_ms.trim().parse::<f64>())
                    {
                        if ms > 0.0 {
                            manual_rules.push((id, ms));
                            new_rule_id.clear();
                            new_rule_ms.clear();
                        }
                    }
                }
            });
            // List current manual rules with a per-row remove button.
            let mut rule_to_remove: Option<usize> = None;
            for (i, (id, ms)) in manual_rules.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.button("✕").on_hover_text("Remove this rule").clicked() {
                        rule_to_remove = Some(i);
                    }
                    ui.monospace(format!("0x{id:X}  every {ms} ms"));
                });
            }
            if let Some(i) = rule_to_remove {
                manual_rules.remove(i);
            }

            // --- Unknown frames (no DBC/rule) -----------------------------
            ui.add_space(4.0);
            ui.strong("Unknown frames (no DBC/rule)");
            if unknown.is_empty() {
                ui.label("—");
            } else {
                let list = unknown
                    .iter()
                    .map(|id| format!("0x{id:X}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                ui.monospace(list);
            }
            ui.add_space(4.0);

            // Use the memoized report (built from the DBC + manual rules in
            // `AnalysisCache::refresh`). `None` means no rules are defined yet.
            let report = match report {
                Some(r) => r,
                None => {
                    ui.label("Load a DBC in Config or add a manual rule to derive cycle rules.");
                    return;
                }
            };

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

/// Maximum number of bus-load rows the table will display, to keep the
/// per-frame rebuild cheap even when a log spans very many windows/channels.
const BUS_LOAD_ROW_LIMIT: usize = 500;

/// Bus-load section of the Analysis tab (collapsing). Thin view: it renders the
/// bitrate/window inputs (which only mutate App state — the [`AnalysisCache`]
/// recomputes the bus load next frame) and displays the memoized `points`
/// passed in, one row per (channel, window) with its on-wire load percentage.
fn bus_load_section(
    ui: &mut egui::Ui,
    points: &[BusLoadPoint],
    bus_bitrate: &mut u32,
    bus_window: &mut f64,
) {
    egui::CollapsingHeader::new("Bus load")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Bitrate (bps):");
                ui.add(
                    egui::DragValue::new(bus_bitrate)
                        .speed(1000.0)
                        .range(1..=u32::MAX),
                );
                ui.separator();
                ui.label("Window (s):");
                ui.add(
                    egui::DragValue::new(bus_window)
                        .speed(0.1)
                        .range(0.001..=f64::MAX),
                );
            });

            if points.is_empty() {
                ui.label("No frames to compute bus load (load a log in Config).");
                return;
            }

            let shown = points.len().min(BUS_LOAD_ROW_LIMIT);
            if points.len() > BUS_LOAD_ROW_LIMIT {
                ui.label(format!(
                    "showing first {shown} of {} windows (truncated)",
                    points.len()
                ));
            } else {
                ui.label(format!("{} windows", points.len()));
            }

            TableBuilder::new(ui)
                .id_salt("bus_load_table")
                .striped(true)
                .resizable(true)
                // Bounded + shrink-to-content so this virtualized table sits
                // inside the collapsing section instead of trying to fill it.
                .auto_shrink([false, true])
                .max_scroll_height(180.0)
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::remainder())
                .header(20.0, |mut header| {
                    for h in ["channel", "t_start", "t_end", "load %"] {
                        header.col(|ui| {
                            ui.strong(h);
                        });
                    }
                })
                .body(|body| {
                    body.rows(18.0, shown, |mut row| {
                        let p = &points[row.index()];
                        row.col(|ui| {
                            ui.monospace(p.channel.to_string());
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.4}", p.t_start));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.4}", p.t_end));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:.2}", p.load_pct));
                        });
                    });
                });
        });
}

/// Graph tab — signal tree, decimated plot, and a per-signal stats strip.
#[allow(clippy::too_many_arguments)]
fn graph_tab(
    ctx: &egui::Context,
    session: &Session,
    data_epoch: u64,
    acache: &mut AnalysisCache,
    signals: &[SignalMeta],
    selected: &mut BTreeSet<String>,
    plot_x_range: &mut Option<(f64, f64)>,
    cursor: &mut Option<(f64, f64)>,
    plot_normalize: &mut bool,
    export_status: &mut Option<String>,
) {
    // Effective decimation window: the range we captured from the plot last
    // frame (so zooming in fetches finer detail), falling back to the whole log.
    let (t_start, t_end) = plot_x_range.unwrap_or((0.0, session.duration()));
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
        // Empty state: no decodable signals means no DBC is loaded (or it decodes
        // nothing), so point at Config rather than the generic select-a-signal hint.
        if signals.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("No decodable signals. Load a DBC in the Config tab.");
            });
            return;
        }
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

        // Normalize toggle: a stand-in for multi-Y-axis (egui_plot is single-Y).
        ui.horizontal(|ui| {
            ui.checkbox(plot_normalize, "Normalize (0..1 per signal)");
            if *plot_normalize {
                ui.label("— normalized view: each signal scaled by its own min/max");
            }
        });

        // Cursor readout from last frame's pointer position.
        if let Some((cx, cy)) = *cursor {
            ui.label(format!("cursor: t={cx:.3} v={cy:.3}"));
        } else {
            ui.label("cursor: —");
        }

        // Decimate to the plot's pixel width — only screen-sized data crosses
        // the core boundary, regardless of how big the log is. Memoized: the
        // (re-)decode runs only when the selection/zoom/width/normalize/data
        // change, not on every repaint.
        let px = ui.available_width().max(1.0) as u32;
        let normalize = *plot_normalize;
        let selected_names: Vec<String> = selected.iter().cloned().collect();
        acache.refresh_graph(
            session,
            data_epoch,
            &selected_names,
            t_start,
            t_end,
            px,
            normalize,
        );
        let graph = &acache.graph;
        Plot::new("signal_plot")
            .legend(Legend::default())
            .show(ui, |pui| {
                for (name, series) in graph {
                    let pts: PlotPoints = if normalize {
                        // Scale to 0..1 by this signal's own decimated extent.
                        let lo = series
                            .bins
                            .iter()
                            .map(|b| b.v_min)
                            .fold(f64::INFINITY, f64::min);
                        let hi = series
                            .bins
                            .iter()
                            .map(|b| b.v_max)
                            .fold(f64::NEG_INFINITY, f64::max);
                        let span = hi - lo;
                        // Guard divide-by-zero (flat signal / empty extent).
                        let scale = |v: f64| {
                            if span.is_finite() && span > 0.0 {
                                (v - lo) / span
                            } else {
                                0.0
                            }
                        };
                        series.bins.iter().map(|b| [b.t, scale(b.v_max)]).collect()
                    } else {
                        series.bins.iter().map(|b| [b.t, b.v_max]).collect()
                    };
                    pui.line(Line::new(pts).name(name));
                }

                // Draw a cursor VLine at the pointer and capture (t, v) for the
                // readout shown above on the next frame.
                if let Some(p) = pui.pointer_coordinate() {
                    *cursor = Some((p.x, p.y));
                    pui.vline(VLine::new(p.x));
                }

                // Capture the visible x-range so next frame re-decimates at the
                // current zoom (one-frame lag is fine). Drag-pan / scroll-zoom /
                // box-zoom / double-click-reset stay enabled (egui_plot defaults).
                let b = pui.plot_bounds();
                *plot_x_range = Some((b.min()[0], b.max()[0]));
            });
    });
}

/// Diff tab — compare two loaded logs by CAN id. Thin view: it picks two log
/// ids from [`Session::list_logs`] and renders [`Session::diff_logs`] as a table
/// (per-id presence, counts, count delta, and mean inter-arrival times).
fn diff_tab(
    ctx: &egui::Context,
    session: &Session,
    diff_a: &mut Option<u32>,
    diff_b: &mut Option<u32>,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Diff");
        ui.separator();

        let logs = session.list_logs();
        if logs.len() < 2 {
            ui.label("Load at least two logs in Config to diff.");
            return;
        }

        // Label a log by its path basename, falling back to its id.
        let label_for = |id: u32| -> String {
            logs.iter()
                .find(|l| l.id == id)
                .map(|l| basename(&l.path).to_string())
                .unwrap_or_else(|| format!("log {id}"))
        };
        let selected_text = |sel: &Option<u32>| -> String {
            match sel {
                Some(id) => label_for(*id),
                None => "—".to_string(),
            }
        };

        ui.horizontal(|ui| {
            ui.label("Log A:");
            egui::ComboBox::from_id_salt("diff_log_a")
                .selected_text(selected_text(diff_a))
                .show_ui(ui, |ui| {
                    for l in &logs {
                        ui.selectable_value(diff_a, Some(l.id), basename(&l.path));
                    }
                });
            ui.separator();
            ui.label("Log B:");
            egui::ComboBox::from_id_salt("diff_log_b")
                .selected_text(selected_text(diff_b))
                .show_ui(ui, |ui| {
                    for l in &logs {
                        ui.selectable_value(diff_b, Some(l.id), basename(&l.path));
                    }
                });
        });
        ui.separator();

        let (a, b) = match (*diff_a, *diff_b) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                ui.label("Pick a log for both A and B.");
                return;
            }
        };
        if a == b {
            ui.label("Pick two different logs to compare.");
            return;
        }

        let diffs = match session.diff_logs(a, b) {
            Ok(d) => d,
            Err(e) => {
                ui.colored_label(egui::Color32::RED, format!("Diff failed: {e}"));
                return;
            }
        };

        ui.label(format!("{} CAN ids", diffs.len()));
        TableBuilder::new(ui)
            .id_salt("log_diff_table")
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
                for h in ["id", "status", "count A", "count B", "Δcount", "mean_dt A", "mean_dt B"]
                {
                    header.col(|ui| {
                        ui.strong(h);
                    });
                }
            })
            .body(|mut body| {
                for d in &diffs {
                    body.row(18.0, |mut row| {
                        // Tint rows present on only one side so regressions stand out.
                        let color = match d.status {
                            DiffStatus::OnlyA => Some(egui::Color32::from_rgb(220, 120, 120)),
                            DiffStatus::OnlyB => Some(egui::Color32::from_rgb(120, 180, 120)),
                            DiffStatus::Both => None,
                        };
                        let mono = |ui: &mut egui::Ui, text: String| match color {
                            Some(c) => {
                                ui.monospace(egui::RichText::new(text).color(c));
                            }
                            None => {
                                ui.monospace(text);
                            }
                        };
                        let status = match d.status {
                            DiffStatus::OnlyA => "OnlyA",
                            DiffStatus::OnlyB => "OnlyB",
                            DiffStatus::Both => "Both",
                        };
                        // Count delta (B − A), signed so a drop in B reads negative.
                        let dcount = d.count_b as i64 - d.count_a as i64;
                        let fmt_dt = |dt: Option<f64>| match dt {
                            Some(v) => format!("{v:.6}"),
                            None => "—".to_string(),
                        };
                        row.col(|ui| mono(ui, format!("0x{:X}", d.can_id)));
                        row.col(|ui| mono(ui, status.to_string()));
                        row.col(|ui| mono(ui, d.count_a.to_string()));
                        row.col(|ui| mono(ui, d.count_b.to_string()));
                        row.col(|ui| mono(ui, dcount.to_string()));
                        row.col(|ui| mono(ui, fmt_dt(d.mean_dt_a)));
                        row.col(|ui| mono(ui, fmt_dt(d.mean_dt_b)));
                    });
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
