//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Page 2 — Configure. Compose the per-UE plan (SST / SD / DNN / Cell) and LAUNCH the UEs
// straight from the dashboard. Slice + cell are launch-time properties of a UE, so this page
// builds the run_nue.sh environment (SD_LIST / DNN_LIST / SST_LIST / UE_CELL / HO_CELLn) and
// runs it for you. The right pane shows the live health of the selected UE.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
    Frame,
};

use crate::slice::{spawn_bg, PageCtx, SliceId};
use crate::ui;

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Sst,
    Sd,
    Dnn,
    Cell,
}

impl Field {
    fn next(self) -> Field {
        match self {
            Field::Sst => Field::Sd,
            Field::Sd => Field::Dnn,
            Field::Dnn => Field::Cell,
            Field::Cell => Field::Sst,
        }
    }
    fn prev(self) -> Field {
        match self {
            Field::Sst => Field::Cell,
            Field::Sd => Field::Sst,
            Field::Dnn => Field::Sd,
            Field::Cell => Field::Dnn,
        }
    }
}

fn cell_letter(idx: usize) -> char {
    (b'A' + idx as u8) as char
}

#[derive(Clone)]
struct PlanRow {
    ue_id: usize,
    sst: u8,
    sd: Option<u32>,
    dnn: String,
    cell: usize, // 0=A (primary gNB), 1=B (DU2), ...
}

impl PlanRow {
    fn suggested_dnn(sd: Option<u32>) -> String {
        match sd {
            Some(1) | None => "oai".to_string(),
            Some(n) => format!("oai{}", n),
        }
    }
}

pub struct ConfigureState {
    rows: Vec<PlanRow>,
    sel: usize,
    field: Field,
    status: String,
    initialised: bool,
    last_running: Vec<usize>,
    reflecting: bool,
}

