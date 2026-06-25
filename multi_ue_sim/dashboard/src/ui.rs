//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Shared visual language for the 2.0 dashboard pages — a clean, modern CLI look
// (rounded panels, one cohesive accent, muted secondaries). Used by Configure and
// Advanced so they feel like one product.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

// ── palette (tokyonight-ish) ────────────────────────────────────────────────
pub const ACCENT: Color = Color::Rgb(125, 207, 255); // primary (light blue)
pub const ACCENT2: Color = Color::Rgb(187, 154, 247); // secondary (mauve)
pub const OK: Color = Color::Rgb(158, 206, 106); // green
pub const WARN: Color = Color::Rgb(224, 175, 104); // amber
pub const ERR: Color = Color::Rgb(247, 118, 142); // red/pink
pub const TEXT: Color = Color::Rgb(192, 202, 245); // primary text
pub const SUBTLE: Color = Color::Rgb(154, 165, 200); // secondary text
pub const MUTED: Color = Color::Rgb(96, 104, 144); // labels / hints
pub const SEL_BG: Color = Color::Rgb(41, 46, 66); // selected row bg
pub const FOCUS_BG: Color = Color::Rgb(54, 66, 110); // focused field bg
pub const BORDER: Color = Color::Rgb(60, 67, 99); // idle panel border
pub const CHIP_BG: Color = Color::Rgb(30, 34, 52);

/// A rounded panel with an accent title. `focused` brightens the border.
pub fn panel(title: &str, focused: bool) -> Block<'static> {
    let bcol = if focused { ACCENT } else { BORDER };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bcol))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { ACCENT } else { SUBTLE })
                .add_modifier(Modifier::BOLD),
        ))
}

/// `label value` line fragment helper (muted label + colored value).
pub fn kv<'a>(label: &'a str, value: impl Into<String>, vcolor: Color) -> Vec<Span<'a>> {
    vec![
        Span::styled(label, Style::default().fg(MUTED)),
        Span::styled(value.into(), Style::default().fg(vcolor).add_modifier(Modifier::BOLD)),
    ]
}

/// A small key-hint chip: `key`(accent) + `desc`(muted).
pub fn keyhint<'a>(key: &'a str, desc: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!(" {key} "), Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {desc}   "), Style::default().fg(SUBTLE)),
    ]
}

/// ●/○ status dot styled green/red.
pub fn dot(up: bool) -> Span<'static> {
    if up {
        Span::styled("●", Style::default().fg(OK))
    } else {
        Span::styled("○", Style::default().fg(ERR))
    }
}

pub fn s(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}
pub fn b(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color).add_modifier(Modifier::BOLD))
}
