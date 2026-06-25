//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Forced-handover control for the ocudu (srsRAN) CU console — the ZMQ-friendly G8 test path.
//
// The CU exposes a console command (verified in apps/units/o_cu_cp/cu_cp/cu_cp_cmdline_commands.h):
//     ho <serving_pci> <rnti> <target_pci>
// which calls mobility_manager::trigger_handover(). With the G8 veto now wired into that path,
// a forced `ho` toward a cell that cannot serve the UE's slices is suppressed ("G8 forced-handover
// suppressed ...") and one toward a compatible cell proceeds.
//
// The dashboard delivers the command via a named pipe the CU reads as stdin. Launch the CU as:
//     mkfifo /tmp/ocu_console.fifo
//     sleep infinity > /tmp/ocu_console.fifo &        # holder: keeps the CU from seeing EOF
//     ./ocu -c cu_g1_g8.yaml < /tmp/ocu_console.fifo
// Then this module writes "ho <s> <rnti> <t>\n" into the FIFO.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Named pipe the CU reads commands from (see the launch recipe above).
pub const CU_FIFO: &str = "/tmp/ocu_console.fifo";

/// Root of the OCUDU tree (build/apps, configs/ live under it). Read from the
/// OCUDU_TREE env var; empty when unset — callers that need a concrete path
/// (e.g. launch_cu_fifo) surface the failure to the user.
pub fn ocudu_tree() -> String {
    std::env::var("OCUDU_TREE").unwrap_or_default()
}

/// Launch the OCUDU CU with a FIFO console entirely from the dashboard (no run_cu_fifo.sh):
/// create the FIFO, hold it open read-write so the CU's stdin never hits EOF, and exec the CU
/// reading it. Dir + config are derived from OCUDU_TREE; CU_CFG / CU_CPU override the defaults.
/// After this, the dashboard's `h` (send_ho) is delivered into the same FIFO.
pub fn launch_cu_fifo(status: Arc<Mutex<String>>) {
    let tree = ocudu_tree();
    let cfg = std::env::var("CU_CFG").unwrap_or_else(|_| format!("{tree}/configs/cu_g1_g8.yaml"));
    let cpu = std::env::var("CU_CPU").unwrap_or_else(|_| "1-2".to_string());
    let apps = format!("{tree}/build/apps");
    let cfg_short = cfg.rsplit('/').next().unwrap_or(&cfg).to_string();
    // pkill any prior CU, (re)create the FIFO, open a RW holder fd (9), then exec the CU with its
    // stdin = the FIFO. The exec'd CU inherits fd 9, so the FIFO stays open between dashboard writes.
    let script = format!(
        "pkill -9 -x ocu 2>/dev/null; sleep 1; rm -f {fifo}; mkfifo {fifo}; exec 9<>{fifo}; \
         cd {apps} || exit 1; \
         exec env MKL_THREADING_LAYER=GNU taskset -c {cpu} ./cu/ocu -c {cfg} < {fifo}",
        fifo = CU_FIFO, apps = apps, cpu = cpu, cfg = cfg);
    if let Ok(mut s) = status.lock() {
        *s = format!("launching CU: taskset -c {cpu} ./cu/ocu -c {cfg_short}  (FIFO {CU_FIFO}) …");
    }
    thread::spawn(move || {
        let log = std::fs::OpenOptions::new()
            .create(true).append(true).open("/tmp/ocu_cu_launch.log").ok();
        let (out, err) = match log {
            Some(f) => match f.try_clone() {
                Ok(f2) => (Stdio::from(f), Stdio::from(f2)),
                Err(_) => (Stdio::null(), Stdio::null()),
            },
            None => (Stdio::null(), Stdio::null()),
        };
        let res = Command::new("bash")
            .arg("-c").arg(&script)
            .stdin(Stdio::null())
            .stdout(out).stderr(err)
            .spawn();
        let msg = match res {
            Ok(_) => format!("CU starting (reading {CU_FIFO}); ~10s to reach AMF, then R/h work"),
            Err(e) => format!("CU launch failed: {e}"),
        };
        if let Ok(mut s) = status.lock() {
            *s = msg;
        }
    });
}

