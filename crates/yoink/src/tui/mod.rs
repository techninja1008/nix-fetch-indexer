//! The fullscreen pull monitor.

pub mod layout;
pub mod render;

use crate::state::{Conn, State};
use crate::stream::Msg;
use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use layout::Grid;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Field,
    /// One item's logs, or all logs.
    Logs(Option<usize>),
}

pub struct App {
    pub state: State,
    pub view: View,
    pub selected: usize,
    /// Field scroll offset, in rows.
    pub scroll: usize,
    /// Log view top line; `None` follows the tail.
    pub log_scroll: Option<usize>,
    /// The log view's top line and page height as last drawn.
    pub log_top: usize,
    pub log_page: usize,
    /// The field's last layout and area, for navigation and mouse hits.
    pub grid: Option<(Grid, Rect)>,
    last_click: Option<(Instant, usize)>,
    pub targets: String,
    pub server: String,
    pub quit: bool,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

impl App {
    pub fn new(state: State, targets: String, server: String) -> Self {
        App {
            state,
            view: View::Field,
            selected: 0,
            scroll: 0,
            log_scroll: None,
            log_top: 0,
            log_page: 10,
            grid: None,
            last_click: None,
            targets,
            server,
            quit: false,
        }
    }

    fn select(&mut self, i: isize) {
        let n = self.state.items.len();
        if n == 0 {
            return;
        }
        self.selected = i.clamp(0, n as isize - 1) as usize;
        if let Some((g, _)) = &self.grid {
            self.scroll = g.reveal(self.selected, self.scroll);
        }
    }

    fn open_logs(&mut self, item: Option<usize>) {
        self.view = View::Logs(item);
        self.log_scroll = None;
    }

    fn scroll_logs(&mut self, by: isize) {
        let top = self.log_scroll.unwrap_or(self.log_top) as isize;
        self.log_scroll = Some((top + by).max(0) as usize);
    }

    pub fn key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if k.code == KeyCode::Char('q')
            || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.quit = true;
            return;
        }
        match self.view {
            View::Field => {
                let (cols, page) = self
                    .grid
                    .map(|(g, _)| (g.cols as isize, (g.cols * g.rows) as isize))
                    .unwrap_or((1, 10));
                let s = self.selected as isize;
                match k.code {
                    KeyCode::Left => self.select(s - 1),
                    KeyCode::Right => self.select(s + 1),
                    KeyCode::Up | KeyCode::Char('k') => self.select(s - cols),
                    KeyCode::Down | KeyCode::Char('j') => self.select(s + cols),
                    KeyCode::PageUp => self.select(s - page),
                    KeyCode::PageDown => self.select(s + page),
                    KeyCode::Home => self.select(0),
                    KeyCode::End => self.select(isize::MAX / 2),
                    KeyCode::Enter => self.open_logs(Some(self.selected)),
                    KeyCode::Char('l') => self.open_logs(None),
                    _ => {}
                }
            }
            View::Logs(_) => {
                let page = self.log_page as isize;
                match k.code {
                    KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('l') => self.view = View::Field,
                    KeyCode::Up | KeyCode::Char('k') => self.scroll_logs(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.scroll_logs(1),
                    KeyCode::PageUp => self.scroll_logs(-page),
                    KeyCode::PageDown => self.scroll_logs(page),
                    KeyCode::Home => self.log_scroll = Some(0),
                    KeyCode::End => self.log_scroll = None,
                    _ => {}
                }
            }
        }
    }

