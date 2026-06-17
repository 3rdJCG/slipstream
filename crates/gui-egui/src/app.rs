use std::collections::BTreeSet;

use egui_extras::{Column, TableBuilder};
use egui_plot::{GridMark, Legend, Line, Plot, PlotPoints, Polygon, VLine};

use slipstream_core::health::{HealthReport, HealthRule, HealthRuleSet, Tolerance};
use slipstream_core::model::SignalMeta;
use slipstream_core::predicate::Predicate;
use slipstream_core::query::{
    BusLoadPoint, CycleStats, DecimateRequest, DecimatedSeries, DiffStatus, FrameFilter,
    MessageStats, StatsRequest,
};
use slipstream_core::Session;

/// Memoized results of the (O(n)) Trace/Graphics/Health queries. Each field is paired
/// with the key it was computed from; [`AnalysisCache::refresh`] recomputes a
/// field only when its key changed, so a 50 MB log isn't re-scanned on every
/// repaint. `data_epoch` bumps on any session mutation (see `setup_tab`),
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
    /// Refresh the Trace/Health-tab caches whose keys changed. Called once near
    /// the top of `trace_tab`/`health_tab`; the per-section render code then reads
    /// the cached fields without re-querying the session.
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
            let rules = build_health_rules(session, tol_abs, tol_value, manual_rules);
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

    /// Refresh the Graphics-tab decimation cache, re-decoding only when the
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

/// Top-level tabs. Each is a thin view over the *same* [`Session`]. Each tab
/// (except Setup) lays out a resizable left `SidePanel` for controls/selection
/// and a `CentralPanel` for the main visualization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Setup,
    Trace,
    Graphics,
    Health,
    Diff,
}

/// Which Health-tab detail view is shown (one at a time, below the always-visible
/// summary, to keep the tab uncluttered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HealthView {
    #[default]
    Rules,
    Violations,
    Timeline,
}

/// Thin egui view over a [`Session`]. Holds only UI state; all data comes from
/// core query calls.
pub struct App {
    session: Session,
    signals: Vec<SignalMeta>,
    /// Which tab is currently shown.
    tab: Tab,
    // --- Graphics tab state ------------------------------------------------
    selected: BTreeSet<String>,
    /// Decoded-signal duration `[0, t_end]`; refreshed on load for the Setup
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
    // --- Trace/Health tab filter inputs ------------------------------------
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
    /// Health tab: show only failing rules/violations (default on — the primary
    /// task is finding problems).
    health_only_failing: bool,
    /// Health tab: hex CAN-id substring filter for the rules/violations lists.
    health_search: String,
    /// Health tab: which detail view (Rules/Violations/Timeline) is shown.
    health_view: HealthView,
    /// Manual health rules `(can_id, expected_ms)` added in the Health tab for
    /// frames the DBC doesn't declare a cycle time for. They reuse the tolerance
    /// above and an `Always` gate.
    manual_rules: Vec<(u32, f64)>,
    /// Hex CAN id text for the "add manual rule" form.
    new_rule_id: String,
    /// Expected-period-in-ms text for the "add manual rule" form.
    new_rule_ms: String,
    /// Channels selected in the Trace/Health-tab filter; empty = all channels.
    channel_filter: BTreeSet<u8>,
    /// Bus-load nominal bitrate in bits/s (Trace tab).
    bus_bitrate: u32,
    /// Bus-load aggregation window in seconds (Trace tab).
    bus_window: f64,
    // --- Setup tab state ---------------------------------------------------
    /// Last load error (log or DBC), shown in the Setup tab; `None` = no error.
    config_error: Option<String>,
    /// Last CSV-export result (success row count or error), shown in the
    /// Trace/Graphics/Health tabs; `None` = nothing exported yet this session.
    export_status: Option<String>,
    // --- Diff tab state ----------------------------------------------------
    /// Selected log id for side A of the diff (`None` = not yet chosen).
    diff_a: Option<u32>,
    /// Selected log id for side B of the diff (`None` = not yet chosen).
    diff_b: Option<u32>,
    // --- Memoization -------------------------------------------------------
    /// Bumped on every successful session mutation (in `setup_tab`). Used as
    /// part of each cache key so the Trace/Graphics/Health queries recompute when
    /// the loaded data changes.
    data_epoch: u64,
    /// Memoized Trace/Graphics/Health query results (see [`AnalysisCache`]).
    acache: AnalysisCache,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, session: Session) -> Self {
        let signals = session.available_signals();
        let t_end = session.duration();
        Self {
            session,
            signals,
            tab: Tab::Setup,
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
            health_only_failing: true,
            health_search: String::new(),
            health_view: HealthView::default(),
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
            health_only_failing,
            health_search,
            health_view,
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
                ui.selectable_value(tab, Tab::Setup, "Setup");
                ui.selectable_value(tab, Tab::Trace, "Trace");
                ui.selectable_value(tab, Tab::Graphics, "Graphics");
                ui.selectable_value(tab, Tab::Health, "Health");
                ui.selectable_value(tab, Tab::Diff, "Diff");
            });
        });

        match tab {
            Tab::Setup => setup_tab(ctx, session, signals, t_end, config_error, data_epoch),
            Tab::Trace => trace_tab(
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
                channel_filter,
                bus_bitrate,
                bus_window,
                export_status,
            ),
            Tab::Graphics => graphics_tab(
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
            Tab::Health => health_tab(
                ctx,
                session,
                *data_epoch,
                acache,
                filter_id,
                filter_t_start,
                filter_t_end,
                channel_filter,
                health_tol_abs,
                health_tol_value,
                health_only_failing,
                health_search,
                health_view,
                manual_rules,
                new_rule_id,
                new_rule_ms,
                export_status,
            ),
            Tab::Diff => diff_tab(ctx, session, diff_a, diff_b),
        }
    }
}

