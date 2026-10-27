//! Drawing the screen.

use super::layout::{Grid, Level, STATUS_W};
use super::{App, View};
use crate::format;
use crate::state::{Conn, Item, State};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use std::time::Instant;
use yoink_api::{LogLevel, Outcome};

/// Colour flagging an item's logs: red for errors, yellow for warnings.
fn flag(item: &Item) -> Style {
    match (item.outcome, item.worst) {
        (Some(Outcome::Fatal), _) | (_, Some(LogLevel::Error | LogLevel::Fatal)) => {
            Style::new().fg(Color::Red)
        }
        (_, Some(LogLevel::Warning)) => Style::new().fg(Color::Yellow),
        _ => Style::new().fg(Color::DarkGray),
    }
}

/// The 4 characters inside the status brackets, and their style.
pub fn status_text(s: &State, item: &Item) -> (String, Style) {
    match item.outcome {
        Some(Outcome::Success) => (" ✓  ".into(), Style::new().fg(Color::Green)),
        Some(Outcome::Fatal) => (
            " !  ".into(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        None => match (&item.phase, item.progress) {
            (None, _) => ("    ".into(), Style::new()),
            (Some(_), Some(p)) => (
                format!("{}{:>3}", s.phase_abbrev(item), p.percent()),
                Style::new().fg(Color::Cyan),
            ),
            (Some(_), None) => (
                format!("{}   ", s.phase_abbrev(item)),
                Style::new().fg(Color::Cyan),
            ),
        },
    }
}

fn pad(s: &str, w: usize) -> String {
    let t = format::truncate(s, w);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(w - n))
}

/// One indicator, `width` columns wide (without the trailing gap).
pub fn indicator(s: &State, item: &Item, level: Level, width: u16) -> Line<'static> {
    let (text, style) = status_text(s, item);
    let fl = flag(item);
    let mut spans = vec![
        Span::styled("[", fl),
        Span::styled(text, style),
        Span::styled("]", fl),
    ];
    let rest = (width.saturating_sub(STATUS_W + 1)) as usize;
    let name_style = if item.outcome == Some(Outcome::Success) {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new()
    };
    match level {
        Level::Mini => {}
        Level::Short => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad(&item.name, rest), name_style));
        }
        Level::Wide => {
            let phase_w = (rest / 3).min(18);
            let name_w = rest.saturating_sub(phase_w + 1);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad(&item.name, name_w), name_style));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                pad(&s.phase_name(item), phase_w),
                Style::new().fg(Color::Gray),
            ));
        }
        Level::Full => {
            let readout = match (item.outcome, item.progress) {
                (None, Some(p)) => format::readout(&p),
                (Some(Outcome::Success), _) => item.size.map(format::size).unwrap_or_default(),
                _ => String::new(),
            };
            let (phase_w, readout_w) = (18, 21);
            let name_w = (rest / 3).max(12);
            let bar_w = rest.saturating_sub(name_w + phase_w + readout_w + 3);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad(&item.name, name_w), name_style));
            spans.push(Span::raw(" "));
            if bar_w >= 4 {
                let ratio = match (item.outcome, item.progress) {
                    (Some(Outcome::Success), _) => 1.0,
                    (None, Some(p)) if p.max > 0 => p.current.min(p.max) as f64 / p.max as f64,
                    _ => 0.0,
                };
                let filled = ((bar_w as f64) * ratio).round() as usize;
                spans.push(Span::styled("━".repeat(filled), Style::new().fg(Color::Cyan)));
                spans.push(Span::styled(
                    "─".repeat(bar_w - filled),
                    Style::new().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::raw(" ".repeat(bar_w)));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                pad(&s.phase_name(item), phase_w),
                Style::new().fg(Color::Gray),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::raw(format!("{readout:>readout_w$}")));
        }
    }
    Line::from(spans)
}

/// Header (4 lines), body, footer (1 line).
pub fn areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([Constraint::Length(4), Constraint::Min(1), Constraint::Length(1)]).areas(area)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] = areas(f.area());
    header(f, app, head);
    match app.view {
        View::Field => field(f, app, body),
        View::Logs(item) => logs(f, app, item, body),
    }
    footer(f, app, foot);
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.state;
    let c = s.counts();
    let rows = Layout::vertical([Constraint::Length(1); 4]).split(area);
    let title = Line::from(vec![
        Span::styled("yoink ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{} → {}", app.targets, app.server)),
    ]);
    f.render_widget(Paragraph::new(title), rows[0]);
    let ratio = |a: f64, b: f64| if b > 0.0 { (a / b).clamp(0.0, 1.0) } else { 0.0 };
    let failed = if c.failed > 0 {
        format!(", {} failed", c.failed)
    } else {
        String::new()
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(if c.failed > 0 { Color::Red } else { Color::Green }))
            .ratio(ratio(c.done() as f64, c.total as f64))
            .label(format!("paths {}/{}{failed}", c.done(), c.total)),
        rows[1],
    );
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(Color::Cyan))
            .ratio(ratio(c.bytes_done as f64, c.bytes_total as f64))
            .label(format!(
                "size {} / {}",
                format::size(c.bytes_done),
                format::size(c.bytes_total)
            )),
        rows[2],
    );
    let plan = match &s.plan {
        Some(p) => {
            let mut spans = vec![Span::raw(format!(
                "nfi: {} cached paths, {} to build, {} to fetch",
                p.closure, p.to_build, p.to_fetch
            ))];
            if !p.warnings.is_empty() {
                spans.push(Span::styled(
                    format!(", {} warnings (l)", p.warnings.len()),
                    Style::new().fg(Color::Yellow),
                ));
            }
            Line::from(spans)
        }
        None => Line::raw(""),
    };
    f.render_widget(Paragraph::new(plan).style(Style::new().fg(Color::Gray)), rows[3]);
}