/// Among candidate paths, return the existing, non-empty one with the NEWEST mtime.
/// Picking by mtime (not first-existing) means a stale log left in an earlier-listed
/// location never shadows the one the running DU/CU is actually writing right now.
fn newest_nonempty(candidates: Vec<String>) -> Option<String> {
    candidates
        .into_iter()
        .filter_map(|p| {
            std::fs::metadata(&p).ok().and_then(|m| {
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

/// Candidate CU log locations (the CU's `log.filename`, resolved against its cwd). CU_LOG overrides.
pub fn cu_log_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("CU_LOG") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    if let Ok(tree) = std::env::var("OCUDU_TREE") {
        if !tree.is_empty() {
            v.push(format!("{}/build/apps/cu.log", tree));
            v.push(format!("{}/build/apps/cu/cu.log", tree));
            v.push(format!("{}/cu.log", tree));
        }
    }
    v.push("cu.log".to_string());
    v
}

pub fn cu_log_path() -> Option<String> {
    newest_nonempty(cu_log_candidates())
}

/// Extract an RNTI token (`0x….`) from a log line, ignoring 0x0000-style placeholders.
fn extract_rnti(line: &str) -> Option<String> {
    // Prefer the assigned C-RNTI over the temporary one when both appear.
    for key in ["c-rnti=", "crnti=", "rnti="] {
        if let Some(p) = line.find(key) {
            let rest = &line[p + key.len()..];
            let tok: String = rest
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == 'x' || *c == 'X')
                .collect();
            if let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
                if let Ok(v) = u32::from_str_radix(hex, 16) {
                    if v >= 0x10 {
                        return Some(format!("0x{:x}", v));
                    }
                }
            }
        }
    }
    None
}

/// Best-effort discovery of the most-recent UE C-RNTI from the CU log (then DU logs).
/// Returns None if nothing parseable is found — the UI then asks the user to type it.
pub fn discover_rnti() -> Option<String> {
    let mut paths: Vec<String> = Vec::new();
    if let Some(p) = cu_log_path() {
        paths.push(p);
    }
    paths.extend(crate::slice::du_log_candidates());
    for path in paths {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for line in content.lines().rev().take(4000) {
            if let Some(r) = extract_rnti(line) {
                return Some(r);
            }
        }
    }
    None
}

/// Send `ho <serving_pci> <rnti> <target_pci>` into the CU FIFO on a background thread (so the UI
/// never blocks if the CU isn't reading yet). Reports the outcome into `status`.
pub fn send_ho(serving_pci: u16, rnti: String, target_pci: u16, status: Arc<Mutex<String>>) {
    let cmd = format!("ho {} {} {}", serving_pci, rnti, target_pci);
    if let Ok(mut s) = status.lock() {
        *s = format!("sending: {cmd} …");
    }
    thread::spawn(move || {
        let res = std::fs::OpenOptions::new()
            .write(true)
            .open(CU_FIFO)
            .and_then(|mut f| writeln!(f, "{cmd}"));
        let msg = match res {
            Ok(_) => format!("sent: {cmd}  — watch the events pane"),
            Err(e) => format!("FIFO write failed ({e}). Is the CU running with  < {CU_FIFO} ?"),
        };
        if let Ok(mut s) = status.lock() {
            *s = msg;
        }
    });
}

/// Candidate target-DU (cell B) log locations. DU2_LOG overrides; falls back to the cell-A du.log
/// candidates from slice.rs so a single-DU run still shows handover events.
pub fn du2_log_candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("DU2_LOG") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    if let Ok(tree) = std::env::var("OCUDU_TREE") {
        if !tree.is_empty() {
            v.push(format!("{}/build/apps/du/du2.log", tree)); // DU runs from build/apps/du -> live log
            v.push(format!("{}/build/apps/du2.log", tree));
            v.push(format!("{}/du2.log", tree));
        }
    }
    v.push("du2.log".to_string());
    v
}

/// The target-DU (cell B) log: prefer a real du2 log; only fall back to the cell-A du.log
/// (single-DU runs) when no du2 log exists. Within each group, newest mtime wins.
pub fn du2_log_path() -> Option<String> {
    newest_nonempty(du2_log_candidates())
        .or_else(|| newest_nonempty(crate::slice::du_log_candidates()))
}

/// Tail the target DU's log for the radio-level handover markers (PRACH, RAR, Msg3, C-RNTI,
/// UE Configuration, RRC). This is what proves the UE actually arrived on the target cell.
pub fn du_ho_events(max: usize) -> Vec<String> {
    let path = match du2_log_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: Vec<String> = content
        .lines()
        .rev()
        .filter(|l| {
            // Drop indented ASN.1/JSON fragment lines (e.g. `"prach-RootSequenceIndex": {`) — keep events.
            if l.trim_start().starts_with('"') {
                return false;
            }
            let low = l.to_ascii_lowercase();
            (low.contains("prach") || low.contains("rar(") || low.contains("ra-rnti")
                || low.contains("msg3") || low.contains("c-rnti=0x") || low.contains("crnti=0x")
                || low.contains("ue configuration") || low.contains("ue creation")
                || low.contains("uecontextsetup") || low.contains("rrcreconfiguration"))
                // Per-slot scheduler echo ("Slot decisions ... RAR: ... msg3: ...") is noise —
                // we want the arrival markers (prach detection, UE creation, reconfiguration), not grants.
                && !low.contains("slot decisions")
                && !low.contains("metrics")
                && !low.contains("rach-config")
                && !low.contains("prach-config")
        })
        .take(max)
        .map(|s| match s.rfind("] ") {
            Some(i) => s[i + 2..].to_string(),
            None => s.to_string(),
        })
        .collect();
    out.reverse();
    out
}

/// Set a specific UE's gain on a specific cell (per-UE routing): /tmp/proxy_ue_gain_<ue>_<cell>.
pub fn set_ue_cell_gain(ue: usize, cell_idx: usize, gain: f64) -> std::io::Result<()> {
    let g = gain.clamp(0.0, 1.0);
    std::fs::write(format!("/tmp/proxy_ue_gain_{ue}_{cell_idx}"), format!("{g:.3}"))
}

/// Per-UE handover crossfade: make the target cell audible to THIS UE now, then fade its source
/// cell after `delay_ms` (once the HO command has reached it). Other UEs are untouched, so you can
/// move UE3 A→B while UE1/UE2 stay put.
///
/// NOTE: superseded by `handover_switch_on_reconfig` for the dashboard 'h' flow — that variant
/// keeps the source clean until the CU has actually sent the reconfiguration, instead of a blind
/// timer that could fade the source before the UE receives the handover command.
pub fn handover_crossfade_ue(ue: usize, source_idx: usize, target_idx: usize, delay_ms: u64) {
    let _ = set_ue_cell_gain(ue, target_idx, 1.0);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let _ = set_ue_cell_gain(ue, source_idx, 0.0);
    });
}