/// Setup tab — loaded-state summary and file/DBC/project open buttons. Settings
/// only, so it stays single-column (no left/center split).
///
/// Loading mutates the shared [`Session`] in place; refreshing `signals`/`t_end`
/// here is what makes the change visible to the Trace/Graphics/Health tabs
/// immediately (they read from the same `Session` and the refreshed view state).
fn setup_tab(
    ctx: &egui::Context,
    session: &mut Session,
    signals: &mut Vec<SignalMeta>,
    t_end: &mut f64,
    config_error: &mut Option<String>,
    data_epoch: &mut u64,
) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Setup");
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

/// Build the [`HealthRuleSet`] for the current tolerance + manual rules: the
/// DBC-derived cycle rules plus one `Always`-gated rule per manual entry. Shared
/// by [`AnalysisCache::refresh`] (per-frame, keyed) and the Health-tab CSV export
/// (one-shot on click), so the two never disagree on which rules are checked.
fn build_health_rules(
    session: &Session,
    tol_abs: bool,
    tol_value: f64,
    manual_rules: &[(u32, f64)],
) -> HealthRuleSet {
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
    rules
}

/// Build a [`FrameFilter`] from the (lenient) text/checkbox inputs shared by the
/// Trace and Health tabs. Unparseable fields are treated as "no constraint", so
/// a default filter matches all rows. Returns the filter and whether any
/// constraint is active.
fn build_filter(
    filter_id: &str,
    filter_t_start: &str,
    filter_t_end: &str,
    channel_filter: &BTreeSet<u8>,
) -> (FrameFilter, bool) {
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
    (filter, active)
}