impl ConfigureState {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            sel: 0,
            field: Field::Sd,
            status: "compose each UE's slice + cell, then  r  to launch them".into(),
            initialised: false,
            last_running: Vec::new(),
            reflecting: false,
        }
    }

    fn ensure_init(&mut self, ctx: &PageCtx) {
        if self.initialised {
            return;
        }
        let mut ids: Vec<usize> = ctx.slice_map.keys().copied().collect();
        ids.sort_unstable();
        if ids.is_empty() {
            self.rows = vec![
                PlanRow { ue_id: 1, sst: 1, sd: Some(1), dnn: "oai".into(), cell: 0 },
                PlanRow { ue_id: 2, sst: 1, sd: Some(2), dnn: "oai2".into(), cell: 1 },
            ];
        } else {
            self.rows = ids
                .into_iter()
                .map(|id| {
                    let e = &ctx.slice_map[&id];
                    let cell = e.cell_label().chars().next().map(|c| (c as u8).wrapping_sub(b'A') as usize).unwrap_or(0);
                    PlanRow {
                        ue_id: id,
                        sst: e.sst,
                        sd: e.sd_num(),
                        dnn: e.dnn.clone().unwrap_or_else(|| PlanRow::suggested_dnn(e.sd_num())),
                        cell: cell.min(7),
                    }
                })
                .collect();
        }
        self.initialised = true;
    }

    /// When UEs are actually running (launched by the script), reflect them here with the real
    /// SST/SD detected from each UE's log, instead of an editable launch plan. Rebuilds only when
    /// the running set changes, so it stays cheap and doesn't fight the cursor.
    fn sync_running(&mut self, ctx: &PageCtx) {
        let mut running: Vec<usize> = ctx.ue_health.iter().map(|h| h.id).collect();
        running.sort_unstable();

        if running.is_empty() {
            // Nothing running -> (re)build the editable launch plan.
            if self.reflecting {
                self.initialised = false; // UEs stopped: drop the reflected rows
            }
            self.reflecting = false;
            self.last_running.clear();
            self.ensure_init(ctx);
            return;
        }

        self.reflecting = true;
        if running == self.last_running && !self.rows.is_empty() {
            return; // running set unchanged -> keep current rows
        }
        self.last_running = running.clone();

        let scripts_dir = ctx.scripts_dir;
        self.rows = running
            .into_iter()
            .map(|id| {
                let (sst, sd) = detect_ue_snssai(scripts_dir, id).unwrap_or((1, None));
                let cell = ctx.slice_map.get(&id).map(|e| e.cell_index()).unwrap_or(0);
                PlanRow { ue_id: id, sst, sd, dnn: PlanRow::suggested_dnn(sd), cell: cell.min(7) }
            })
            .collect();
        self.initialised = true;
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, ctx: &PageCtx) -> bool {
        let n = self.rows.len();
        match key.code {
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                if self.sel + 1 < n {
                    self.sel += 1;
                }
            }
            KeyCode::Left => self.field = self.field.prev(),
            KeyCode::Right | KeyCode::Char('\t') => self.field = self.field.next(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.bump(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.bump(-1),
            KeyCode::Char(c @ '0'..='9') => return self.set_digit(c.to_digit(10).unwrap()),
            KeyCode::Char('a') => {
                let next_id = self.rows.iter().map(|r| r.ue_id).max().unwrap_or(0) + 1;
                self.rows.push(PlanRow { ue_id: next_id, sst: 1, sd: Some(1), dnn: "oai".into(), cell: 0 });
                self.sel = self.rows.len() - 1;
                self.status = format!("added UE{next_id}");
            }
            KeyCode::Char('d') => {
                if self.rows.len() > 1 {
                    let removed = self.rows.remove(self.sel.min(self.rows.len() - 1));
                    self.sel = self.sel.min(self.rows.len().saturating_sub(1));
                    self.status = format!("removed UE{}", removed.ue_id);
                }
            }
            KeyCode::Char('r') | KeyCode::Enter => self.launch(ctx),
            KeyCode::Char('k') => {
                spawn_bg(format!("bash {}/dash_ctl.sh stop", ctx.scripts_dir));
                self.status = "stopping all UEs + proxy…".into();
            }
            _ => return false,
        }
        true
    }

    fn set_digit(&mut self, d: u32) -> bool {
        let sel = self.sel;
        let field = self.field;
        let (consumed, msg) = {
            let Some(row) = self.rows.get_mut(sel) else { return false };
            match field {
                Field::Sst if (1..=4).contains(&d) => {
                    row.sst = d as u8;
                    (true, format!("UE{} SST={}", row.ue_id, d))
                }
                Field::Sd => {
                    row.sd = if d == 0 { None } else { Some(d) };
                    row.dnn = PlanRow::suggested_dnn(row.sd);
                    (true, format!("UE{} SD={}", row.ue_id, if d == 0 { "none".into() } else { format!("sd{d}") }))
                }
                Field::Cell if d <= 7 => {
                    row.cell = d as usize;
                    (true, format!("UE{} cell={}", row.ue_id, cell_letter(d as usize)))
                }
                _ => (false, String::new()),
            }
        };
        if consumed {
            self.status = msg;
        }
        consumed
    }

    fn bump(&mut self, dir: i32) {
        let Some(row) = self.rows.get_mut(self.sel) else { return };
        match self.field {
            Field::Sst => row.sst = (row.sst as i32 + dir).clamp(1, 4) as u8,
            Field::Sd => {
                let v = (row.sd.unwrap_or(0) as i32 + dir).clamp(0, 8);
                row.sd = if v == 0 { None } else { Some(v as u32) };
                row.dnn = PlanRow::suggested_dnn(row.sd);
            }
            Field::Dnn => {
                let cur: u32 = row.dnn.trim_start_matches("oai").parse().unwrap_or(1);
                let v = ((cur as i32 - 1 + dir).rem_euclid(4) + 1) as u32;
                row.dnn = if v == 1 { "oai".into() } else { format!("oai{v}") };
            }
            Field::Cell => row.cell = (row.cell as i32 + dir).rem_euclid(4) as usize,
        }
    }

    /// Compose the run_nue.sh environment from the plan and launch the UEs.
    fn launch(&mut self, ctx: &PageCtx) {
        if !ctx.gnb_up {
            self.status = "gNB is DOWN — start the CU + DU(s) first, then press r".into();
            return;
        }
        let n = self.rows.len();
        let join = |f: &dyn Fn(&PlanRow) -> String| -> String {
            self.rows.iter().map(f).collect::<Vec<_>>().join(",")
        };
        let sst = join(&|r| r.sst.to_string());
        let sd = join(&|r| r.sd.map(|s| s.to_string()).unwrap_or_default());
        let dnn = join(&|r| r.dnn.clone());
        let cells = join(&|r| r.cell.to_string());

        // HO_CELL<n> for each EXTRA cell (idx>0) the plan uses -> the proxy bridges those DUs.
        let mut extra: Vec<usize> = self.rows.iter().map(|r| r.cell).filter(|&c| c > 0).collect();
        extra.sort_unstable();
        extra.dedup();
        let mut ho = String::new();
        for c in extra {
            ho.push_str(&format!(
                " HO_CELL{}=\"tcp://127.0.0.1:{},tcp://127.0.0.1:{}\"",
                c + 1, 4556 + 2 * c, 4557 + 2 * c
            ));
        }
        let cmd = format!(
            "SST_LIST={sst} SD_LIST={sd} DNN_LIST={dnn} UE_CELL={cells} N_RB=106{ho} \
             bash {}/run_nue.sh {n}",
            ctx.scripts_dir
        );
        spawn_bg(cmd);
        self.status = format!("launching {n} UE(s)  SD_LIST={sd}  UE_CELL={cells}  (watch Monitor page)");
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, ctx: &PageCtx) {
        self.sync_running(ctx);
        if !self.rows.is_empty() && self.sel >= self.rows.len() {
            self.sel = self.rows.len() - 1;
        }

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(area);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(4), Constraint::Length(1)])
            .split(cols[0]);

        self.render_plan(f, left[0]);
        self.render_launch_bar(f, left[1], ctx);
        self.render_status(f, left[2]);
        self.render_health(f, cols[1], ctx);
    }

    fn render_plan(&self, f: &mut Frame, area: Rect) {
        let hdr = Row::new(["UE", "SST", "SD", "DNN", "CELL", "SLICE"]).style(
            Style::default().fg(ui::SUBTLE).add_modifier(Modifier::BOLD),
        );
        let focus = Style::default().bg(ui::FOCUS_BG).fg(ui::TEXT).add_modifier(Modifier::BOLD);
        let rows: Vec<Row> = self.rows.iter().enumerate().map(|(i, r)| {
            let on = i == self.sel;
            let sid = SliceId::new(r.sst, r.sd);
            let sd_s = r.sd.map(|s| format!("sd{s}")).unwrap_or_else(|| "—".into());
            let fc = |fl: Field, t: String| if on && self.field == fl { Cell::from(t).style(focus) } else { Cell::from(t) };
            let row = Row::new(vec![
                Cell::from(format!("UE{}", r.ue_id)).style(Style::default().fg(ui::TEXT)),
                fc(Field::Sst, r.sst.to_string()),
                fc(Field::Sd, sd_s),
                fc(Field::Dnn, r.dnn.clone()),
                fc(Field::Cell, format!("{}", cell_letter(r.cell))),
                Cell::from(format!("{} {}", sid.short(), sid.service())).style(Style::default().fg(slice_color(sid))),
            ]);
            if on { row.style(Style::default().bg(ui::SEL_BG)) } else { row }
        }).collect();

        let widths = [Constraint::Length(5), Constraint::Length(4), Constraint::Length(5),
                      Constraint::Length(7), Constraint::Length(5), Constraint::Min(12)];
        let title = if self.reflecting {
            "Running UEs  ·  ↑↓ select  (SST/SD detected from each UE log)"
        } else {
            "Plan  ·  ↑↓ row   ←→ field   0-9 / +- set   a add   d del"
        };
        let table = Table::new(rows, widths).header(hdr).block(ui::panel(title, true));
        f.render_widget(table, area);
    }

    fn render_launch_bar(&self, f: &mut Frame, area: Rect, ctx: &PageCtx) {
        let n = self.rows.len();
        let sd = self.rows.iter().map(|r| r.sd.map(|s| s.to_string()).unwrap_or_default()).collect::<Vec<_>>().join(",");
        let cells = self.rows.iter().map(|r| cell_letter(r.cell).to_string()).collect::<Vec<_>>().join(",");
        let mut lines = vec![
            Line::from(vec![
                Span::raw(" "), ui::dot(ctx.gnb_up),
                ui::b(format!(" gNB {}   ", if ctx.gnb_up { "up" } else { "down" }), if ctx.gnb_up { ui::OK } else { ui::ERR }),
                ui::s(format!("plan: {n} UE   SD=[{sd}]   cells=[{cells}]"), ui::SUBTLE),
            ]),
            Line::from({
                let mut sp = ui::keyhint("r", "launch UEs");
                sp.extend(ui::keyhint("k", "stop"));
                sp.extend(ui::keyhint("3", "handover →"));
                sp
            }),
        ];
        if !ctx.gnb_up {
            lines.push(Line::from(ui::s("  start the CU + DU(s) first", ui::WARN)));
        }
        f.render_widget(Paragraph::new(lines).block(ui::panel("Launch", false)), area);
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        f.render_widget(
            Paragraph::new(Line::from(ui::s(format!(" {}", self.status), ui::ACCENT))),
            area,
        );
    }

    fn render_health(&self, f: &mut Frame, area: Rect, ctx: &PageCtx) {
        let row = self.rows.get(self.sel);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(r) = row {
            let sid = SliceId::new(r.sst, r.sd);
            let health = ctx.ue_health.iter().find(|h| h.id == r.ue_id);
            lines.push(Line::from(ui::b(format!(" UE {}", r.ue_id), ui::TEXT)));
            lines.push(Line::from(ui::kv("  slice : ", format!("{} ({})", sid.short(), sid.service()), slice_color(sid))));
            lines.push(Line::from(ui::kv("  cell  : ", format!("{}  (pci {})", cell_letter(r.cell), r.cell + 1), ui::ACCENT2)));
            lines.push(Line::from(ui::kv("  dnn   : ", r.dnn.clone(), ui::TEXT)));
            if let Some(imsi) = ctx.slice_map.get(&r.ue_id).and_then(|e| e.imsi.clone()) {
                lines.push(Line::from(ui::kv("  imsi  : ", imsi, ui::MUTED)));
            }
            lines.push(Line::from(""));
            match health {
                Some(h) => {
                    let c = if h.has_tun { ui::OK } else { ui::WARN };
                    lines.push(Line::from(ui::kv("  stage : ", h.stage.clone(), c)));
                    lines.push(Line::from(ui::kv("  ip    : ", h.ip.clone(), c)));
                    lines.push(Line::from(ui::kv("  rtt   : ", h.last_rtt.map(|r| format!("{r:.0} ms")).unwrap_or_else(|| "—".into()), ui::WARN)));
                    lines.push(Line::from(ui::kv("  dl/ul : ", format!("{:.2} / {:.2} Mbps", h.dl_mbps, h.ul_mbps), ui::ACCENT)));
                }
                None => lines.push(Line::from(ui::s("  (planned — not running yet)", ui::MUTED))),
            }
        } else {
            lines.push(Line::from(ui::s("  (no UE selected)", ui::MUTED)));
        }
        f.render_widget(Paragraph::new(lines).block(ui::panel("Health", false)), area);
    }
}

