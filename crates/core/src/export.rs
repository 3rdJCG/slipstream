//! CSV exporters — write query results to disk for sharing / spreadsheets.
//!
//! These are file-writing siblings of the view-driven queries: instead of
//! sizing the result to the screen, they stream every matching row through a
//! [`BufWriter`] so a multi-GB log never lands in memory at once. Each method
//! returns the number of *data* rows written (header excluded). Output is
//! UTF-8 with `\n` line endings; payload bytes are emitted as a quoted,
//! space-separated hex field so they survive a comma-delimited round trip.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::health::HealthRuleSet;
use crate::query::{FrameFilter, Session};
use crate::Result;

impl Session {
    /// Export every frame matching `filter` to a CSV file.
    ///
    /// Header: `index,timestamp,channel,can_id,is_fd,dlc,data`. `can_id` is
    /// `0x`-prefixed hex; `data` is the space-separated hex payload bytes in a
    /// quoted field (`"AA BB .."`). Matching reuses the same logic as
    /// [`Session::filtered_rows`]. Returns the number of data rows written.
    pub fn export_frames_csv(&self, filter: &FrameFilter, path: &Path) -> Result<u64> {
        let cols = self.store.columns();
        let indices = self.matching_indices(filter);
        let mut w = BufWriter::new(File::create(path)?);
        writeln!(w, "index,timestamp,channel,can_id,is_fd,dlc,data")?;
        for &i in &indices {
            let dlc = cols.dlc[i] as usize;
            let mut data = String::with_capacity(dlc * 3);
            for (n, b) in cols.data[i][..dlc].iter().enumerate() {
                if n > 0 {
                    data.push(' ');
                }
                data.push_str(&format!("{b:02X}"));
            }
            writeln!(
                w,
                "{},{},{},0x{:X},{},{},\"{}\"",
                i,
                cols.timestamp[i],
                cols.channel[i],
                cols.can_id[i],
                cols.is_fd[i],
                cols.dlc[i],
                data,
            )?;
        }
        w.flush()?;
        Ok(indices.len() as u64)
    }

    /// Export a decoded signal's `(timestamp, value)` series to a CSV file.
    ///
    /// Header: `timestamp,value`. Errors with [`crate::Error::UnknownSignal`]
    /// if the signal name is not found in any loaded DBC. Returns the number of
    /// data rows written.
    pub fn export_signal_csv(&self, signal: &str, path: &Path) -> Result<u64> {
        let series = self.signal_series(signal)?;
        let mut w = BufWriter::new(File::create(path)?);
        writeln!(w, "timestamp,value")?;
        for (t, v) in &series {
            writeln!(w, "{t},{v}")?;
        }
        w.flush()?;
        Ok(series.len() as u64)
    }

    /// Export every cadence violation from running `rules` to a CSV file.
    ///
    /// Header: `can_id,kind,t_start,t_end,observed_dt,expected_dt`, one row per
    /// violation across all rules (reuses [`Session::health_report`]). `can_id`
    /// is `0x`-prefixed hex. Returns the number of violation rows written.
    pub fn export_health_csv(&self, rules: &HealthRuleSet, path: &Path) -> Result<u64> {
        let report = self.health_report(rules);
        let mut w = BufWriter::new(File::create(path)?);
        writeln!(w, "can_id,kind,t_start,t_end,observed_dt,expected_dt")?;
        let mut rows = 0u64;
        for rule in &report.rules {
            for v in &rule.violations {
                writeln!(
                    w,
                    "0x{:X},{:?},{},{},{},{}",
                    v.can_id, v.kind, v.t_start, v.t_end, v.observed_dt, v.expected_dt,
                )?;
                rows += 1;
            }
        }
        w.flush()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a file back as lines, deleting it afterwards.
    fn read_and_cleanup(path: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(path).expect("read export back");
        let _ = std::fs::remove_file(path);
        text.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn export_frames_csv_header_and_row_count() {
        let s = Session::demo();
        let filter = FrameFilter {
            can_ids: vec![0x100],
            ..Default::default()
        };
        let expected = s.filtered_count(&filter);

        let path =
            std::env::temp_dir().join(format!("slipstream_export_frames_{}.csv", std::process::id()));
        let written = s.export_frames_csv(&filter, &path).expect("export frames");
        assert_eq!(written, expected, "rows written == filtered_count");

        let lines = read_and_cleanup(&path);
        assert_eq!(lines[0], "index,timestamp,channel,can_id,is_fd,dlc,data");
        // Header + one line per data row.
        assert_eq!(lines.len() as u64, expected + 1);
        // Every data row is the filtered id, in 0x hex, with a quoted payload.
        for line in &lines[1..] {
            assert!(line.contains(",0x100,"), "line {line}");
            assert!(line.contains('"'), "payload quoted in {line}");
        }
    }

    #[test]
    fn export_signal_csv_header_and_rows() {
        let s = Session::demo();
        let path =
            std::env::temp_dir().join(format!("slipstream_export_signal_{}.csv", std::process::id()));
        let written = s.export_signal_csv("EngineSpeed", &path).expect("export signal");
        assert!(written > 0, "decoded rows {written}");

        let lines = read_and_cleanup(&path);
        assert_eq!(lines[0], "timestamp,value");
        assert_eq!(lines.len() as u64, written + 1);
    }

    #[test]
    fn export_signal_csv_unknown_errors() {
        let s = Session::demo();
        let path = std::env::temp_dir().join("slipstream_export_unknown.csv");
        assert!(s.export_signal_csv("Nope", &path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_health_csv_header_and_violations() {
        use crate::health::{HealthRule, HealthRuleSet, Tolerance};
        use crate::predicate::Predicate;
        let s = Session::demo();
        // An unrealistically tight cadence flags every real gap as missing.
        let set = HealthRuleSet {
            rules: vec![HealthRule {
                can_id: 0x100,
                name: "EngineData".into(),
                expected_dt: 0.0001,
                tolerance: Tolerance::Percent(0.5),
                gate: Predicate::Always,
            }],
        };
        let path =
            std::env::temp_dir().join(format!("slipstream_export_health_{}.csv", std::process::id()));
        let written = s.export_health_csv(&set, &path).expect("export health");
        assert!(written > 0, "violations {written}");

        let lines = read_and_cleanup(&path);
        assert_eq!(lines[0], "can_id,kind,t_start,t_end,observed_dt,expected_dt");
        assert_eq!(lines.len() as u64, written + 1);
        assert!(lines[1].starts_with("0x100,"), "line {}", lines[1]);
    }
}