fn field(f: &mut Frame, app: &mut App, area: Rect) {
    let n = app.state.items.len();
    let grid = Grid::choose(n, area.width, area.height);
    app.scroll = app.scroll.min(grid.max_scroll());
    app.grid = Some((grid, area));
    for (i, x, y) in grid.visible(n, app.scroll) {
        let w = grid.cell_w.saturating_sub(1).min(area.width - x);
        let mut line = indicator(&app.state, &app.state.items[i], grid.level, w);
        if i == app.selected {
            line = line.patch_style(Style::new().add_modifier(Modifier::REVERSED));
        }
        f.render_widget(Paragraph::new(line), Rect::new(area.x + x, area.y + y, w, 1));
    }
}

fn log_line(s: &State, e: &crate::state::LogEntry, with_name: bool) -> Line<'static> {
    let style = match e.level {
        LogLevel::Info => Style::new().fg(Color::Gray),
        LogLevel::Warning => Style::new().fg(Color::Yellow),
        LogLevel::Error => Style::new().fg(Color::Red),
        LogLevel::Fatal => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    let mut spans = vec![Span::styled(format!("[{}] ", e.level.as_str()), style)];
    if with_name {
        spans.push(Span::styled(
            format!("{}: ", s.items[e.item].name),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(e.message.clone()));
    Line::from(spans)
}

fn logs(f: &mut Frame, app: &mut App, item: Option<usize>, area: Rect) {
    let s = &app.state;
    let mut lines: Vec<Line> = Vec::new();
    let title = match item {
        None => {
            if let Some(p) = &s.plan {
                for w in &p.warnings {
                    lines.push(Line::styled(format!("[nfi] {w}"), Style::new().fg(Color::Yellow)));
                }
            }
            lines.extend(s.logs.iter().map(|e| log_line(s, e, true)));
            " all logs ".to_string()
        }
        Some(i) => {
            let it = &s.items[i];
            let state = match it.outcome {
                Some(Outcome::Success) => "succeeded".to_string(),
                Some(Outcome::Fatal) => "failed".to_string(),
                None => {
                    let p = it
                        .progress
                        .map(|p| format!(" {} ({}%)", format::readout(&p), p.percent()));
                    format!("{}{}", s.phase_name(it), p.unwrap_or_default())
                }
            };
            lines.push(Line::raw(format!("{}  {state}", it.purl)));
            lines.extend(it.logs.iter().map(|&l| log_line(s, &s.logs[l], false)));
            format!(" {} ", it.name)
        }
    };
    let inner = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner);
    let top = app.log_scroll.map_or(max, |t| t.min(max));
    app.log_scroll = app.log_scroll.map(|t| t.min(max));
    app.log_top = top;
    app.log_page = inner.max(1);
    let p = Paragraph::new(lines)
        .block(Block::new().borders(Borders::TOP | Borders::BOTTOM).title(title))
        .scroll((top as u16, 0));
    f.render_widget(p, area);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let keys = match app.view {
        View::Field => "←↑↓→/jk move  enter path log  l all logs  q quit",
        View::Logs(_) => "↑↓/jk scroll  end follow  esc back  q quit",
    };
    let status = match &app.state.conn {
        Conn::Connecting => Span::styled("connecting", Style::new().fg(Color::Yellow)),
        Conn::Connected => Span::styled("connected", Style::new().fg(Color::Green)),
        Conn::Reconnecting { at, error } => Span::styled(
            format!(
                "reconnecting in {} ({error})",
                format::duration(at.saturating_duration_since(Instant::now()))
            ),
            Style::new().fg(Color::Yellow),
        ),
        Conn::Done => Span::styled("done", Style::new().fg(Color::Green)),
        Conn::Failed(e) => Span::styled(format!("failed: {e}"), Style::new().fg(Color::Red)),
    };
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
    f.render_widget(
        Paragraph::new(Line::styled(keys, Style::new().fg(Color::DarkGray))),
        l,
    );
    f.render_widget(Paragraph::new(Line::from(status)).right_aligned(), r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::purl;
    use yoink_api::{Event, PhaseDef, Progress, Unit};

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn indicators() {
        let a = purl(0, "hello-2.12");
        let mut s = State::new(std::slice::from_ref(&a));
        s.apply(Event::Phases {
            phases: vec![PhaseDef::new("t", "Transferring NAR", 'T')],
        });
        let it = s.items[0].clone();
        assert_eq!(text(&indicator(&s, &it, Level::Mini, 6)), "[    ]");
        s.apply(Event::Status {
            purl: a.clone(),
            phase: "t".into(),
            progress: Some(Progress {
                current: 57,
                max: 100,
                unit: Some(Unit::Bytes),
            }),
        });
        let it = s.items[0].clone();
        assert_eq!(text(&indicator(&s, &it, Level::Mini, 6)), "[T 57]");
        assert_eq!(text(&indicator(&s, &it, Level::Short, 16)), "[T 57] hello-2.…");
        let w = text(&indicator(&s, &it, Level::Wide, 39));
        assert!(
            w.starts_with("[T 57] hello-2.12") && w.contains("Transferr…"),
            "{w}"
        );
        assert_eq!(w.chars().count(), 39);
        let full = text(&indicator(&s, &it, Level::Full, 100));
        assert!(full.contains('━') && full.ends_with("57/100 B"), "{full}");
        assert_eq!(full.chars().count(), 100);
        s.apply(Event::Result {
            purl: a,
            outcome: Outcome::Success,
        });
        let it = s.items[0].clone();
        assert_eq!(text(&indicator(&s, &it, Level::Mini, 6)), "[ ✓  ]");
    }
}