/// Stable per-slice color for tables/charts.
pub fn slice_color(s: SliceId) -> ratatui::style::Color {
    use ratatui::style::Color;
    const PALETTE: [Color; 6] = [
        Color::Rgb(125, 207, 255),
        Color::Rgb(224, 175, 104),
        Color::Rgb(158, 206, 106),
        Color::Rgb(187, 154, 247),
        Color::Rgb(115, 218, 202),
        Color::Rgb(247, 118, 142),
    ];
    let key = s.sst as usize + s.sd.unwrap_or(0) as usize;
    PALETTE[key % PALETTE.len()]
}

/// Detect a running UE's S-NSSAI (SST, optional SD) from its launch log. The OAI UE echoes its
/// full command line ("CMDLINE: ... nssai_sst N ... nssai_sd N ...") at startup, so this works
/// regardless of which script launched it. Returns None if the log is missing/unparseable.
fn detect_ue_snssai(scripts_dir: &str, ue_id: usize) -> Option<(u8, Option<u32>)> {
    let path = format!("{}/logs/ue{}_nue.log", scripts_dir, ue_id);
    let content = std::fs::read_to_string(&path).ok()?;
    let sst = num_after(&content, "nssai_sst").unwrap_or(1) as u8;
    let sd = num_after(&content, "nssai_sd");
    Some((sst, sd))
}

/// First integer that appears after `key` in `haystack` (e.g. key="nssai_sd" in `... nssai_sd" "2"`).
fn num_after(haystack: &str, key: &str) -> Option<u32> {
    let pos = haystack.find(key)?;
    let rest = &haystack[pos + key.len()..];
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    rest[start..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}