/// Trace tab — frame-centric view. LEFT side panel holds the filter, the
/// per-id Messages table, and a collapsing Bus load section; CENTER holds the
/// virtualized frame table (kept LAST so it can't overlap preceding widgets).
/// All heavy data is read from the memoized [`AnalysisCache`].
#[allow(clippy::too_many_arguments)]
fn trace_tab(
    ctx: &egui::Context,
    session: &Session,
    data_epoch: u64,
    acache: &mut AnalysisCache,
    filter_id: &mut String,
    filter_t_start: &mut String,
    filter_t_end: &mut String,
    health_tol_abs: &mut bool,
    health_tol_value: &mut f64,
    manual_rules: &[(u32, f64)],
    channel_filter: &mut BTreeSet<u8>,
    bus_bitrate: &mut u32,
    bus_window: &mut f64,
    export_status: &mut Option<String>,
) {
    // Empty state: with no frames loaded, skip rendering the (virtualized) frame
    // and message tables entirely and show a single prominent hint. The tab stays
    // usable — switching tabs and loading a log in Setup still works.
    if session.frame_count() == 0 {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Trace");
            ui.separator();
            ui.centered_and_justified(|ui| {
                ui.label("No frames. Load a BLF/ASC log in the Setup tab.");
            });
        });
        return;
    }

    let (filter, active) =
        build_filter(filter_id, filter_t_start, filter_t_end, channel_filter);

    // Refresh all Trace/Health caches ONCE; each recomputes only when its inputs
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

    // --- LEFT: filter, Messages table, Bus load ----------------------------
    egui::SidePanel::left("trace_controls")
        .resizable(true)
        .default_width(360.0)
        .show(ctx, |ui| {
            ui.heading("Filter");
            ui.horizontal(|ui| {
                ui.label("CAN id (hex):");
                ui.add(egui::TextEdit::singleline(filter_id).desired_width(80.0));
            });
            ui.horizontal(|ui| {
                ui.label("t ≥");
                ui.add(egui::TextEdit::singleline(filter_t_start).desired_width(64.0));
                ui.label("t ≤");
                ui.add(egui::TextEdit::singleline(filter_t_end).desired_width(64.0));
            });

            // Channel ON/OFF filter (none checked = all channels). Only mutates
            // App state (`channel_filter`); the selection is read back into
            // `filter` on the next frame.
            let channels = session.channels();
            if !channels.is_empty() {
                ui.horizontal_wrapped(|ui| {
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

            // Matching count off the filter; full row count when no constraint
            // is set. When a filter is active this reads the memoized index list.
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
            });
            if let Some(status) = export_status.as_deref() {
                ui.label(status);
            }
            ui.separator();

            // --- Messages -------------------------------------------------
            // One per-id row, merging presence stats (count, mean_dt from
            // `message_stats`) with cadence jitter (from `all_cycle_stats`). Both
            // are sorted ascending by can_id; we index the cycle stats by id so
            // ids with a single frame (no cadence) still appear, blank jitter.
            ui.strong("Messages");
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
                .auto_shrink([false, true])
                .max_scroll_height(260.0)
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::auto())
                .column(Column::remainder())
                .header(20.0, |mut header| {
                    for h in ["id", "count", "mean_dt", "jitter"] {
                        header.col(|ui| {
                            ui.strong(h);
                        });
                    }
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

            ui.separator();
            bus_load_section(ui, &acache.bus, bus_bitrate, bus_window);
        });

    // --- CENTER: virtualized frame table (LAST widget) ---------------------
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Frames");
        ui.separator();

        let total = if active {
            acache.filtered.len()
        } else {
            session.frame_count() as usize
        };

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
                for h in ["#", "time", "ch", "id", "data"] {
                    header.col(|ui| {
                        ui.strong(h);
                    });
                }
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

/// Health tab — frame cadence checks. LEFT side panel holds the tolerance
/// controls, DBC rule count, the manual-rule add form + list, and the unknown
/// frames list; CENTER holds the per-rule summary table, the violations table,
/// and the CSV export (with a marked Phase-2 Timeline spot above the tables).
/// The memoized [`HealthReport`] is built in [`AnalysisCache::refresh`].
#[allow(clippy::too_many_arguments)]
fn health_tab(
    ctx: &egui::Context,
    session: &Session,
    data_epoch: u64,
    acache: &mut AnalysisCache,
    filter_id: &str,
    filter_t_start: &str,
    filter_t_end: &str,
    channel_filter: &BTreeSet<u8>,
    health_tol_abs: &mut bool,
    health_tol_value: &mut f64,
    health_only_failing: &mut bool,
    health_search: &mut String,
    health_view: &mut HealthView,
    manual_rules: &mut Vec<(u32, f64)>,
    new_rule_id: &mut String,
    new_rule_ms: &mut String,
    export_status: &mut Option<String>,
) {
    // Refresh the cache (keyed, cheap when unchanged). Reuse the Trace-tab filter
    // so the filtered-index cache stays consistent across tabs.
    let (filter, active) =
        build_filter(filter_id, filter_t_start, filter_t_end, channel_filter);
    acache.refresh(
        session,
        data_epoch,
        &filter,
        active,
        *health_tol_abs,
        *health_tol_value,
        manual_rules,
        // Bus-load inputs are owned by the Trace tab; pass the cached key so the
        // bus computation isn't disturbed (a 0 bitrate would never be a real key).
        acache.bus_key.1,
        f64::from_bits(acache.bus_key.2),
    );

    // --- LEFT: tolerance, rules, manual form, unknown frames ---------------
    egui::SidePanel::left("health_controls")
        .resizable(true)
        .default_width(360.0)
        .show(ctx, |ui| {
            ui.heading("Health rules");

            // Tolerance mode + value.
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

            // DBC-derived rule count: total rules minus the manual ones we appended.
            let total_rules = acache.health.as_ref().map_or(0, |r| r.rules.len());
            let dbc_rules = total_rules.saturating_sub(manual_rules.len());
            ui.label(format!("DBC-derived rules: {dbc_rules}"));
            ui.separator();

            // --- Manual rules (DBC-independent) ---------------------------
            ui.strong("Manual rules");
            ui.horizontal(|ui| {
                ui.label("CAN id (hex):");
                ui.add(egui::TextEdit::singleline(new_rule_id).desired_width(80.0));
            });
            ui.horizontal(|ui| {
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
            ui.separator();

            // --- Unknown frames (no DBC/rule) -----------------------------
            ui.strong("Unknown frames (no DBC/rule)");
            let unknown = &acache.unknown;
            if unknown.is_empty() {
                ui.label("—");
            } else {
                let list = unknown
                    .iter()
                    .map(|id| format!("0x{id:X}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.monospace(list);
                    });
            }
        });

    // --- CENTER: always-visible summary + one detail view at a time --------
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Health");
        let report = acache.health.as_ref();

        // Prominent pass/fail summary — the first thing the eye lands on.
        health_summary(ui, report);

        // Controls: filter to problems + search by id + CSV export.
        ui.horizontal(|ui| {
            ui.checkbox(health_only_failing, "Show only failing");
            ui.separator();
            ui.label("id:");
            ui.add(
                egui::TextEdit::singleline(health_search)
                    .desired_width(72.0)
                    .hint_text("hex"),
            );
            ui.separator();
            // Rebuild the ruleset for export (cheap: O(DBC messages)); matches
            // what the memoized report was built from.
            let rules =
                build_health_rules(session, *health_tol_abs, *health_tol_value, manual_rules);
            if ui.button("Export CSV…").clicked() {
                if let Some(path) =
                    rfd::FileDialog::new().add_filter("CSV", &["csv"]).save_file()
                {
                    *export_status = Some(match session.export_health_csv(&rules, &path) {
                        Ok(n) => format!("Exported {n} rows"),
                        Err(e) => format!("Export failed: {e}"),
                    });
                }
            }
            if let Some(s) = export_status.as_deref() {
                ui.label(s);
            }
        });

        // Segmented view selector — show one detail view at a time.
        ui.horizontal(|ui| {
            ui.selectable_value(health_view, HealthView::Rules, "Rules");
            ui.selectable_value(health_view, HealthView::Violations, "Violations");
            ui.selectable_value(health_view, HealthView::Timeline, "Timeline");
        });
        ui.separator();

        match report {
            None => {
                ui.label("Load a DBC in Setup, or add a manual rule on the left.");
            }
            Some(r) => match *health_view {
                HealthView::Rules => {
                    health_rules_view(ui, r, *health_only_failing, health_search)
                }
                HealthView::Violations => {
                    health_violations_view(ui, r, *health_only_failing, health_search)
                }
                HealthView::Timeline => health_timeline(ui, Some(r), &acache.msgs),
            },
        }
    });
}

/// A prominent, color-coded pass/fail line for the Health tab.
fn health_summary(ui: &mut egui::Ui, report: Option<&HealthReport>) {
    let text = match report {
        None => egui::RichText::new("No rules defined").italics(),
        Some(r) if r.all_ok => egui::RichText::new(format!("✓ All OK — {} rules", r.rules.len()))
            .color(egui::Color32::from_rgb(60, 180, 90))
            .strong(),
        Some(r) => {
            let failing = r.rules.iter().filter(|rr| !rr.ok).count();
            egui::RichText::new(format!(
                "✗ {failing}/{} rules failing · {} violations",
                r.rules.len(),
                r.total_violations
            ))
            .color(egui::Color32::from_rgb(220, 80, 80))
            .strong()
        }
    };
    ui.add(egui::Label::new(text.size(16.0)));
}

/// Does `0x{id:X}` contain the (case-insensitive) search substring?
fn id_matches(id: u32, search: &str) -> bool {
    let s = search.trim();
    s.is_empty() || format!("{id:X}").contains(&s.trim_start_matches("0x").to_uppercase())
}

/// Rules view: one row per rule, failing first then by violation count, filtered
/// by the "only failing" toggle and the id search.
fn health_rules_view(
    ui: &mut egui::Ui,
    report: &HealthReport,
    only_failing: bool,
    search: &str,
) {
    let mut rows: Vec<&slipstream_core::health::RuleReport> = report
        .rules
        .iter()
        .filter(|rr| (!only_failing || !rr.ok) && id_matches(rr.can_id, search))
        .collect();
    // Priority: failing first, then most violations first.
    rows.sort_by(|a, b| {
        a.ok.cmp(&b.ok)
            .then((b.missing + b.excessive).cmp(&(a.missing + a.excessive)))
            .then(a.can_id.cmp(&b.can_id))
    });
    ui.label(format!("{} rules", rows.len()));
    TableBuilder::new(ui)
        .id_salt("health_rule_table")
        .striped(true)
        .resizable(true)
        .column(Column::initial(64.0).at_least(48.0)) // id
        .column(Column::initial(150.0).at_least(60.0).clip(true)) // name
        .column(Column::initial(40.0)) // ok
        .column(Column::initial(64.0)) // missing
        .column(Column::initial(72.0)) // excessive
        .column(Column::remainder()) // expected_dt
        .header(20.0, |mut header| {
            for h in ["id", "name", "ok", "missing", "excessive", "expected"] {
                header.col(|ui| {
                    ui.strong(h);
                });
            }
        })
        .body(|body| {
            body.rows(18.0, rows.len(), |mut row| {
                let rr = rows[row.index()];
                row.col(|ui| {
                    ui.monospace(format!("0x{:X}", rr.can_id));
                });
                row.col(|ui| {
                    ui.monospace(&rr.name);
                });
                row.col(|ui| {
                    if rr.ok {
                        ui.colored_label(egui::Color32::from_rgb(60, 180, 90), "✓");
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "✗");
                    }
                });
                row.col(|ui| {
                    ui.monospace(rr.missing.to_string());
                });
                row.col(|ui| {
                    ui.monospace(rr.excessive.to_string());
                });
                row.col(|ui| {
                    ui.monospace(format!("{:.4} s", rr.expected_dt));
                });
            });
        });
}

/// Violations view: flattened, filtered, capped list of individual violations.
fn health_violations_view(
    ui: &mut egui::Ui,
    report: &HealthReport,
    only_failing: bool,
    search: &str,
) {
    let all: Vec<&slipstream_core::health::Violation> = report
        .rules
        .iter()
        .filter(|rr| !only_failing || !rr.ok)
        .flat_map(|rr| rr.violations.iter())
        .filter(|v| id_matches(v.can_id, search))
        .collect();
    let shown = all.len().min(HEALTH_VIOLATION_LIMIT);
    if all.len() > shown {
        ui.label(format!("first {shown} of {} violations", all.len()));
    } else {
        ui.label(format!("{} violations", all.len()));
    }
    TableBuilder::new(ui)
        .id_salt("health_violation_table")
        .striped(true)
        .resizable(true)
        .column(Column::initial(64.0).at_least(48.0)) // id
        .column(Column::initial(80.0)) // kind
        .column(Column::initial(88.0)) // t_start
        .column(Column::initial(88.0)) // t_end
        .column(Column::initial(96.0)) // observed
        .column(Column::remainder()) // expected
        .header(20.0, |mut header| {
            for h in ["id", "kind", "t_start", "t_end", "observed", "expected"] {
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
                    ui.monospace(format!("{:.4}", v.observed_dt));
                });
                row.col(|ui| {
                    ui.monospace(format!("{:.4}", v.expected_dt));
                });
            });
        });
}

