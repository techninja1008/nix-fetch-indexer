//! Line-oriented output, for when there is no terminal (or with --plain).

use crate::format;
use crate::state::{Change, Conn, State};
use std::time::{Duration, Instant};
use yoink_api::{LogLevel, Outcome};

/// How often `-vv` reports progress of each active PURL.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);

/// Verbosity: -1 (`-q`) errors only; 0 phase changes, errors and results;
/// 1 (`-v`) also warnings and info; 2 (`-vv`) also periodic progress.
pub struct Plain {
    verbosity: i8,
    last_progress: Option<Instant>,
}

impl Plain {
    pub fn new(verbosity: i8) -> Self {
        Plain {
            verbosity,
            last_progress: None,
        }
    }

    pub fn change(&self, s: &State, c: &Change) -> Option<String> {
        let v = self.verbosity;
        match *c {
            Change::Phase(i) if v >= 0 => {
                let it = &s.items[i];
                Some(format!("{}: {}", it.name, s.phase_name(it)))
            }
            Change::Log(l) => {
                let e = &s.logs[l];
                let show = match e.level {
                    LogLevel::Error | LogLevel::Fatal => true,
                    LogLevel::Warning | LogLevel::Info => v >= 1,
                };
                show.then(|| format!("{}: {}: {}", s.items[e.item].name, e.level.as_str(), e.message))
            }
            Change::Result(i) => {
                let it = &s.items[i];
                match it.outcome {
                    Some(Outcome::Success) if v >= 0 => Some(format!("{}: done", it.name)),
                    Some(Outcome::Fatal) => Some(format!("{}: FAILED", it.name)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn conn(&self, c: &Conn) -> Option<String> {
        match c {
            Conn::Reconnecting { at, error } if self.verbosity >= 0 => Some(format!(
                "connection lost ({error}); reconnecting in {}",
                format::duration(at.saturating_duration_since(Instant::now()) + Duration::from_millis(999))
            )),
            Conn::Failed(e) => Some(format!("error: {e}")),
            _ => None,
        }
    }

    /// With `-vv`, progress lines for active PURLs every few seconds.
    pub fn tick(&mut self, s: &State, now: Instant) -> Vec<String> {
        if self.verbosity < 2
            || self
                .last_progress
                .is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY)
        {
            return vec![];
        }
        self.last_progress = Some(now);
        s.items
            .iter()
            .filter(|i| !i.is_done())
            .filter_map(|i| {
                let p = i.progress?;
                Some(format!(
                    "{}: {} {} ({}%)",
                    i.name,
                    s.phase_name(i),
                    format::readout(&p),
                    p.percent()
                ))
            })
            .collect()
    }
}

/// The closing summary: counts, NFI's plan, and each failure with its errors.
pub fn summary(s: &State) -> String {
    let c = s.counts();
    let mut out = String::new();
    if let Some(p) = &s.plan
        && (p.to_build > 0 || p.to_fetch > 0)
    {
        out.push_str(&format!(
            "note: {} derivations must still be built and {} sources fetched; yoink only mirrors cached paths\n",
            p.to_build, p.to_fetch
        ));
    }
    for it in s.items.iter().filter(|i| i.outcome == Some(Outcome::Fatal)) {
        out.push_str(&format!("failed: {}\n", it.purl));
        for &l in &it.logs {
            let e = &s.logs[l];
            if e.level >= LogLevel::Error {
                out.push_str(&format!("  {}: {}\n", e.level.as_str(), e.message));
            }
        }
    }
    out.push_str(&format!(
        "{} of {} paths mirrored, {} failed{}",
        c.succeeded,
        c.total,
        c.failed,
        if c.bytes_total > 0 {
            format!(" ({} transferred)", format::size(c.bytes_done))
        } else {
            String::new()
        }
    ));
    if c.done() < c.total {
        out.push_str(&format!(", {} unfinished", c.total - c.done()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::purl;
    use yoink_api::{Event, PhaseDef};

    #[test]
    fn lines() {
        let a = purl(0, "a");
        let mut s = State::new(std::slice::from_ref(&a));
        s.apply(Event::Phases {
            phases: vec![PhaseDef::new("q", "Queued", 'Q')],
        });
        let p = Plain::new(0);
        let c = s.apply(Event::Status {
            purl: a.clone(),
            phase: "q".into(),
            progress: None,
        });
        assert_eq!(p.change(&s, &c).as_deref(), Some("a: Queued"));
        assert_eq!(Plain::new(-1).change(&s, &c), None);
        let log = |level| Event::Log {
            purl: a.clone(),
            level,
            message: "boom".into(),
        };
        let c = s.apply(log(LogLevel::Warning));
        assert_eq!(p.change(&s, &c), None);
        assert_eq!(Plain::new(1).change(&s, &c).as_deref(), Some("a: warning: boom"));
        let c = s.apply(log(LogLevel::Fatal));
        assert_eq!(Plain::new(-1).change(&s, &c).as_deref(), Some("a: fatal: boom"));
        let c = s.apply(Event::Result {
            purl: a.clone(),
            outcome: Outcome::Fatal,
        });
        assert_eq!(Plain::new(-1).change(&s, &c).as_deref(), Some("a: FAILED"));
        let sum = summary(&s);
        assert!(sum.contains("fatal: boom") && !sum.contains("warning"), "{sum}");
        assert!(sum.ends_with("0 of 1 paths mirrored, 1 failed"), "{sum}");
    }
}