    pub fn mouse(&mut self, m: MouseEvent, now: Instant) {
        match (self.view, m.kind) {
            (View::Field, MouseEventKind::Down(MouseButton::Left)) => {
                let Some((g, area)) = self.grid else { return };
                if m.column < area.x || m.row < area.y {
                    return;
                }
                let n = self.state.items.len();
                let Some(i) = g.hit(n, self.scroll, m.column - area.x, m.row - area.y) else {
                    return;
                };
                let double = self
                    .last_click
                    .is_some_and(|(t, j)| j == i && now.duration_since(t) < DOUBLE_CLICK);
                if i == self.selected || double {
                    self.open_logs(Some(i));
                } else {
                    self.selected = i;
                }
                self.last_click = Some((now, i));
            }
            (View::Field, MouseEventKind::ScrollDown) => {
                let max = self.grid.map_or(0, |(g, _)| g.max_scroll());
                self.scroll = (self.scroll + 1).min(max);
            }
            (View::Field, MouseEventKind::ScrollUp) => self.scroll = self.scroll.saturating_sub(1),
            (View::Logs(_), MouseEventKind::ScrollDown) => self.scroll_logs(3),
            (View::Logs(_), MouseEventKind::ScrollUp) => self.scroll_logs(-3),
            (View::Logs(_), MouseEventKind::Down(MouseButton::Right)) => self.view = View::Field,
            _ => {}
        }
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Restores the terminal when dropped, and on panic.
struct Guard;

impl Guard {
    fn enter() -> Result<Guard> {
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                prev(info)
            }));
        });
        enable_raw_mode()?;
        let g = Guard;
        execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(g)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Run until every PURL has a result, the pull fails, or the user quits.
/// Returns the final state and whether the user quit.
pub async fn run(mut app: App, mut rx: UnboundedReceiver<Msg>) -> Result<(State, bool)> {
    let guard = Guard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut redraw = true;
    loop {
        if redraw {
            term.draw(|f| render::draw(f, &mut app))?;
            redraw = false;
        }
        if app.quit || matches!(app.state.conn, Conn::Done | Conn::Failed(_)) {
            break;
        }
        tokio::select! {
            m = rx.recv() => {
                let Some(m) = m else { break };
                apply(&mut app.state, m);
                // Take whatever else is queued before drawing again.
                while let Ok(m) = rx.try_recv() {
                    apply(&mut app.state, m);
                }
                redraw = true;
            }
            e = input.next() => match e {
                Some(Ok(TermEvent::Key(k))) => { app.key(k); redraw = true; }
                Some(Ok(TermEvent::Mouse(m))) => { app.mouse(m, Instant::now()); redraw = true; }
                Some(Ok(TermEvent::Resize(..))) => redraw = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            _ = tick.tick() => redraw = true,
        }
    }
    drop(term);
    drop(guard);
    Ok((app.state, app.quit))
}

pub fn apply(s: &mut State, m: Msg) {
    match m {
        Msg::Event(e) => {
            s.apply(e);
        }
        Msg::Conn(c) => s.conn = c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::purl;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn navigation() {
        let purls: Vec<String> = (0..9).map(|i| purl(i, &format!("p{i}"))).collect();
        let mut app = App::new(State::new(&purls), String::new(), String::new());
        // 5 columns of short cells, 2 rows on screen.
        let g = Grid::choose(9, 120, 2);
        assert_eq!((g.cols, g.rows), (5, 2));
        app.grid = Some((g, Rect::new(0, 4, 120, 2)));
        app.key(key(KeyCode::Right));
        assert_eq!(app.selected, 1);
        app.key(key(KeyCode::Down));
        assert_eq!(app.selected, 6);
        app.key(key(KeyCode::End));
        assert_eq!(app.selected, 8);
        app.key(key(KeyCode::Home));
        assert_eq!(app.selected, 0);
        app.key(key(KeyCode::Left));
        assert_eq!(app.selected, 0);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.view, View::Logs(Some(0)));
        app.key(key(KeyCode::Up));
        assert!(app.log_scroll.is_some());
        app.key(key(KeyCode::End));
        assert_eq!(app.log_scroll, None);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.view, View::Field);
        app.key(key(KeyCode::Char('l')));
        assert_eq!(app.view, View::Logs(None));
        app.key(key(KeyCode::Char('l')));
        // Click selects, a second click opens the log.
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let t = Instant::now();
        app.mouse(click(30, 5), t);
        assert_eq!((app.selected, app.view), (6, View::Field));
        app.mouse(click(30, 5), t + Duration::from_secs(2));
        assert_eq!(app.view, View::Logs(Some(6)));
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }
}
