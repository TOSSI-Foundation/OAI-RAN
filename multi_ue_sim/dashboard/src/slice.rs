//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Slice / SLA model shared by the Monitor, Configure and Advanced pages.
//
//  * SliceId          — an S-NSSAI (SST + optional SD), compared numerically.
//  * SliceMapEntry    — UE -> slice/cell identity, read from
//                       /tmp/multi_ue_slice_map.json (written by run_nue.sh / mue_add.sh).
//  * SliceMetric      — one configured RAN slice's live PRB picture, parsed from the
//                       ocudu DU log lines:
//      "Scheduler slice pci={} sst={}{ sd=0x..} metrics: nof_ues={}
//       dl[min_prbs={} max_prbs={} ded_prbs={}] ul[min_prbs={} max_prbs={} ded_prbs={}]
//       avg_dl_rbs_per_slot={:.2f} avg_ul_rbs_per_slot={:.2f}
//       dl_prb_ratio={:.1f}% ul_prb_ratio={:.1f}%"
//    (verified against scheduler_metrics_consumers.cpp:278-296 in the ocudu tree).
//
// The DU log path is the ocudu DU's `log.filename` (default "du.log") resolved
// against its working dir. Under clean_start.sh that is $OCUDU_TREE/build/apps/du.log.
// Override with the DU_LOG env var.

use std::collections::HashMap;
use std::fs;

use serde::Deserialize;

/// An S-NSSAI: SST plus an optional Slice Differentiator. Compared by numeric value
/// (the DU logs SD in hex, run_nue.sh / the YAML use decimal — both map to the same u32).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SliceId {
    pub sst: u8,
    pub sd: Option<u32>,
}

impl SliceId {
    pub fn new(sst: u8, sd: Option<u32>) -> Self {
        Self { sst, sd }
    }

    /// Short label for tables, e.g. "S1/sd1" or "S2".
    pub fn short(&self) -> String {
        match self.sd {
            Some(sd) => format!("S{}/sd{}", self.sst, sd),
            None => format!("S{}", self.sst),
        }
    }

    /// A conventional service name by SST (TS 23.501 standardised SST values).
    pub fn service(&self) -> &'static str {
        match self.sst {
            1 => "eMBB",
            2 => "URLLC",
            3 => "mMTC",
            4 => "V2X",
            _ => "slice",
        }
    }
}

/// One UE's slice/cell identity, as written to /tmp/multi_ue_slice_map.json.
#[derive(Clone, Debug, Deserialize)]
pub struct SliceMapEntry {
    pub sst: u8,
    /// SD as written by the scripts: a decimal string ("1") or JSON null.
    #[serde(default)]
    pub sd: Option<String>,
    #[serde(default)]
    pub dnn: Option<String>,
    #[serde(default)]
    pub cell: Option<String>,
    #[serde(default)]
    pub imsi: Option<String>,
}

impl SliceMapEntry {
    /// Numeric SD (decimal string from the script -> u32), if present.
    pub fn sd_num(&self) -> Option<u32> {
        self.sd.as_deref().and_then(parse_sd)
    }
    pub fn slice_id(&self) -> SliceId {
        SliceId::new(self.sst, self.sd_num())
    }
    /// Cell index (0=A). Accepts either a LETTER ("A","B"…) or a numeric index ("0","1"…) —
    /// older run_nue.sh wrote the raw UE_CELL number, newer writes the letter; handle both.
    pub fn cell_index(&self) -> usize {
        let raw = self.cell.as_deref().unwrap_or("A").trim();
        if let Ok(n) = raw.parse::<usize>() {
            return n.min(25);
        }
        raw.chars()
            .next()
            .map(|c| (c.to_ascii_uppercase() as u8).wrapping_sub(b'A') as usize)
            .unwrap_or(0)
            .min(25)
    }
    /// Normalised cell letter (A=0), regardless of how the cell was recorded.
    pub fn cell_label(&self) -> String {
        ((b'A' + self.cell_index() as u8) as char).to_string()
    }
}

pub const SLICE_MAP_PATH: &str = "/tmp/multi_ue_slice_map.json";

