//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Page 3 — Advanced (Handover). Pick which UE to move and which cell to move it to, fire the
// G8-checked inter-DU handover, and watch it land — CU-side (RRC) and DU-side (radio) handover
// logs are shown live, side by side. Log file paths are configurable via env:
//   CU_LOG (cu.log) · DU2_LOG (target DU log) · DU_LOG/OCUDU_TREE (fallbacks).

use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::configure::slice_color;
use crate::handover;
use crate::slice::{PageCtx, SliceId};
use crate::ui;

fn cell_letter(idx: usize) -> char {
    (b'A' + idx as u8) as char
}

/// Compact an absolute log path to the last few components so it fits a narrow panel title,
/// e.g. /home/.../ocudu/build/apps/du/du2.log -> …/build/apps/du/du2.log
fn compact_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 4 {
        return p.to_string();
    }
    format!("…/{}", parts[parts.len() - 4..].join("/"))
}

pub struct AdvancedState {
    sel_ue_idx: usize,  // index into the running-UE list
    target_cell: usize, // 0=A, 1=B, ...
    rnti: String,
    status: String,
    ho_status: Arc<Mutex<String>>,
}

impl AdvancedState {
    pub fn new() -> Self {
        Self {
            sel_ue_idx: 0,
            target_cell: 1,
            rnti: String::new(),
            status: "c start CU   ↑↓ pick UE   </> target cell   R detect rnti   h handover".into(),
            ho_status: Arc::new(Mutex::new("idle — pick a UE and a target cell, then h".into())),
        }
    }

    /// The cell a UE currently sits on, from the slice map (A=0 default). Robust to the cell being
    /// recorded as a letter ("A") or a numeric index ("0") — see SliceMapEntry::cell_index.
    fn ue_cell(ctx: &PageCtx, ue: usize) -> usize {
        ctx.slice_map.get(&ue).map(|e| e.cell_index()).unwrap_or(0).min(7)
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &PageCtx) -> bool {
        let ue_ids: Vec<usize> = ctx.ue_health.iter().map(|h| h.id).collect();
        match key.code {
            KeyCode::Up => self.sel_ue_idx = self.sel_ue_idx.saturating_sub(1),
            KeyCode::Down => {
                if self.sel_ue_idx + 1 < ue_ids.len() {
                    self.sel_ue_idx += 1;
                }
            }
            KeyCode::Char('>') | KeyCode::Char('.') => self.target_cell = (self.target_cell + 1).min(7),
            KeyCode::Char('<') | KeyCode::Char(',') => self.target_cell = self.target_cell.saturating_sub(1),
            KeyCode::Char('R') | KeyCode::Char('r') => {
                self.rnti = handover::discover_rnti().unwrap_or_default();
                let s = if self.rnti.is_empty() {
                    "no RNTI found — is a UE attached? (check Monitor)".to_string()
                } else {
                    format!("detected rnti={}", self.rnti)
                };
                if let Ok(mut g) = self.ho_status.lock() { *g = s; }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                handover::launch_cu_fifo(self.ho_status.clone());
            }
            KeyCode::Char('h') | KeyCode::Enter => self.do_handover(ctx, &ue_ids),
            _ => return false,
        }
        true
    }