/// Per-id health timeline (CENTER of the Health tab, above the tables). Thin
/// view: it draws one horizontal lane per rule in the memoized [`HealthReport`],
/// using only already-computed data — no new per-frame scan.
///
/// Per lane (y = rule index): a translucent GREEN rectangle spans the message's
/// present interval `[first_t, last_t]` (looked up by `can_id` in the memoized
/// `MessageStats`; lanes whose id never appears are drawn empty), and a
/// translucent RED rectangle is overlaid for each cadence violation's
/// `[t_start, t_end]`. The Y axis maps each integer lane index back to its
/// `0x{can_id:X}` via [`Plot::y_axis_formatter`]; X is time in seconds.
fn health_timeline(ui: &mut egui::Ui, report: Option<&HealthReport>, msgs: &[MessageStats]) {
    ui.strong("Timeline");
    ui.label("green = present span, red = cadence violation");

    let report = match report {
        Some(r) => r,
        None => {
            ui.label("No rules yet — load a DBC in Setup or add a manual rule.");
            return;
        }
    };
    if report.rules.is_empty() {
        ui.label("No rules to plot.");
        return;
    }

    // Translucent fills (green = present span, red = violation).
    let green = egui::Color32::from_rgba_unmultiplied(60, 180, 75, 70);
    let red = egui::Color32::from_rgba_unmultiplied(220, 60, 60, 110);

    // Axis-aligned rectangle as a 4-point Polygon at lane `idx` ± `half`.
    let rect = |t0: f64, t1: f64, idx: usize, half: f64| -> Vec<[f64; 2]> {
        let y = idx as f64;
        vec![
            [t0, y - half],
            [t1, y - half],
            [t1, y + half],
            [t0, y + half],
        ]
    };

    // Snapshot the per-lane can_id so the y-axis formatter (which outlives this
    // call via the plot closure) can map an integer lane index back to its id.
    let lane_ids: Vec<u32> = report.rules.iter().map(|rr| rr.can_id).collect();
    let n = lane_ids.len();

    // Fixed-ish height: ~24 px per lane, bounded so the plot sits above the
    // tables and never eats the whole panel.
    let height = ((n as f32) * 24.0 + 24.0).clamp(72.0, 240.0);

    Plot::new("health_timeline")
        .height(height)
        .legend(Legend::default())
        .y_axis_formatter(move |mark: GridMark, _range: &std::ops::RangeInclusive<f64>| {
            // Only label integer lanes that exist; blank everything else so
            // intermediate grid marks don't print bogus ids.
            let v = mark.value;
            if v.fract().abs() < 1e-6 && v >= 0.0 {
                let idx = v as usize;
                if idx < lane_ids.len() {
                    return format!("0x{:X}", lane_ids[idx]);
                }
            }
            String::new()
        })
        .show(ui, |pui| {
            for (idx, rr) in report.rules.iter().enumerate() {
                // Present span: look up this rule's id in the memoized stats.
                if let Some(m) = msgs.iter().find(|m| m.can_id == rr.can_id) {
                    pui.polygon(
                        Polygon::new(PlotPoints::from(rect(m.first_t, m.last_t, idx, 0.3)))
                            .fill_color(green)
                            .stroke(egui::Stroke::NONE)
                            .allow_hover(false),
                    );
                }
                // Violation spans on top, slightly taller so they read clearly.
                for v in &rr.violations {
                    pui.polygon(
                        Polygon::new(PlotPoints::from(rect(v.t_start, v.t_end, idx, 0.35)))
                            .fill_color(red)
                            .stroke(egui::Stroke::NONE)
                            .allow_hover(false),
                    );
                }
            }
        });
}