/// Read the UE -> slice/cell map. Returns an empty map if the file is absent or
/// malformed (the dashboard degrades gracefully to "?" slices).
pub fn read_slice_map() -> HashMap<usize, SliceMapEntry> {
    let raw: HashMap<String, SliceMapEntry> = fs::read_to_string(SLICE_MAP_PATH)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    raw.into_iter()
        .filter_map(|(k, v)| k.parse::<usize>().ok().map(|id| (id, v)))
        .collect()
}

/// Parse an SD token that may be decimal ("2") or hex ("0x2"). Case-insensitive.
pub fn parse_sd(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u32>().ok()
    }
}

/// One configured RAN slice's live PRB picture, parsed from a DU log "Scheduler slice" line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SliceMetric {
    pub pci: u16,
    pub sst: u8,
    pub sd: Option<u32>,
    pub nof_ues: u32,
    pub dl_min_prbs: u32,
    pub dl_max_prbs: u32,
    pub dl_ded_prbs: u32,
    pub avg_dl_rbs_per_slot: f64,
    pub avg_ul_rbs_per_slot: f64,
    pub dl_prb_ratio: f64, // percent of cell PRBs used DL (the live SLA share)
    pub ul_prb_ratio: f64,
}

impl SliceMetric {
    pub fn slice_id(&self) -> SliceId {
        SliceId::new(self.sst, self.sd)
    }
}

/// Pull `key=value` out of a log line, returning the value token (whitespace- or
/// bracket-delimited). `start` lets the caller restrict the search to a substring
/// region (used to disambiguate the dl[...] vs ul[...] groups that share key names).
fn field<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let p = s.find(key)?;
    let rest = &s[p + key.len()..];
    let val: &str = rest
        .split(|c: char| c == ' ' || c == ']' || c == '%' || c == '\t')
        .next()?;
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Parse a single DU log line into a SliceMetric, or None if it is not a
/// "Scheduler slice" metrics line. Robust to the srsRAN log prefix.
pub fn parse_slice_line(line: &str) -> Option<SliceMetric> {
    let marker = "Scheduler slice ";
    let p = line.find(marker)?;
    let s = &line[p..];

    let pci = field(s, "pci=")?.parse::<u16>().ok()?;
    let sst = field(s, "sst=")?.parse::<u8>().ok()?;
    // SD is only present when != wildcard; printed as "sd=0x.."
    let sd = field(s, "sd=").and_then(parse_sd);
    let nof_ues = field(s, "nof_ues=").and_then(|v| v.parse().ok()).unwrap_or(0);

    // dl[...] group — restrict to the bracketed region so we don't read ul's keys.
    let dl_grp = s
        .find("dl[")
        .map(|i| &s[i..s[i..].find(']').map(|j| i + j + 1).unwrap_or(s.len())])
        .unwrap_or(s);
    let dl_min = field(dl_grp, "min_prbs=").and_then(|v| v.parse().ok()).unwrap_or(0);
    let dl_max = field(dl_grp, "max_prbs=").and_then(|v| v.parse().ok()).unwrap_or(0);
    let dl_ded = field(dl_grp, "ded_prbs=").and_then(|v| v.parse().ok()).unwrap_or(0);

    let avg_dl = field(s, "avg_dl_rbs_per_slot=").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let avg_ul = field(s, "avg_ul_rbs_per_slot=").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let dl_ratio = field(s, "dl_prb_ratio=").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let ul_ratio = field(s, "ul_prb_ratio=").and_then(|v| v.parse().ok()).unwrap_or(0.0);

    Some(SliceMetric {
        pci,
        sst,
        sd,
        nof_ues,
        dl_min_prbs: dl_min,
        dl_max_prbs: dl_max,
        dl_ded_prbs: dl_ded,
        avg_dl_rbs_per_slot: avg_dl,
        avg_ul_rbs_per_slot: avg_ul,
        dl_prb_ratio: dl_ratio,
        ul_prb_ratio: ul_ratio,
    })
}

