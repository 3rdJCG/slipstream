//! Time-axis signal predicates — one composable predicate type reused by
//! search/filter, health-check gates, and (later) stat interval selection, so
//! these aren't built three different ways.
//!
//! A predicate is evaluated *over time*: [`PredEval::is_active`] answers whether
//! it holds at a timestamp. A `Signal` leaf holds its last decoded sample value
//! forward and is inactive before the first sample. Building a [`PredEval`] from
//! a [`Predicate`] requires decoding the referenced signals, so it lives on
//! `Session` (`build_pred`); the types here stay data-only and serde-friendly.

use serde::{Deserialize, Serialize};

/// Comparison operator for a signal-value predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compare {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Compare {
    pub fn eval(self, a: f64, b: f64) -> bool {
        match self {
            Compare::Eq => a == b,
            Compare::Ne => a != b,
            Compare::Gt => a > b,
            Compare::Ge => a >= b,
            Compare::Lt => a < b,
            Compare::Le => a <= b,
        }
    }
}

/// A composable predicate over decoded signal values and time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum Predicate {
    /// Always true (no constraint).
    #[default]
    Always,
    /// `signal op value`, with the signal value held forward between samples.
    Signal {
        signal: String,
        op: Compare,
        value: f64,
    },
    /// Active within an inclusive time window (seconds).
    TimeRange { t_start: f64, t_end: f64 },
    /// Conjunction — active when all children are.
    All(Vec<Predicate>),
    /// Disjunction — active when any child is.
    Any(Vec<Predicate>),
    /// Negation.
    Not(Box<Predicate>),
}

/// Precomputed evaluator: signal leaves are decoded once into sorted
/// `(timestamp, active?)` samples; composites hold their child evaluators.
pub enum PredEval {
    Always,
    Range(f64, f64),
    /// Sorted by timestamp; value held forward; inactive before the first sample.
    Signal(Vec<(f64, bool)>),
    All(Vec<PredEval>),
    Any(Vec<PredEval>),
    Not(Box<PredEval>),
}

impl PredEval {
    /// Is the predicate active at time `t`?
    pub fn is_active(&self, t: f64) -> bool {
        match self {
            PredEval::Always => true,
            PredEval::Range(a, b) => t >= *a && t <= *b,
            PredEval::Signal(samples) => {
                let idx = samples.partition_point(|(st, _)| *st <= t);
                idx > 0 && samples[idx - 1].1
            }
            PredEval::All(v) => v.iter().all(|p| p.is_active(t)),
            PredEval::Any(v) => v.iter().any(|p| p.is_active(t)),
            PredEval::Not(p) => !p.is_active(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_range_and_not() {
        // active in [0,10] AND NOT in [4,6]
        let e = PredEval::All(vec![
            PredEval::Range(0.0, 10.0),
            PredEval::Not(Box::new(PredEval::Range(4.0, 6.0))),
        ]);
        assert!(e.is_active(2.0));
        assert!(!e.is_active(5.0));
        assert!(e.is_active(8.0));
        assert!(!e.is_active(20.0));
    }

    #[test]
    fn signal_held_forward_and_inactive_before_first() {
        // samples: active from t=1 (true) until t=3 (false)
        let e = PredEval::Signal(vec![(1.0, true), (3.0, false)]);
        assert!(!e.is_active(0.5)); // before first sample
        assert!(e.is_active(1.0));
        assert!(e.is_active(2.9)); // held forward
        assert!(!e.is_active(3.0));
    }

    #[test]
    fn any_is_or() {
        let e = PredEval::Any(vec![PredEval::Range(0.0, 1.0), PredEval::Range(5.0, 6.0)]);
        assert!(e.is_active(0.5));
        assert!(!e.is_active(3.0));
        assert!(e.is_active(5.5));
    }
}
