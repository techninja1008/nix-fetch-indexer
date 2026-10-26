//! The pull's state, built from mirror stream events; independent of how it
//! is displayed.

use crate::targets::display_name;
use std::collections::HashMap;
use std::time::Instant;
use yoink_api::{Event, LogLevel, Outcome, PhaseDef, Progress, Unit};

/// One requested PURL.
#[derive(Clone, Debug)]
pub struct Item {
    pub purl: String,
    pub name: String,
    /// The current phase id; `None` before the first status.
    pub phase: Option<String>,
    pub progress: Option<Progress>,
    pub outcome: Option<Outcome>,
    /// Indices into [`State::logs`].
    pub logs: Vec<usize>,
    /// The most severe log level seen.
    pub worst: Option<LogLevel>,
    /// The largest byte total reported, for the overall size.
    pub size: Option<u64>,
}

impl Item {
    pub fn is_done(&self) -> bool {
        self.outcome.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub item: usize,
    pub level: LogLevel,
    pub message: String,
}

/// What NFI's minimise said beyond the closure.
#[derive(Clone, Debug, Default)]
pub struct PlanSummary {
    pub to_build: usize,
    pub to_fetch: usize,
    pub closure: usize,
    pub warnings: Vec<String>,
}

/// The connection to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Connected,
    /// Retrying at `at` after `error`.
    Reconnecting {
        at: Instant,
        error: String,
    },
    Done,
    /// Gave up; the pull cannot complete.
    Failed(String),
}

/// What an event changed, for line-oriented output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Phases,
    Phase(usize),
    Progress(usize),
    Log(usize),
    Result(usize),
    /// An event for a PURL that wasn't requested, or for one already finished.
    Ignored,
}

pub struct State {
    pub items: Vec<Item>,
    index: HashMap<String, usize>,
    pub phases: Vec<PhaseDef>,
    pub logs: Vec<LogEntry>,
    pub plan: Option<PlanSummary>,
    pub conn: Conn,
}

impl State {
    pub fn new(purls: &[String]) -> Self {
        State {
            items: purls
                .iter()
                .map(|p| Item {
                    purl: p.clone(),
                    name: display_name(p),
                    phase: None,
                    progress: None,
                    outcome: None,
                    logs: vec![],
                    worst: None,
                    size: None,
                })
                .collect(),
            index: purls.iter().enumerate().map(|(i, p)| (p.clone(), i)).collect(),
            phases: vec![],
            logs: vec![],
            plan: None,
            conn: Conn::Connecting,
        }
    }

    pub fn phase(&self, id: &str) -> Option<&PhaseDef> {
        self.phases.iter().find(|p| p.id == id)
    }

    /// Full phase name of an item, `?` for unknown phases.
    pub fn phase_name(&self, item: &Item) -> String {
        match &item.phase {
            None => String::new(),
            Some(id) => self
                .phase(id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "?".into()),
        }
    }

    pub fn phase_abbrev(&self, item: &Item) -> char {
        match &item.phase {
            None => ' ',
            Some(id) => self.phase(id).map(|p| p.abbrev).unwrap_or('?'),
        }
    }

    pub fn apply(&mut self, ev: Event) -> Change {
        if let Event::Phases { phases } = ev {
            self.phases = phases;
            return Change::Phases;
        }
        let Some(&i) = ev.purl().and_then(|p| self.index.get(p)) else {
            return Change::Ignored;
        };
        let item = &mut self.items[i];
        if item.is_done() {
            return Change::Ignored;
        }
        match ev {
            Event::Phases { .. } => unreachable!(),
            Event::Status { phase, progress, .. } => {
                if let Some(p) = &progress
                    && p.unit == Some(Unit::Bytes)
                {
                    item.size = Some(item.size.unwrap_or(0).max(p.max));
                }
                item.progress = progress;
                if item.phase.as_deref() != Some(&phase) {
                    item.phase = Some(phase);
                    Change::Phase(i)
                } else {
                    Change::Progress(i)
                }
            }
            Event::Log { level, message, .. } => {
                item.worst = item.worst.max(Some(level));
                item.logs.push(self.logs.len());
                self.logs.push(LogEntry {
                    item: i,
                    level,
                    message,
                });
                Change::Log(self.logs.len() - 1)
            }
            Event::Result { outcome, .. } => {
                item.outcome = Some(outcome);
                Change::Result(i)
            }
        }
    }