/// Candidate DU log locations, in priority order. DU_LOG env wins.
pub fn du_log_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("DU_LOG") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    if let Ok(tree) = std::env::var("OCUDU_TREE") {
        if !tree.is_empty() {
            v.push(format!("{}/build/apps/du.log", tree)); // clean_start.sh cwd = build/apps
            v.push(format!("{}/build/apps/du/du.log", tree));
            v.push(format!("{}/du.log", tree));
        }
    }
    v.push("du.log".to_string());
    v
}

/// The existing, non-empty DU log with the NEWEST mtime — so a stale log left in an
/// earlier-listed location (e.g. build/apps/du.log) never shadows the one the running
/// DU is actually writing (e.g. build/apps/du/du.log).
pub fn du_log_path() -> Option<String> {
    du_log_candidates()
        .into_iter()
        .filter_map(|p| {
            fs::metadata(&p).ok().and_then(|m| {
                if m.is_file() && m.len() > 0 {
                    m.modified().ok().map(|t| (p, t))
                } else {
                    None
                }
            })
        })
        .max_by_key(|(_, t)| *t)
        .map(|(p, _)| p)
}

/// Read the latest SliceMetric for each slice from the DU log tail. Returns a map
/// keyed by SliceId, holding the most-recent line per slice (the DU re-logs every
/// `du_report_period` ms, so the tail holds several reporting cycles).
pub fn read_slice_metrics() -> HashMap<SliceId, SliceMetric> {
    let mut out: HashMap<SliceId, SliceMetric> = HashMap::new();
    let path = match du_log_path() {
        Some(p) => p,
        None => return out,
    };
    let content = fs::read_to_string(&path).unwrap_or_default();
    // Walk only the tail to stay cheap on large logs.
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(4000);
    for line in &lines[start..] {
        if let Some(m) = parse_slice_line(line) {
            out.insert(m.slice_id(), m); // later line wins -> most recent
        }
    }
    out
}

/// G2 cross-slice admission rule, mirroring du_high_config_validator.cpp:1374:
/// Σ(min ratios) ≤ 100 AND Σ(dedicated ratios) ≤ 100, per direction. The dashboard
/// refuses to write a slice config the DU would reject at startup.
pub fn g2_ok(min_ratios: &[u32], ded_ratios: &[u32]) -> bool {
    min_ratios.iter().sum::<u32>() <= 100 && ded_ratios.iter().sum::<u32>() <= 100
}

// ---------------------------------------------------------------------------
// Cross-page context: a light, render-only snapshot the Configure / Advanced
// pages consume so they never depend on the Monitor page's internal UeView type.
// ---------------------------------------------------------------------------

/// A render-only health snapshot for one UE, built each frame by the Monitor loop
/// from its live UeView and handed to the other pages.
#[derive(Clone, Debug, Default)]
pub struct UeHealth {
    pub id: usize,
    pub stage: String,
    pub ip: String,
    pub has_tun: bool,
    pub last_rtt: Option<f64>,
    pub dl_mbps: f64,
    pub ul_mbps: f64,
}

/// Read-only data passed to a page's render() and handle_key() each frame.
pub struct PageCtx<'a> {
    pub ue_health: &'a [UeHealth],
    pub slice_map: &'a HashMap<usize, SliceMapEntry>,
    pub slice_metrics: &'a HashMap<SliceId, SliceMetric>,
    pub srv: &'a Option<String>,
    pub gnb_up: bool,
    pub scripts_dir: &'a str, // multi_ue_sim dir, for launching run_nue.sh from the Configure page
}

impl<'a> PageCtx<'a> {
    /// The slice a UE belongs to, from the slice map (None if unmapped).
    pub fn slice_of(&self, ue: usize) -> Option<SliceId> {
        self.slice_map.get(&ue).map(|e| e.slice_id())
    }

    /// All running UE ids that belong to a given slice.
    pub fn ues_in_slice(&self, slice: SliceId) -> Vec<usize> {
        self.ue_health
            .iter()
            .filter(|h| self.slice_of(h.id) == Some(slice))
            .map(|h| h.id)
            .collect()
    }

