//! The view-driven query API — the durable boundary between core and any UI.
//!
//! Every method here takes a serde request and returns a serde value. `gui-egui`
//! calls them directly; a future `gui-tauri` wraps each as a `#[tauri::command]`.
//! Crucially, results are sized to the *view* (a few thousand decimated points, a
//! window of rows), never the whole dataset — so the boundary cost is tiny
//! regardless of log size.

use serde::{Deserialize, Serialize};

use crate::dbc::DbcDatabase;
use crate::model::{FrameColumns, FrameRow, SignalMeta};
use crate::store::FrameStore;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Wire types (request / response)
// ---------------------------------------------------------------------------

/// Ask for a signal decimated to fit `px_width` horizontal pixels over a time
/// window. The reply has at most ~`2 * px_width` points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecimateRequest {
    pub signal: String,
    pub t_start: f64,
    pub t_end: f64,
    pub px_width: u32,
}

/// One pixel column's min/max envelope — drawing both preserves spikes that
/// naive subsampling would drop.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PlotBin {
    pub t: f64,
    pub v_min: f64,
    pub v_max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecimatedSeries {
    pub signal: String,
    pub bins: Vec<PlotBin>,
}

/// A window of raw frames for the table view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowWindowRequest {
    pub start: u64,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowWindow {
    pub total: u64,
    pub rows: Vec<FrameRow>,
}

/// Predicate over raw frames for the table/search view. Each field is a
/// conjunction (AND); within a list-valued field the values are OR'd. Empty
/// lists / `None` bounds mean "no constraint", so a default `FrameFilter`
/// matches every frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameFilter {
    /// CAN ids to keep (empty = all ids).
    pub can_ids: Vec<u32>,
    /// Channels to keep (empty = all channels).
    pub channels: Vec<u8>,
    /// Inclusive lower time bound, seconds (`None` = open).
    pub t_start: Option<f64>,
    /// Inclusive upper time bound, seconds (`None` = open).
    pub t_end: Option<f64>,
}

