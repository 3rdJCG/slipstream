//! Frame-health checking: are frames arriving as expected?
//!
//! A [`HealthRule`] says "CAN id X should arrive every `expected_dt` seconds
//! (±`tolerance`)", optionally only while a [`Gate`] is active (e.g. ignition
//! ON). [`scan_cadence`] is the pure core: given the (gated) arrival times of an
//! id, it reports cadence [`Violation`]s. `Session::check_health` (in `query`)
//! gathers the times per rule, applies the gate, and calls this.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::predicate::Predicate;
use crate::{Error, Result};

/// Expected cadence of one CAN id, optionally gated by a [`Predicate`] (e.g.
/// "ignition ON"). The rule is only evaluated where its gate is active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthRule {
    pub can_id: u32,
    /// Display name (e.g. the DBC message name).
    pub name: String,
    /// Expected inter-arrival time, seconds.
    pub expected_dt: f64,
    /// Fractional tolerance, e.g. 0.2 = ±20%.
    pub tolerance: f64,
    /// When to apply the rule. Defaults to [`Predicate::Always`].
    #[serde(default)]
    pub gate: Predicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationKind {
    /// Gap too large — a frame is late or was dropped.
    Missing,
    /// Gap too small — frames arriving faster than expected.
    Excessive,
    /// Fewer than two frames in the active window — nothing to measure.
    NoData,
}

/// A single cadence violation over `[t_start, t_end]`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Violation {
    pub can_id: u32,
    pub kind: ViolationKind,
    pub t_start: f64,
    pub t_end: f64,
    pub observed_dt: f64,
    pub expected_dt: f64,
}

/// A persistable collection of rules (manual + DBC-derived).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthRuleSet {
    pub rules: Vec<HealthRule>,
}

/// Per-rule outcome of a health check: the rule's identity, whether it passed,
/// counts by violation kind, and the violations themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleReport {
    pub can_id: u32,
    pub name: String,
    pub expected_dt: f64,
    /// `true` when this rule produced no violations.
    pub ok: bool,
    /// Number of [`ViolationKind::Missing`] violations.
    pub missing: u64,
    /// Number of [`ViolationKind::Excessive`] violations.
    pub excessive: u64,
    /// `true` if a [`ViolationKind::NoData`] violation is present.
    pub no_data: bool,
    pub violations: Vec<Violation>,
}

/// RPC-shaped result of running a [`HealthRuleSet`]: one [`RuleReport`] per rule
/// plus rolled-up aggregates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub rules: Vec<RuleReport>,
    /// Total number of violations across all rules.
    pub total_violations: u64,
    /// `true` when every rule passed (no violations anywhere).
    pub all_ok: bool,
}

impl HealthRuleSet {
    /// Save as pretty JSON.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Parse(format!("rule set serialize: {e}")))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load from JSON.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|e| Error::Parse(format!("rule set parse: {e}")))
    }
}

/// Pure cadence check: given time-ordered arrival `times` of `can_id`, report
/// intervals whose gap falls outside `expected_dt * (1 ± tol)`. Returns empty
/// for fewer than two samples (the caller decides whether that is `NoData`).
pub fn scan_cadence(can_id: u32, times: &[f64], expected_dt: f64, tol: f64) -> Vec<Violation> {
    let mut out = Vec::new();
    if times.len() < 2 || expected_dt <= 0.0 {
        return out;
    }
    let hi = expected_dt * (1.0 + tol);
    let lo = expected_dt * (1.0 - tol);
    for w in times.windows(2) {
        let dt = w[1] - w[0];
        let kind = if dt > hi {
            ViolationKind::Missing
        } else if dt < lo {
            ViolationKind::Excessive
        } else {
            continue;
        };
        out.push(Violation {
            can_id,
            kind,
            t_start: w[0],
            t_end: w[1],
            observed_dt: dt,
            expected_dt,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_cadence_has_no_violations() {
        let times: Vec<f64> = (0..10).map(|i| i as f64 * 1.0).collect();
        assert!(scan_cadence(1, &times, 1.0, 0.1).is_empty());
    }

    #[test]
    fn detects_missing_gap() {
        // 0,1,2 then a jump to 6 (gap of 4 >> 1).
        let times = [0.0, 1.0, 2.0, 6.0];
        let v = scan_cadence(0x10, &times, 1.0, 0.2);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::Missing);
        assert_eq!(v[0].can_id, 0x10);
        assert!((v[0].observed_dt - 4.0).abs() < 1e-9);
        assert!((v[0].t_start - 2.0).abs() < 1e-9 && (v[0].t_end - 6.0).abs() < 1e-9);
    }

    #[test]
    fn detects_excessive_burst() {
        // a too-fast pair (0.1 << 1).
        let times = [0.0, 1.0, 1.1, 2.1];
        let v = scan_cadence(1, &times, 1.0, 0.2);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::Excessive);
    }

    #[test]
    fn rule_set_round_trips() {
        let set = HealthRuleSet {
            rules: vec![HealthRule {
                can_id: 0x100,
                name: "EngineData".into(),
                expected_dt: 0.01,
                tolerance: 0.2,
                gate: Predicate::Signal {
                    signal: "Ignition".into(),
                    op: crate::predicate::Compare::Ge,
                    value: 1.0,
                },
            }],
        };
        let path = std::env::temp_dir().join("slipstream_test_rules.json");
        set.save(&path).unwrap();
        let back = HealthRuleSet::load(&path).unwrap();
        assert_eq!(back.rules.len(), 1);
        assert_eq!(back.rules[0].can_id, 0x100);
        assert!((back.rules[0].expected_dt - 0.01).abs() < 1e-12);
        let _ = std::fs::remove_file(&path);
    }
}