/// Current byte length of the live CU log (0 if absent) — used to watch only NEW lines.
fn cu_log_len() -> u64 {
    cu_log_path()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Has the CU logged a handover reconfiguration in the bytes appended since `baseline`?
fn cu_reconfig_after(baseline: u64) -> bool {
    let Some(p) = cu_log_path() else { return false };
    let content = std::fs::read_to_string(&p).unwrap_or_default();
    let tail = content.get(baseline as usize..).unwrap_or(content.as_str());
    let low = tail.to_ascii_lowercase();
    low.contains("reconfigurationwithsync")
        || low.contains("handover reconfiguration")
        || low.contains("intra cu handover")
}

/// Move UE `ue` from its source cell to the target cell — but only AFTER the CU has actually sent
/// the handover reconfiguration (reconfigurationWithSync). We snapshot the CU log, then poll its
/// tail; once the reconfig appears (or we give up), we settle briefly to let it reach the UE over
/// the air and THEN hard-switch the per-UE gains (target=1, source=0). This fixes two hazards of a
/// blind timer: (1) it never fades the source before the UE has the handover command (which causes
/// radio-link failure), and (2) the source cell stays clean (target muted) while the reconfig is
/// delivered, so there is no two-cell summing on the source. If no reconfig is observed (CU not on
/// the FIFO, G8 veto, etc.) the UE is LEFT on the source cell — a failed handover does not drop it.
pub fn handover_switch_on_reconfig(ue: usize, source_idx: usize, target_idx: usize) {
    let baseline = cu_log_len();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if cu_reconfig_after(baseline) {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !seen {
            return; // no reconfig delivered -> leave the UE on the source cell (no RLF)
        }
        // Settle: let the reconfig reach the UE over the air before fading the source.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = set_ue_cell_gain(ue, target_idx, 1.0);
        let _ = set_ue_cell_gain(ue, source_idx, 0.0);
    });
}