/// Maximum number of violation rows the violations table will display, to keep
/// the per-frame rebuild cheap even when a log has very many violations.
const HEALTH_VIOLATION_LIMIT: usize = 500;


/// Maximum number of bus-load rows the table will display, to keep the
/// per-frame rebuild cheap even when a log spans very many windows/channels.
const BUS_LOAD_ROW_LIMIT: usize = 500;

/// Bus-load section of the Trace tab (collapsing). Thin view: it renders the
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
                ui.label("No frames to compute bus load (load a log in Setup).");
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

/// Graphics tab — signal tree, decimated plot, and a per-signal stats strip.
/// LEFT side panel groups signals by message name as collapsing headers and
/// holds the normalize toggle + CSV export; CENTER is the egui_plot.
#[allow(clippy::too_many_arguments)]
fn graphics_tab(
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
    // --- Signal tree (grouped by message) ----------------------------------
    egui::SidePanel::left("signals")
        .resizable(true)
        .default_width(260.0)
        .show(ctx, |ui| {
            ui.heading("Signals");

            // Normalize toggle: a stand-in for multi-Y-axis (egui_plot is single-Y).
            ui.checkbox(plot_normalize, "Normalize (0..1 per signal)");

            // Export the first selected signal's decoded series to CSV. Disabled
            // until at least one signal is selected (the core call needs a name).
            ui.add_enabled_ui(!selected.is_empty(), |ui| {
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
            });
            if let Some(status) = export_status.as_deref() {
                ui.label(status);
            }
            ui.separator();

            // Group signal checkboxes under a collapsing header per message name.
            // `available_signals` returns rows ordered by message then signal, so
            // consecutive entries with the same message belong together; we open a
            // new header whenever the message name changes.
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut i = 0;
                while i < signals.len() {
                    let message = signals[i].message.clone();
                    let end = signals[i..]
                        .iter()
                        .position(|s| s.message != message)
                        .map_or(signals.len(), |off| i + off);
                    egui::CollapsingHeader::new(if message.is_empty() {
                        "(no message)"
                    } else {
                        &message
                    })
                    .default_open(true)
                    .id_salt(("sig_group", i))
                    .show(ui, |ui| {
                        for s in &signals[i..end] {
                            let mut on = selected.contains(&s.name);
                            let label = if s.unit.is_empty() {
                                s.name.clone()
                            } else {
                                format!("{} [{}]", s.name, s.unit)
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
                    i = end;
                }
            });
        });

    // --- Plot --------------------------------------------------------------
    egui::CentralPanel::default().show(ctx, |ui| {
        // Empty state: no decodable signals means no DBC is loaded (or it decodes
        // nothing), so point at Setup rather than the generic select-a-signal hint.
        if signals.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("No decodable signals. Load a DBC in the Setup tab.");
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

/// Diff tab — compare two loaded logs by CAN id. Thin view: the LEFT side panel
/// picks two log ids from [`Session::list_logs`]; the CENTER renders
/// [`Session::diff_logs`] as a table (per-id presence, counts, count delta, and
/// mean inter-arrival times).
fn diff_tab(
    ctx: &egui::Context,
    session: &Session,
    diff_a: &mut Option<u32>,
    diff_b: &mut Option<u32>,
) {
    let logs = session.list_logs();

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

    // --- LEFT: log pickers -------------------------------------------------
    egui::SidePanel::left("diff_controls")
        .resizable(true)
        .default_width(260.0)
        .show(ctx, |ui| {
            ui.heading("Diff");
            ui.separator();
            if logs.len() < 2 {
                ui.label("Load at least two logs in Setup to diff.");
                return;
            }
            ui.label("Log A:");
            egui::ComboBox::from_id_salt("diff_log_a")
                .selected_text(selected_text(diff_a))
                .show_ui(ui, |ui| {
                    for l in &logs {
                        ui.selectable_value(diff_a, Some(l.id), basename(&l.path));
                    }
                });
            ui.label("Log B:");
            egui::ComboBox::from_id_salt("diff_log_b")
                .selected_text(selected_text(diff_b))
                .show_ui(ui, |ui| {
                    for l in &logs {
                        ui.selectable_value(diff_b, Some(l.id), basename(&l.path));
                    }
                });
        });

    // --- CENTER: diff table ------------------------------------------------
    egui::CentralPanel::default().show(ctx, |ui| {
        if logs.len() < 2 {
            ui.centered_and_justified(|ui| {
                ui.label("Load at least two logs in Setup to diff.");
            });
            return;
        }

        let (a, b) = match (*diff_a, *diff_b) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                ui.label("Pick a log for both A and B on the left.");
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
