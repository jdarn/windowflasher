//! Rendering.

use crate::app::{App, Screen, DOWNLOAD_URL};
use crate::devices::human_size;
use crate::installer::STAGES;
use ratatui::{prelude::*, widgets::*};

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let iso = app
        .iso_name
        .as_deref()
        .map(|n| format!("ISO: {n}"))
        .unwrap_or_else(|| "No ISO selected".to_string());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Windows 11 USB Creator", Style::new().bold().fg(Color::Cyan)),
            Span::styled(format!("   {iso}"), Style::new().dim()),
        ]))
        .block(Block::bordered()),
        header,
    );

    let cwd = app.cwd.display().to_string();
    let show_all = app.show_all;
    let help;
    match &mut app.screen {
        Screen::NoIso { note } => {
            help = "R/Enter rescan   Q quit";
            let mut lines = vec![
                Line::styled("No Windows 11 ISO found.", Style::new().bold().yellow()),
                Line::raw(""),
                Line::raw("1. Download the ISO from:"),
                Line::styled(
                    format!("   {DOWNLOAD_URL}"),
                    Style::new().fg(Color::Cyan).underlined(),
                ),
                Line::raw("   (choose \"Download Windows 11 Disk Image (ISO)\")"),
                Line::raw(""),
                Line::raw("2. Put the .iso file in the same folder as this program:"),
                Line::styled(format!("   {cwd}"), Style::new().bold()),
                Line::raw(""),
                Line::raw("3. Press R to scan again."),
            ];
            if let Some(n) = note {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("Problem: {n}"), Style::new().fg(Color::Red)));
            }
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Windows 11 ISO ")),
                body,
            );
        }
        Screen::PickIso { isos, state } => {
            help = "↑/↓ move   Enter select   R rescan   Q quit";
            let items: Vec<ListItem> = isos
                .iter()
                .map(|p| {
                    ListItem::new(
                        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    )
                })
                .collect();
            let list = List::new(items)
                .block(Block::bordered().title(" Several ISOs found: pick one "))
                .highlight_style(Style::new().reversed().bold())
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, body, state);
        }
        Screen::Devices { devices, state, note, why } => {
            help = if show_all {
                "↑/↓ move   Enter select   R rescan   A hide SSD/HDD-like disks   Q quit"
            } else {
                "↑/↓ move   Enter select   R rescan   A show all USB disks   Q quit"
            };
            let mut info: Vec<Line> = Vec::new();
            if let Some(n) = note.as_ref() {
                info.push(Line::styled(n.clone(), Style::new().fg(Color::Red)));
            }
            for w in why.iter().take(6) {
                info.push(Line::styled(w.clone(), Style::new().dim()));
            }
            if why.len() > 6 {
                info.push(Line::styled(
                    format!("...and {} more", why.len() - 6),
                    Style::new().dim(),
                ));
            }
            let info_h = if info.is_empty() { 0 } else { info.len() as u16 + 2 };
            let [list_area, info_area] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(info_h)]).areas(body);
            if devices.is_empty() {
                f.render_widget(
                    Paragraph::new(vec![
                        Line::styled("No USB flash drives found.", Style::new().bold().yellow()),
                        Line::raw(""),
                        Line::raw("Insert a USB flash drive and press R to rescan."),
                        Line::styled(
                            "Internal disks are never listed. Press A to also show USB disks that might be SSD/HDD.",
                            Style::new().dim(),
                        ),
                    ])
                    .block(Block::bordered().title(" USB flash drives ")),
                    list_area,
                );
            } else {
                let items: Vec<ListItem> = devices
                    .iter()
                    .map(|d| {
                        ListItem::new(Line::from(vec![
                            Span::styled(format!("{:<32}", d.label), Style::new().bold()),
                            Span::styled(
                                format!("{:>9}   {}", human_size(d.size), d.path),
                                Style::new().dim(),
                            ),
                            Span::styled(
                                if d.warn.is_some() { "   ⚠ maybe an SSD/HDD" } else { "" },
                                Style::new().fg(Color::Yellow),
                            ),
                        ]))
                    })
                    .collect();
                let list = List::new(items)
                    .block(Block::bordered().title(" Select a USB flash drive "))
                    .highlight_style(Style::new().reversed())
                    .highlight_symbol("▶ ");
                f.render_stateful_widget(list, list_area, state);
            }
            if info_h > 0 {
                let title = if devices.is_empty() {
                    " Why nothing is listed "
                } else {
                    " Hidden or ignored "
                };
                f.render_widget(
                    Paragraph::new(info)
                        .wrap(Wrap { trim: true })
                        .block(Block::bordered().title(title)),
                    info_area,
                );
            }
        }
        Screen::Confirm { device, yes } => {
            help = "←/→ choose   Enter confirm   N/Esc cancel";
            let sel = Style::new().reversed().bold();
            let (yes_style, no_style) = if *yes {
                (sel.fg(Color::Red), Style::new())
            } else {
                (Style::new(), sel.fg(Color::Green))
            };
            let mut lines = vec![
                Line::styled("⚠  WARNING: ALL DATA WILL BE DELETED", Style::new().bold().fg(Color::Red)),
                Line::raw(""),
                Line::raw(format!(
                    "Everything on {} ({}, {}) will be permanently erased.",
                    device.label,
                    human_size(device.size),
                    device.path
                )),
                Line::raw("This cannot be undone."),
                Line::raw(""),
                Line::raw("Continue?"),
                Line::raw(""),
                Line::from(vec![
                    Span::styled("  Yes  ", yes_style),
                    Span::raw("    "),
                    Span::styled("  No  ", no_style),
                ]),
            ];
            if let Some(w) = &device.warn {
                lines.insert(
                    4,
                    Line::styled(
                        format!("Check this is the right drive: {w}."),
                        Style::new().fg(Color::Yellow).bold(),
                    ),
                );
            }
            f.render_widget(
                Paragraph::new(lines)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true })
                    .block(Block::bordered().border_style(Style::new().fg(Color::Red)).title(" Confirm ")),
                body,
            );
        }
        Screen::Installing(st) => {
            help = "Working... do not unplug the drive";
            let [stages_area, gauge_area, status_area, log_area] = Layout::vertical([
                Constraint::Length(STAGES.len() as u16 + 2),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(3),
            ])
            .areas(body);

            let lines: Vec<Line> = STAGES
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    if i < st.stage {
                        Line::styled(format!("✔ {name}"), Style::new().fg(Color::Green))
                    } else if i == st.stage {
                        Line::styled(format!("▶ {name}"), Style::new().fg(Color::Yellow).bold())
                    } else {
                        Line::styled(format!("  {name}"), Style::new().dim())
                    }
                })
                .collect();
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered().title(format!(" Writing {} ({}) ", st.device.label, st.device.path)),
                ),
                stages_area,
            );

            let pct = (st.progress.clamp(0.0, 1.0) * 100.0).round();
            f.render_widget(
                Gauge::default()
                    .block(Block::bordered())
                    .gauge_style(Style::new().fg(Color::Cyan))
                    .ratio(st.progress.clamp(0.0, 1.0))
                    .label(format!("{pct:.0}%")),
                gauge_area,
            );
            f.render_widget(Paragraph::new(st.status.clone()).style(Style::new().dim()), status_area);

            let h = log_area.height.saturating_sub(2) as usize;
            let start = st.log.len().saturating_sub(h);
            let log_lines: Vec<Line> = st.log[start..].iter().map(|l| Line::raw(l.clone())).collect();
            f.render_widget(
                Paragraph::new(log_lines).block(Block::bordered().title(" Log ")),
                log_area,
            );
        }
        Screen::Done { ok, message, log } => {
            help = "Enter/Q quit";
            let color = if *ok { Color::Green } else { Color::Red };
            let title = if *ok { " Done " } else { " Failed " };
            let mut lines = vec![
                Line::styled(
                    if *ok { "✔ Success" } else { "✘ Error" },
                    Style::new().bold().fg(color),
                ),
                Line::raw(""),
            ];
            lines.extend(message.lines().map(|l| Line::raw(l.to_string())));
            if !*ok {
                lines.push(Line::raw(""));
                lines.push(Line::styled("Last log lines:", Style::new().dim()));
                let h = body.height.saturating_sub(lines.len() as u16 + 3) as usize;
                let start = log.len().saturating_sub(h.max(3));
                lines.extend(log[start..].iter().map(|l| Line::styled(l.clone(), Style::new().dim())));
            }
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().border_style(Style::new().fg(color)).title(title)),
                body,
            );
        }
    }
    f.render_widget(Paragraph::new(help).style(Style::new().dim()), footer);
}