    /// PURLs without a result yet, in request order.
    pub fn pending(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| !i.is_done())
            .map(|i| i.purl.clone())
            .collect()
    }

    pub fn finished(&self) -> bool {
        self.items.iter().all(Item::is_done)
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            total: self.items.len(),
            ..Default::default()
        };
        for i in &self.items {
            match i.outcome {
                Some(Outcome::Success) => c.succeeded += 1,
                Some(Outcome::Fatal) => c.failed += 1,
                None => {}
            }
            if let Some(max) = i.size {
                c.bytes_total += max;
                c.bytes_done += match (i.outcome, i.progress) {
                    (Some(Outcome::Success), _) => max,
                    (None, Some(p)) if p.unit == Some(Unit::Bytes) => p.current.min(p.max),
                    _ => 0,
                };
            }
        }
        c
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl Counts {
    pub fn done(&self) -> usize {
        self.succeeded + self.failed
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use yoink_api::Purl;

    pub fn purl(n: u8, name: &str) -> String {
        let h = format!("{n}xy6xjvbl4bm3pq4w6p2b6vfffw4vi9b");
        Purl::nix(&format!("/nix/store/{h}-{name}")).to_string()
    }

    fn status(p: &str, phase: &str, progress: Option<(u64, u64)>) -> Event {
        Event::Status {
            purl: p.into(),
            phase: phase.into(),
            progress: progress.map(|(current, max)| Progress {
                current,
                max,
                unit: Some(Unit::Bytes),
            }),
        }
    }

    #[test]
    fn applies_events() {
        let (a, b) = (purl(0, "a"), purl(1, "b"));
        let mut s = State::new(&[a.clone(), b.clone()]);
        assert_eq!(s.items[0].name, "a");
        assert_eq!(
            s.apply(Event::Phases {
                phases: vec![PhaseDef::new("t", "Transferring NAR", 'T')]
            }),
            Change::Phases
        );
        assert_eq!(s.apply(status(&a, "t", Some((10, 100)))), Change::Phase(0));
        assert_eq!(s.apply(status(&a, "t", Some((50, 100)))), Change::Progress(0));
        assert_eq!(s.phase_name(&s.items[0]), "Transferring NAR");
        assert_eq!(s.apply(status(&b, "zz", None)), Change::Phase(1));
        assert_eq!(s.phase_name(&s.items[1]), "?");
        assert_eq!(s.phase_abbrev(&s.items[1]), '?');
        let c = s.counts();
        assert_eq!((c.bytes_done, c.bytes_total, c.done()), (50, 100, 0));
        let log = |p: &str, level| Event::Log {
            purl: p.into(),
            level,
            message: "m".into(),
        };
        assert_eq!(s.apply(log(&b, LogLevel::Warning)), Change::Log(0));
        s.apply(log(&b, LogLevel::Info));
        assert_eq!(s.items[1].worst, Some(LogLevel::Warning));
        s.apply(log(&b, LogLevel::Fatal));
        assert_eq!(s.items[1].logs, vec![0, 1, 2]);
        assert_eq!(
            s.apply(Event::Result {
                purl: a.clone(),
                outcome: Outcome::Success
            }),
            Change::Result(0)
        );
        assert_eq!(s.pending(), vec![b.clone()]);
        assert!(!s.finished());
        // Events after a result, and for unknown PURLs, are ignored.
        assert_eq!(s.apply(status(&a, "t", None)), Change::Ignored);
        assert_eq!(s.apply(status("pkg:nix/other", "t", None)), Change::Ignored);
        s.apply(Event::Result {
            purl: b.clone(),
            outcome: Outcome::Fatal,
        });
        assert!(s.finished());
        let c = s.counts();
        assert_eq!(
            (c.succeeded, c.failed, c.bytes_done, c.bytes_total),
            (1, 1, 100, 100)
        );
    }
}