impl FrameFilter {
    /// Does row `i` of `cols` satisfy this filter?
    fn matches(&self, cols: &FrameColumns, i: usize) -> bool {
        if !self.can_ids.is_empty() && !self.can_ids.contains(&cols.can_id[i]) {
            return false;
        }
        if !self.channels.is_empty() && !self.channels.contains(&cols.channel[i]) {
            return false;
        }
        let t = cols.timestamp[i];
        if let Some(start) = self.t_start {
            if t < start {
                return false;
            }
        }
        if let Some(end) = self.t_end {
            if t > end {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsRequest {
    pub signal: String,
    pub t_start: f64,
    pub t_end: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalStats {
    pub signal: String,
    pub count: u64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

/// Per-message-id cycle-time statistics: how regularly frames sharing a CAN id
/// arrive. `dt` is the inter-arrival time (seconds) between consecutive frames
/// of that id (frames are time-ordered). `jitter` is `max_dt - min_dt`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CycleStats {
    pub can_id: u32,
    /// Number of inter-arrival intervals (one fewer than the frame count).
    pub count: u64,
    pub mean_dt: f64,
    pub min_dt: f64,
    pub max_dt: f64,
    pub jitter: f64,
}

// ---------------------------------------------------------------------------
// Session — owns loaded state, answers queries
// ---------------------------------------------------------------------------

pub struct Session {
    store: FrameStore,
    dbc: DbcDatabase,
    /// Total time span of the loaded log, seconds.
    duration: f64,
}

impl Session {
    /// Build a synthetic session so the egui UI has something to render before
    /// the real ingest/decode lands. Deterministic — no RNG.
    pub fn demo() -> Self {
        let duration = 60.0;
        let n = 5_000u32;
        let mut frames = FrameColumns::default();
        // Encode real waveforms into frame bytes per the demo DBC layout, so the
        // whole encode→ingest→decode path is exercised (not faked).
        for i in 0..n {
            let t = i as f64 / n as f64 * duration;

            // 0x100 EngineData: EngineSpeed (u16 @0, *0.25), CoolantTemp (u8 @byte2, -40).
            let mut p100 = [0u8; 8];
            let rpm = 3000.0 + 2200.0 * (t * 0.5).sin();
            p100[0..2].copy_from_slice(&((rpm / 0.25) as u16).to_le_bytes());
            let temp = 60.0 + 20.0 * (t * 0.05).sin();
            p100[2] = (temp + 40.0) as u8;
            frames.push(t, 1, 0x100, false, &p100);

            // 0x200 VehicleState: VehicleSpeed (u16 @0, *0.01), Gear (nibble @byte2).
            let mut p200 = [0u8; 8];
            let kph = 60.0 + 55.0 * (t * 0.3 + 1.0).sin();
            p200[0..2].copy_from_slice(&((kph / 0.01) as u16).to_le_bytes());
            p200[2] = ((i / 500) % 6) as u8;
            frames.push(t, 1, 0x200, false, &p200);
        }

        Self {
            store: FrameStore::new(frames),
            dbc: demo_dbc(),
            duration,
        }
    }

    /// Open and ingest a real BLF/ASC log. Signals stay empty until a DBC is
    /// loaded (P1); the frame table works immediately.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let frames = crate::ingest::parse(path)?;
        let duration = frames.timestamp.last().copied().unwrap_or(0.0);
        Ok(Self {
            store: FrameStore::new(frames),
            dbc: DbcDatabase::default(),
            duration,
        })
    }

    /// Open a log and load a DBC, so signals are immediately decodable/plottable.
    pub fn open_with_dbc(log: &std::path::Path, dbc: &std::path::Path) -> Result<Self> {
        let mut session = Self::open(log)?;
        session.dbc = DbcDatabase::load(dbc)?;
        Ok(session)
    }

    /// Replace this session's DBC (e.g. loaded later from the Config tab).
    /// `available_signals` reads the DBC live, so callers see the new signals
    /// immediately — nothing to invalidate.
    pub fn set_dbc(&mut self, dbc: DbcDatabase) {
        self.dbc = dbc;
    }

    /// Load a `.dbc` file and set it as this session's DBC, replacing any
    /// existing one.
    pub fn load_dbc(&mut self, path: &std::path::Path) -> Result<()> {
        self.dbc = DbcDatabase::load(path)?;
        Ok(())
    }

    /// Re-ingest a BLF/ASC log into this session: replace the frame store and
    /// recompute the duration. The DBC is kept as-is.
    pub fn load_log(&mut self, path: &std::path::Path) -> Result<()> {
        let frames = crate::ingest::parse(path)?;
        self.duration = frames.timestamp.last().copied().unwrap_or(0.0);
        self.store = FrameStore::new(frames);
        Ok(())
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    pub fn frame_count(&self) -> u64 {
        self.store.len() as u64
    }

    /// Signals available to plot (drives the signal tree).
    pub fn available_signals(&self) -> Vec<SignalMeta> {
        self.dbc.signal_metas()
    }

    /// Decimate one signal to the requested pixel width (min/max per bucket).
    pub fn decimate(&self, req: &DecimateRequest) -> Result<DecimatedSeries> {
        let dense = self.signal_series(&req.signal)?;
        let bins = min_max_decimate(&dense, req.t_start, req.t_end, req.px_width.max(1));
        Ok(DecimatedSeries {
            signal: req.signal.clone(),
            bins,
        })
    }

    /// A single raw frame row (used by the virtualized table view).
    pub fn frame_row(&self, i: u64) -> Option<FrameRow> {
        if (i as usize) < self.store.len() {
            Some(self.store.row(i as usize))
        } else {
            None
        }
    }

    /// A window of raw frames for the table.
    pub fn rows(&self, req: &RowWindowRequest) -> RowWindow {
        RowWindow {
            total: self.store.len() as u64,
            rows: self.store.window(req.start as usize, req.count as usize),
        }
    }

    /// Number of frames matching `filter`.
    pub fn filtered_count(&self, filter: &FrameFilter) -> u64 {
        self.matching_indices(filter).len() as u64
    }

    /// A window of frames matching `filter`, windowed into the filtered list.
    /// Each returned row keeps its index in the full store.
    pub fn filtered_rows(&self, filter: &FrameFilter, start: u64, count: u32) -> RowWindow {
        let idx = self.matching_indices(filter);
        let s = (start as usize).min(idx.len());
        let e = (s + count as usize).min(idx.len());
        RowWindow {
            total: idx.len() as u64,
            rows: self.store.window_of(&idx[s..e]),
        }
    }

    /// Row indices (into the full store) matching `filter`. O(n) scan; a future
    /// optimization is to cache this per filter (see CLAUDE.md).
    fn matching_indices(&self, filter: &FrameFilter) -> Vec<usize> {
        let cols = self.store.columns();
        (0..cols.len()).filter(|&i| filter.matches(cols, i)).collect()
    }

    /// Summary statistics for a signal over a time window.
    pub fn signal_stats(&self, req: &StatsRequest) -> Result<SignalStats> {
        let dense = self.signal_series(&req.signal)?;
        let mut count = 0u64;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0;
        for &(t, v) in &dense {
            if t >= req.t_start && t <= req.t_end {
                count += 1;
                min = min.min(v);
                max = max.max(v);
                sum += v;
            }
        }
        if count == 0 {
            return Err(Error::UnknownSignal(req.signal.clone()));
        }
        Ok(SignalStats {
            signal: req.signal.clone(),
            count,
            min,
            max,
            mean: sum / count as f64,
        })
    }

    /// Cycle-time statistics for a single CAN id, or `None` if fewer than two
    /// frames carry that id (no inter-arrival interval to measure).
    pub fn cycle_stats(&self, can_id: u32) -> Option<CycleStats> {
        let cols = self.store.columns();
        let mut prev: Option<f64> = None;
        let mut count = 0u64;
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for i in 0..cols.len() {
            if cols.can_id[i] != can_id {
                continue;
            }
            let t = cols.timestamp[i];
            if let Some(p) = prev {
                let dt = t - p;
                count += 1;
                sum += dt;
                min = min.min(dt);
                max = max.max(dt);
            }
            prev = Some(t);
        }
        if count == 0 {
            return None;
        }
        Some(CycleStats {
            can_id,
            count,
            mean_dt: sum / count as f64,
            min_dt: min,
            max_dt: max,
            jitter: max - min,
        })
    }

    /// Cycle-time statistics for every distinct CAN id present, sorted ascending
    /// by `can_id`. Ids with fewer than two frames are omitted.
    pub fn all_cycle_stats(&self) -> Vec<CycleStats> {
        let cols = self.store.columns();
        let mut ids: Vec<u32> = cols.can_id.clone();
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter()
            .filter_map(|id| self.cycle_stats(id))
            .collect()
    }

    /// Build default health rules from the DBC's `GenMsgCycleTime` attributes
    /// (one rule per message that declares a cycle time), with a shared
    /// `tolerance`. Manual rules can be added on top.
    pub fn dbc_health_rules(&self, tolerance: f64) -> crate::health::HealthRuleSet {
        use crate::health::{Gate, HealthRule};
        let rules = self
            .dbc
            .messages
            .iter()
            .filter_map(|m| {
                m.expected_cycle_ms.map(|ms| HealthRule {
                    can_id: m.can_id,
                    name: m.name.clone(),
                    expected_dt: ms / 1000.0,
                    tolerance,
                    gate: Gate::Always,
                })
            })
            .collect();
        crate::health::HealthRuleSet { rules }
    }

    /// Run frame-health checks for every rule, returning all cadence violations
    /// (an id with fewer than two frames inside its gate yields a single
    /// `NoData` violation).
    pub fn check_health(
        &self,
        rules: &crate::health::HealthRuleSet,
    ) -> Vec<crate::health::Violation> {
        use crate::health::{scan_cadence, Violation, ViolationKind};
        let cols = self.store.columns();
        let mut out = Vec::new();
        for rule in &rules.rules {
            let gate = self.build_gate(&rule.gate);
            let mut times = Vec::new();
            for i in 0..cols.len() {
                if cols.can_id[i] == rule.can_id && gate.is_active(cols.timestamp[i]) {
                    times.push(cols.timestamp[i]);
                }
            }
            if times.len() < 2 {
                out.push(Violation {
                    can_id: rule.can_id,
                    kind: ViolationKind::NoData,
                    t_start: 0.0,
                    t_end: self.duration,
                    observed_dt: 0.0,
                    expected_dt: rule.expected_dt,
                });
                continue;
            }
            out.extend(scan_cadence(rule.can_id, &times, rule.expected_dt, rule.tolerance));
        }
        out
    }

    /// Precompute a gate evaluator (decodes the gate signal once when needed).
    fn build_gate(&self, gate: &crate::health::Gate) -> GateEval {
        use crate::health::Gate;
        match gate {
            Gate::Always => GateEval::Always,
            Gate::TimeRange { t_start, t_end } => GateEval::Range(*t_start, *t_end),
            Gate::Signal { signal, op, value } => {
                let samples = self
                    .signal_series(signal)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(t, v)| (t, op.eval(v, *value)))
                    .collect();
                GateEval::Signal(samples)
            }
        }
    }

    /// Decode a signal's `(timestamp, value)` series from the columnar frames
    /// via the DBC. Gathers all frames for the signal's `can_id` and decodes
    /// each payload (the column-at-a-time decode path).
    fn signal_series(&self, name: &str) -> Result<Vec<(f64, f64)>> {
        let (can_id, sig) = self
            .dbc
            .find_signal(name)
            .ok_or_else(|| Error::UnknownSignal(name.to_string()))?;
        let cols = self.store.columns();
        let mut out = Vec::new();
        for i in 0..cols.len() {
            if cols.can_id[i] != can_id {
                continue;
            }
            let dlc = cols.dlc[i] as usize;
            if let Some(v) = crate::dbc::decode_signal(sig, &cols.data[i][..dlc]) {
                out.push((cols.timestamp[i], v));
            }
        }
        Ok(out)
    }
}

/// Precomputed gate activity used by [`Session::check_health`]. Signal samples
/// are `(timestamp, active?)` sorted by time; the value is held forward and is
/// inactive before the first sample.
enum GateEval {
    Always,
    Range(f64, f64),
    Signal(Vec<(f64, bool)>),
}

impl GateEval {
    fn is_active(&self, t: f64) -> bool {
        match self {
            GateEval::Always => true,
            GateEval::Range(a, b) => t >= *a && t <= *b,
            GateEval::Signal(samples) => {
                let idx = samples.partition_point(|(st, _)| *st <= t);
                idx > 0 && samples[idx - 1].1
            }
        }
    }
}

/// Min/max decimation: bucket points into `px_width` columns over `[t0, t1]`,
/// emit (t, min, max) per non-empty bucket. Linear scan, allocation-free
/// except the output.
fn min_max_decimate(points: &[(f64, f64)], t0: f64, t1: f64, px_width: u32) -> Vec<PlotBin> {
    let span = (t1 - t0).max(f64::EPSILON);
    let buckets = px_width as usize;
    let mut acc: Vec<Option<(f64, f64, f64)>> = vec![None; buckets]; // (t_first, min, max)
    for &(t, v) in points {
        if t < t0 || t > t1 {
            continue;
        }
        let mut idx = ((t - t0) / span * buckets as f64) as usize;
        if idx >= buckets {
            idx = buckets - 1;
        }
        match &mut acc[idx] {
            Some((_, mn, mx)) => {
                *mn = mn.min(v);
                *mx = mx.max(v);
            }
            slot @ None => *slot = Some((t, v, v)),
        }
    }
    acc.into_iter()
        .flatten()
        .map(|(t, v_min, v_max)| PlotBin { t, v_min, v_max })
        .collect()
}

fn demo_dbc() -> DbcDatabase {
    use crate::dbc::{DbcMessage, SignalDef};
    use crate::model::ByteOrder;
    let sig = |name: &str, start_bit: u16, bit_len: u16, scale: f64, offset: f64, unit: &str| {
        SignalDef {
            name: name.to_string(),
            start_bit,
            bit_len,
            byte_order: ByteOrder::Intel,
            signed: false,
            scale,
            offset,
            unit: unit.to_string(),
        }
    };
    DbcDatabase {
        messages: vec![
            DbcMessage {
                can_id: 0x100,
                name: "EngineData".to_string(),
                signals: vec![
                    sig("EngineSpeed", 0, 16, 0.25, 0.0, "rpm"),
                    sig("CoolantTemp", 16, 8, 1.0, -40.0, "degC"),
                ],
                // Demo emits 5000 frames over 60 s ⇒ 12 ms cadence.
                expected_cycle_ms: Some(12.0),
            },
            DbcMessage {
                can_id: 0x200,
                name: "VehicleState".to_string(),
                signals: vec![
                    sig("VehicleSpeed", 0, 16, 0.01, 0.0, "km/h"),
                    sig("Gear", 16, 4, 1.0, 0.0, ""),
                ],
                expected_cycle_ms: Some(12.0),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_decodes_engine_speed_end_to_end() {
        let s = Session::demo();
        let st = s
            .signal_stats(&StatsRequest {
                signal: "EngineSpeed".into(),
                t_start: 0.0,
                t_end: s.duration(),
            })
            .expect("stats");
        // Demo encodes 3000 ± 2200 rpm; decode should recover that range.
        assert!(st.count > 1000, "count {}", st.count);
        assert!(st.min >= 700.0 && st.max <= 5300.0, "min {} max {}", st.min, st.max);

        let d = s
            .decimate(&DecimateRequest {
                signal: "EngineSpeed".into(),
                t_start: 0.0,
                t_end: s.duration(),
                px_width: 800,
            })
            .expect("decimate");
        assert!(!d.bins.is_empty());
        assert!(d.bins.len() <= 800);
    }

    #[test]
    fn unknown_signal_errors() {
        let s = Session::demo();
        assert!(s
            .signal_stats(&StatsRequest {
                signal: "Nope".into(),
                t_start: 0.0,
                t_end: 1.0,
            })
            .is_err());
    }

    #[test]
    fn filter_by_can_id() {
        let s = Session::demo();
        // Demo emits 0x100 and 0x200 in equal numbers.
        let f = FrameFilter {
            can_ids: vec![0x100],
            ..Default::default()
        };
        let count = s.filtered_count(&f);
        assert_eq!(count, s.frame_count() / 2);
        let w = s.filtered_rows(&f, 0, 16);
        assert_eq!(w.total, count);
        assert!(w.rows.iter().all(|r| r.can_id == 0x100));
    }

    #[test]
    fn filter_by_time_range() {
        let s = Session::demo();
        let f = FrameFilter {
            t_start: Some(0.0),
            t_end: Some(s.duration() / 2.0),
            ..Default::default()
        };
        let count = s.filtered_count(&f);
        assert!(count > 0 && count < s.frame_count(), "count {count}");
    }

    #[test]
    fn cycle_stats_demo_regular_grid() {
        let s = Session::demo();
        // 0x100 is emitted n=5000 times over 60s on a regular grid, so the
        // inter-arrival time is ~60/5000 = 0.012 s with negligible jitter.
        let cs = s.cycle_stats(0x100).expect("0x100 cycle stats");
        assert_eq!(cs.can_id, 0x100);
        assert_eq!(cs.count, 4999); // one fewer interval than frames
        let expected = 60.0 / 5000.0;
        assert!((cs.mean_dt - expected).abs() < 1e-9, "mean_dt {}", cs.mean_dt);
        assert!(cs.jitter < 1e-6, "jitter {}", cs.jitter);
        assert!((cs.max_dt - cs.min_dt - cs.jitter).abs() < 1e-12);

        // Unknown id has no frames at all.
        assert!(s.cycle_stats(0x999).is_none());
    }

    #[test]
    fn all_cycle_stats_sorted_by_id() {
        let s = Session::demo();
        let all = s.all_cycle_stats();
        assert_eq!(all.len(), 2, "two distinct ids");
        assert_eq!(all[0].can_id, 0x100);
        assert_eq!(all[1].can_id, 0x200);
        let expected = 60.0 / 5000.0;
        for cs in &all {
            assert!((cs.mean_dt - expected).abs() < 1e-9, "mean_dt {}", cs.mean_dt);
            assert!(cs.jitter < 1e-6, "jitter {}", cs.jitter);
        }
    }

    #[test]
    fn default_filter_matches_all() {
        let s = Session::demo();
        assert_eq!(s.filtered_count(&FrameFilter::default()), s.frame_count());
    }

    #[test]
    fn health_demo_regular_is_clean() {
        use crate::health::{Gate, HealthRule, HealthRuleSet};
        let s = Session::demo();
        let set = HealthRuleSet {
            rules: vec![HealthRule {
                can_id: 0x100,
                name: "EngineData".into(),
                expected_dt: 0.012,
                tolerance: 0.5,
                gate: Gate::Always,
            }],
        };
        assert!(s.check_health(&set).is_empty());
    }

    #[test]
    fn health_missing_id_is_nodata() {
        use crate::health::{Gate, HealthRule, HealthRuleSet, ViolationKind};
        let s = Session::demo();
        let set = HealthRuleSet {
            rules: vec![HealthRule {
                can_id: 0x999,
                name: "Absent".into(),
                expected_dt: 0.01,
                tolerance: 0.2,
                gate: Gate::Always,
            }],
        };
        let v = s.check_health(&set);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::NoData);
    }

    #[test]
    fn dbc_health_rules_built_from_cycle_time() {
        let s = Session::demo();
        let set = s.dbc_health_rules(0.3);
        assert_eq!(set.rules.len(), 2); // both demo messages declare 12 ms
        assert!(set.rules.iter().all(|r| (r.expected_dt - 0.012).abs() < 1e-9));
        // The DBC-derived rules pass on the regular demo log.
        assert!(s.check_health(&set).is_empty());
    }

    #[test]
    fn health_time_range_gate_excludes_frames() {
        use crate::health::{Gate, HealthRule, HealthRuleSet, ViolationKind};
        let s = Session::demo();
        // Gate to an empty window ⇒ no frames considered ⇒ NoData.
        let set = HealthRuleSet {
            rules: vec![HealthRule {
                can_id: 0x100,
                name: "EngineData".into(),
                expected_dt: 0.012,
                tolerance: 0.5,
                gate: Gate::TimeRange {
                    t_start: 1000.0,
                    t_end: 2000.0,
                },
            }],
        };
        let v = s.check_health(&set);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::NoData);
    }

    #[test]
    fn set_dbc_empty_clears_signals() {
        let mut s = Session::demo();
        assert!(!s.available_signals().is_empty());
        // Swapping in an empty DBC drops all signals (read live, no cache).
        s.set_dbc(DbcDatabase::default());
        assert!(s.available_signals().is_empty());
    }

    #[test]
    fn load_dbc_from_file_populates_signals() {
        // A default session has no DBC, so no signals.
        let mut s = Session::demo();
        s.set_dbc(DbcDatabase::default());
        assert!(s.available_signals().is_empty());

        // Write a tiny DBC to a temp file and load it.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("slipstream_test_{}.dbc", std::process::id()));
        std::fs::write(
            &path,
            "VERSION \"\"\n\nBO_ 256 EngineData: 8 ECU\n SG_ Rpm : 0|16@1+ (0.25,0) [0|16383] \"rpm\" Vector__XXX\n",
        )
        .expect("write temp dbc");

        s.load_dbc(&path).expect("load_dbc");
        let _ = std::fs::remove_file(&path);

        let signals = s.available_signals();
        assert!(!signals.is_empty());
        assert!(signals.iter().any(|m| m.name == "Rpm"));
    }
}