    fn do_handover(&mut self, ctx: &PageCtx, ue_ids: &[usize]) {
        let Some(&ue) = ue_ids.get(self.sel_ue_idx) else {
            self.status = "no UE selected".into();
            return;
        };
        if self.rnti.is_empty() {
            self.rnti = handover::discover_rnti().unwrap_or_default();
        }
        if self.rnti.is_empty() {
            if let Ok(mut g) = self.ho_status.lock() { *g = "no RNTI — press R / attach the UE first".into(); }
            return;
        }
        let source_cell = Self::ue_cell(ctx, ue);
        if self.target_cell == source_cell {
            self.status = format!("UE{ue} already on cell {} — pick a different target (</>)", cell_letter(source_cell));
            return;
        }
        let serving_pci = (source_cell + 1) as u16;
        let target_pci = (self.target_cell + 1) as u16;
        // Deliver the handover command to the CU FIRST — it sends reconfigurationWithSync to the UE
        // via the SOURCE cell. The switch then waits for that reconfig to appear in the CU log
        // before moving the UE to the target, so we never fade the source out from under an
        // undelivered reconfig (which would drop the UE), and the source stays clean (target muted)
        // while the reconfig is delivered. If no reconfig appears, the UE is left on the source.
        handover::send_ho(serving_pci, self.rnti.clone(), target_pci, self.ho_status.clone());
        handover::handover_switch_on_reconfig(ue, source_cell, self.target_cell);
        self.status = format!(
            "UE{ue}: cell {} → {}   ho {} {} {}",
            cell_letter(source_cell), cell_letter(self.target_cell), serving_pci, self.rnti, target_pci
        );
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, ctx: &PageCtx) {
        let ue_ids: Vec<usize> = ctx.ue_health.iter().map(|h| h.id).collect();
        if !ue_ids.is_empty() && self.sel_ue_idx >= ue_ids.len() {
            self.sel_ue_idx = ue_ids.len() - 1;
        }

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(9), Constraint::Min(6), Constraint::Length(1)])
            .split(area);

        self.render_control(f, rows[0], ctx, &ue_ids);

        let logs = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        self.render_cu_log(f, logs[0]);
        self.render_du_log(f, logs[1]);

        f.render_widget(
            Paragraph::new(Line::from(ui::s(format!(" {}", self.status), ui::MUTED))),
            rows[2],
        );
    }

    fn render_control(&self, f: &mut Frame, area: Rect, ctx: &PageCtx, ue_ids: &[usize]) {
        let inner = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        // --- UE picker ---
        let mut picker: Vec<Line> = Vec::new();
        if ue_ids.is_empty() {
            picker.push(Line::from(ui::s("  no UEs running", ui::MUTED)));
            picker.push(Line::from(ui::s("  launch from the Configure page (2)", ui::MUTED)));
        } else {
            for (i, &id) in ue_ids.iter().enumerate() {
                let on = i == self.sel_ue_idx;
                let cell = Self::ue_cell(ctx, id);
                let sid = ctx.slice_map.get(&id).map(|e| e.slice_id()).unwrap_or(SliceId::new(0, None));
                let marker = if on { "►" } else { " " };
                let st = if on {
                    Style::default().fg(ui::TEXT).bg(ui::SEL_BG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(ui::SUBTLE)
                };
                picker.push(Line::from(vec![
                    Span::styled(format!(" {marker} UE{id} ", ), st),
                    Span::styled(format!("{} ", sid.short()), Style::default().fg(slice_color(sid))),
                    ui::s(format!("on cell {}", cell_letter(cell)), ui::MUTED),
                ]));
            }
        }
        f.render_widget(Paragraph::new(picker).block(ui::panel("UE  (↑↓)", true)), inner[0]);

        // --- handover spec ---
        let sel_ue = ue_ids.get(self.sel_ue_idx).copied();
        let src = sel_ue.map(|u| Self::ue_cell(ctx, u)).unwrap_or(0);
        let rnti_disp = if self.rnti.is_empty() { "— (R to detect)".to_string() } else { self.rnti.clone() };
        let cmd = format!("ho {} {} {}", src + 1,
            if self.rnti.is_empty() { "<rnti>".into() } else { self.rnti.clone() }, self.target_cell + 1);
        let ho_status = self.ho_status.lock().map(|s| s.clone()).unwrap_or_default();
        let spec: Vec<Line> = vec![
            Line::from([ui::kv("  move    : ", sel_ue.map(|u| format!("UE{u}")).unwrap_or_else(|| "—".into()), ui::TEXT),
                        ui::kv("   from cell ", cell_letter(src).to_string(), ui::OK)].concat()),
            Line::from(ui::kv("  to cell : ", format!("{}  (pci {})   ", cell_letter(self.target_cell), self.target_cell + 1), ui::ACCENT)
                .into_iter().chain(std::iter::once(ui::s("</> to change", ui::MUTED))).collect::<Vec<_>>()),
            Line::from(ui::kv("  rnti    : ", rnti_disp, ui::ACCENT2)),
            Line::from(ui::kv("  command : ", cmd, ui::WARN)),
            Line::from(""),
            Line::from({ let mut s = ui::keyhint("c", "start CU"); s.extend(ui::keyhint("h", "handover")); s.extend(ui::keyhint("R", "detect rnti")); s.extend(ui::keyhint("</>", "target")); s }),
            Line::from(ui::s(format!("  {ho_status}"), ui::SUBTLE)),
        ];
        f.render_widget(Paragraph::new(spec).block(ui::panel("Handover", false)), inner[1]);
    }

    fn render_cu_log(&self, f: &mut Frame, area: Rect) {
        let h = (area.height as usize).saturating_sub(2);
        let events = handover::cu_ho_events(h);
        let lines: Vec<Line> = if events.is_empty() {
            vec![Line::from(ui::s(" (no handover lines yet — fire one with h)", ui::MUTED))]
        } else {
            events.into_iter().map(|e| {
                let low = e.to_ascii_lowercase();
                let c = if low.contains("suppress") || low.contains("fail") { ui::ERR }
                        else if low.contains("complete") || low.contains("finished successfully") || low.contains("g8") { ui::OK }
                        else { ui::SUBTLE };
                Line::from(ui::s(e, c))
            }).collect()
        };
        let p = format!("CU handover  ·  {}", compact_path(&handover::cu_log_path().unwrap_or_else(|| "cu.log".into())));
        f.render_widget(Paragraph::new(lines).block(ui::panel(&p, false)), area);
    }

    fn render_du_log(&self, f: &mut Frame, area: Rect) {
        let h = (area.height as usize).saturating_sub(2);
        let events = handover::du_ho_events(h);
        let lines: Vec<Line> = if events.is_empty() {
            vec![Line::from(ui::s(" (no radio handover lines yet)", ui::MUTED))]
        } else {
            events.into_iter().map(|e| {
                let low = e.to_ascii_lowercase();
                let c = if low.contains("prach") || low.contains("rar") { ui::ACCENT }
                        else if low.contains("crnti=0x") || low.contains("c-rnti=0x") || low.contains("finished successfully") { ui::OK }
                        else if low.contains("fail") || low.contains("nok") { ui::ERR }
                        else { ui::SUBTLE };
                Line::from(ui::s(e, c))
            }).collect()
        };
        let p = format!("DU radio  ·  {}", compact_path(&handover::du2_log_path().unwrap_or_else(|| "du2.log".into())));
        f.render_widget(Paragraph::new(lines).block(ui::panel(&p, false)), area);
    }
}