/// Make a handover target cell audible (or not) to the UE by writing its proxy gain file
/// /tmp/proxy_cell_gain_<idx>. idx 1 = the first extra cell (pci 2), idx 2 = pci 3, ...
/// The multi-cell proxy reads this live: 0.0 = inaudible (UE stays on cell A), 1.0 = fully
/// audible so the UE can detect that cell's SSB and RACH it during the handover.
pub fn set_cell_gain(cell_idx: usize, gain: f64) -> std::io::Result<()> {
    let g = gain.clamp(0.0, 1.0);
    std::fs::write(format!("/tmp/proxy_cell_gain_{cell_idx}"), format!("{g:.3}"))
}

/// Handover crossfade. Two cells summed at equal gain make the UE fail PBCH decode (their SSBs
/// overlap), so its Msg3 to the target never decodes. This makes the TARGET fully audible now and,
/// after `delay_ms` (enough for the reconfigurationWithSync to reach the UE via the source cell),
/// fades the SOURCE cell out so the UE gets a clean target to sync and RACH.
pub fn handover_crossfade(source_idx: usize, target_idx: usize, delay_ms: u64) {
    let _ = set_cell_gain(target_idx, 1.0); // target audible immediately
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let _ = set_cell_gain(source_idx, 0.0); // fade source after the HO command is delivered
    });
}

/// Tail the CU log for handover / G8 decision lines, most-recent last.
pub fn cu_ho_events(max: usize) -> Vec<String> {
    let path = match cu_log_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: Vec<String> = content
        .lines()
        .rev()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            low.contains("g8")
                || low.contains("handover")
                || low.contains("reconfigurationwithsync")
                || low.contains("rrc reconfiguration")
                || (low.contains("rrc") && low.contains("sync"))
        })
        .take(max)
        .map(|s| {
            // strip the leading timestamp/prefix to keep the pane readable
            match s.rfind("] ") {
                Some(i) => s[i + 2..].to_string(),
                None => s.to_string(),
            }
        })
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rnti_parse() {
        assert_eq!(extract_rnti("... ue=0 c-rnti=0x4601 cell=0 ..."), Some("0x4601".to_string()));
        assert_eq!(extract_rnti("rnti=0x4602 something"), Some("0x4602".to_string()));
        assert_eq!(extract_rnti("tc-rnti=0x4603"), Some("0x4603".to_string()));
        assert_eq!(extract_rnti("no rnti here"), None);
        assert_eq!(extract_rnti("rnti=0x0"), None); // placeholder ignored
    }
}