    /// Distinct slices currently present (from the slice map), sorted for stable display.
    pub fn distinct_slices(&self) -> Vec<SliceId> {
        let mut v: Vec<SliceId> = Vec::new();
        for h in self.ue_health {
            if let Some(s) = self.slice_of(h.id) {
                if !v.contains(&s) {
                    v.push(s);
                }
            }
        }
        v.sort_by_key(|s| (s.sst, s.sd.unwrap_or(u32::MAX)));
        v
    }
}

/// Fire-and-forget a background shell command (used by pages to spawn iperf / scripts),
/// mirroring the Monitor page's existing spawn_bg helper.
pub fn spawn_bg(cmd: String) {
    use std::process::{Command, Stdio};
    let _ = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_format() {
        // Exact format from scheduler_metrics_consumers.cpp:278-296, with a srsRAN prefix.
        let line = "2026-06-18T07:00:00.123 [SCHED ] [I] Scheduler slice pci=1 sst=1 sd=0x2 \
metrics: nof_ues=3 dl[min_prbs=26 max_prbs=106 ded_prbs=0] ul[min_prbs=26 max_prbs=106 ded_prbs=0] \
avg_dl_rbs_per_slot=64.20 avg_ul_rbs_per_slot=3.10 dl_prb_ratio=60.6% ul_prb_ratio=2.9%";
        let m = parse_slice_line(line).expect("should parse");
        assert_eq!(m.pci, 1);
        assert_eq!(m.sst, 1);
        assert_eq!(m.sd, Some(2));
        assert_eq!(m.nof_ues, 3);
        assert_eq!(m.dl_min_prbs, 26);
        assert_eq!(m.dl_max_prbs, 106);
        assert_eq!(m.dl_ded_prbs, 0);
        assert!((m.avg_dl_rbs_per_slot - 64.20).abs() < 1e-6);
        assert!((m.dl_prb_ratio - 60.6).abs() < 1e-3);
        assert!((m.ul_prb_ratio - 2.9).abs() < 1e-3);
    }

    #[test]
    fn parses_no_sd() {
        let line = "Scheduler slice pci=1 sst=2 metrics: nof_ues=0 dl[min_prbs=53 max_prbs=106 \
ded_prbs=0] ul[min_prbs=53 max_prbs=106 ded_prbs=0] avg_dl_rbs_per_slot=0.00 \
avg_ul_rbs_per_slot=0.00 dl_prb_ratio=0.0% ul_prb_ratio=0.0%";
        let m = parse_slice_line(line).expect("should parse");
        assert_eq!(m.sst, 2);
        assert_eq!(m.sd, None);
        assert_eq!(m.dl_min_prbs, 53);
    }

    #[test]
    fn rejects_non_slice_line() {
        assert!(parse_slice_line("[SCHED] some other log line").is_none());
    }

    #[test]
    fn cell_index_accepts_letter_or_number() {
        let mk = |c: &str| SliceMapEntry {
            sst: 1, sd: None, dnn: None, cell: Some(c.to_string()), imsi: None,
        };
        // letters
        assert_eq!(mk("A").cell_index(), 0);
        assert_eq!(mk("B").cell_index(), 1);
        // numeric index (the run_nue.sh UE_CELL form that caused "cell H")
        assert_eq!(mk("0").cell_index(), 0);
        assert_eq!(mk("1").cell_index(), 1);
        // normalised label always a letter
        assert_eq!(mk("0").cell_label(), "A");
        assert_eq!(mk("1").cell_label(), "B");
        // default when absent
        let none = SliceMapEntry { sst: 1, sd: None, dnn: None, cell: None, imsi: None };
        assert_eq!(none.cell_index(), 0);
        assert_eq!(none.cell_label(), "A");
    }

    #[test]
    fn sd_parsing() {
        assert_eq!(parse_sd("2"), Some(2));
        assert_eq!(parse_sd("0x2"), Some(2));
        assert_eq!(parse_sd("0xFF"), Some(255));
    }

    #[test]
    fn g2_rule() {
        assert!(g2_ok(&[25, 25, 25, 25], &[0, 0, 0, 0]));
        assert!(!g2_ok(&[50, 60], &[0, 0]));
        assert!(!g2_ok(&[10, 10], &[60, 60]));
    }
}