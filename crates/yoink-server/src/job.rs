//! Mirror jobs: shared, observable units of work with a phase, progress,
//! logs and an outcome.
//!
//! A backend creates a job with [`Job::start`] (or deduplicates through a
//! [`JobTable`]) and reports through the [`JobReporter`] its task owns. Any
//! number of streams [`Job::subscribe`]; a subscriber attaching late first
//! sees the current status and the logs kept so far.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use yoink_api::{LogLevel, Outcome, Progress, Unit};

/// Logs kept per job; older ones are dropped (subscribers are told how many).
const MAX_LOGS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub phase: String,
    pub progress: Option<Progress>,
}

/// What a subscriber has not seen yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// Status changes since the last update: one per phase entered, with
    /// the last progress it reported; progress-only changes are merged.
    pub statuses: Vec<Status>,
    pub logs: Vec<(LogLevel, String)>,
    /// Set once, in the last update.
    pub outcome: Option<Outcome>,
}

/// Phase changes kept per job for subscribers that fall behind.
const MAX_PHASES: usize = 64;

struct State {
    status: Status,
    /// Phases entered with their last progress, most recent (the current
    /// status) last.
    phases: VecDeque<Status>,
    total_phases: usize,
    logs: VecDeque<(LogLevel, String)>,
    /// Logs ever written (including dropped ones).
    total_logs: usize,
    outcome: Option<Outcome>,
}

struct Shared {
    state: Mutex<State>,
    changed: watch::Sender<()>,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        self.changed.send_replace(());
    }
}

/// A handle to observe a job.
#[derive(Clone)]
pub struct Job(Arc<Shared>);

impl Job {
    /// Create a job in `phase` and spawn `work` for it. `Ok` ends it with
    /// [`Outcome::Success`]; `Err` logs the error as fatal and ends it with
    /// [`Outcome::Fatal`]. A task that panics or is dropped also ends fatally.
    pub fn start<F, Fut>(phase: &str, work: F) -> Job
    where
        F: FnOnce(JobReporter) -> Fut,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self::start_then(phase, work, || {})
    }

    fn start_then<F, Fut>(phase: &str, work: F, done: impl FnOnce() + Send + 'static) -> Job
    where
        F: FnOnce(JobReporter) -> Fut,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let (job, reporter) = Job::new(phase);
        let fut = work(reporter.clone_inner());
        tokio::spawn(async move {
            let r = fut.await;
            reporter.finish(r);
            done();
        });
        job
    }

    /// A job driven by hand; it ends when the reporter finishes or is dropped.
    pub fn new(phase: &str) -> (Job, JobReporter) {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: Status {
                    phase: phase.to_string(),
                    progress: None,
                },
                phases: VecDeque::from([Status {
                    phase: phase.to_string(),
                    progress: None,
                }]),
                total_phases: 1,
                logs: VecDeque::new(),
                total_logs: 0,
                outcome: None,
            }),
            changed: watch::channel(()).0,
        });
        (Job(shared.clone()), JobReporter { shared, owner: true })
    }

    pub fn status(&self) -> Status {
        self.0.state.lock().unwrap().status.clone()
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.0.state.lock().unwrap().outcome
    }

    /// Subscribe; the first update has the current status (not the phases
    /// before it) and the logs kept so far.
    pub fn subscribe(&self) -> Subscription {
        let seen_phases = self.0.state.lock().unwrap().total_phases - 1;
        Subscription {
            shared: self.0.clone(),
            rx: self.0.changed.subscribe(),
            seen_logs: 0,
            seen_phases,
            last_status: None,
            done: false,
        }
    }

    fn same(&self, other: &Job) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Reports a job's progress. Cheap to clone; the job ends when the owning
/// reporter (the one [`Job::new`] returned) finishes or is dropped.
pub struct JobReporter {
    shared: Arc<Shared>,
    owner: bool,
}

impl Clone for JobReporter {
    fn clone(&self) -> Self {
        self.clone_inner()
    }
}

impl JobReporter {
    fn clone_inner(&self) -> JobReporter {
        JobReporter {
            shared: self.shared.clone(),
            owner: false,
        }
    }

    /// Enter a phase (clearing progress).
    pub fn phase(&self, id: &str) {
        self.shared.update(|s| {
            s.status = Status {
                phase: id.to_string(),
                progress: None,
            };
            if s.phases.len() == MAX_PHASES {
                s.phases.pop_front();
            }
            s.phases.push_back(s.status.clone());
            s.total_phases += 1;
        });
    }

    pub fn progress(&self, current: u64, max: u64, unit: Option<Unit>) {
        self.shared.update(|s| {
            s.status.progress = Some(Progress { current, max, unit });
            if let Some(b) = s.phases.back_mut() {
                b.progress = s.status.progress;
            }
        });
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        self.shared.update(|s| {
            if s.logs.len() == MAX_LOGS {
                s.logs.pop_front();
            }
            s.logs.push_back((level, message));
            s.total_logs += 1;
        });
    }

    pub fn info(&self, m: impl Into<String>) {
        self.log(LogLevel::Info, m)
    }

    pub fn warn(&self, m: impl Into<String>) {
        self.log(LogLevel::Warning, m)
    }

    pub fn error(&self, m: impl Into<String>) {
        self.log(LogLevel::Error, m)
    }

    /// End the job with the result of its work.
    pub fn finish(mut self, r: anyhow::Result<()>) {
        self.end(r);
    }

    fn end(&mut self, r: anyhow::Result<()>) {
        let done = self.shared.state.lock().unwrap().outcome.is_some();
        if done {
            return;
        }
        let outcome = match r {
            Ok(()) => Outcome::Success,
            Err(e) => {
                self.log(LogLevel::Fatal, format!("{e:#}"));
                Outcome::Fatal
            }
        };
        self.shared.update(|s| s.outcome = Some(outcome));
    }
}

impl Drop for JobReporter {
    fn drop(&mut self) {
        if self.owner {
            self.end(Err(anyhow::anyhow!("the job ended without a result")));
        }
    }
}

/// A subscriber's view of a job.
pub struct Subscription {
    shared: Arc<Shared>,
    rx: watch::Receiver<()>,
    seen_logs: usize,
    seen_phases: usize,
    last_status: Option<Status>,
    done: bool,
}

impl Subscription {
    /// Wait for and return what changed; `None` after the update carrying
    /// the outcome.
    pub async fn next(&mut self) -> Option<Update> {
        if self.done {
            return None;
        }
        loop {
            if let Some(u) = self.take() {
                return Some(u);
            }
            // The sender lives as long as `shared`, which we hold.
            let _ = self.rx.changed().await;
        }
    }

    fn take(&mut self) -> Option<Update> {
        self.rx.mark_unchanged();
        let s = self.shared.state.lock().unwrap();
        let mut u = Update::default();
        // Phases entered since the last update, except the current one.
        let first_phase = s.total_phases - s.phases.len();
        let unseen = s.total_phases - self.seen_phases.max(first_phase);
        for p in s.phases.iter().rev().skip(1).take(unseen.saturating_sub(1)).rev() {
            u.statuses.push(p.clone());
        }
        self.seen_phases = s.total_phases;
        if unseen > 0 || self.last_status.as_ref() != Some(&s.status) {
            u.statuses.push(s.status.clone());
            self.last_status = Some(s.status.clone());
        }
        let first_kept = s.total_logs - s.logs.len();
        if self.seen_logs < first_kept {
            u.logs.push((
                LogLevel::Info,
                format!("({} earlier log lines dropped)", first_kept - self.seen_logs),
            ));
            self.seen_logs = first_kept;
        }
        u.logs
            .extend(s.logs.iter().skip(self.seen_logs - first_kept).cloned());
        self.seen_logs = s.total_logs;
        u.outcome = s.outcome;
        self.done = u.outcome.is_some();
        (u != Update::default()).then_some(u)
    }
}

/// Deduplicates jobs by key: a key with a running job gets that job; a
/// finished job leaves the table, so the next request starts afresh.
pub struct JobTable<K> {
    jobs: Arc<Mutex<HashMap<K, Job>>>,
}

impl<K> Default for JobTable<K> {
    fn default() -> Self {
        JobTable {
            jobs: Default::default(),
        }
    }
}

impl<K: Eq + Hash + Clone + Send + 'static> JobTable<K> {
    /// The running job for `key`, or a new one started as by [`Job::start`].
    pub fn get_or_start<F, Fut>(&self, key: K, phase: &str, work: F) -> Job
    where
        F: FnOnce(JobReporter) -> Fut,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(j) = jobs.get(&key) {
            return j.clone();
        }
        // The task must not remove the entry before it is inserted; it can't,
        // as removal takes the lock we hold. It may remove a newer job only
        // if it is the same one.
        let table = self.jobs.clone();
        let k = key.clone();
        let slot: Arc<Mutex<Option<Job>>> = Default::default();
        let s = slot.clone();
        let job = Job::start_then(phase, work, move || {
            let mine = s.lock().unwrap().take();
            let mut t = table.lock().unwrap();
            if let (Some(m), Some(j)) = (mine, t.get(&k))
                && m.same(j)
            {
                t.remove(&k);
            }
        });
        *slot.lock().unwrap() = Some(job.clone());
        jobs.insert(key, job.clone());
        job
    }

    /// Number of running jobs.
    pub fn len(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn subscribe_and_finish() {
        let (job, r) = Job::new("q");
        let mut a = job.subscribe();
        let u = a.next().await.unwrap();
        assert_eq!(u.statuses[0].phase, "q");
        r.phase("c");
        r.phase("t");
        r.progress(4, 10, Some(Unit::Bytes));
        r.progress(5, 10, Some(Unit::Bytes));
        r.warn("slow");
        let u = a.next().await.unwrap();
        let ph: Vec<_> = u
            .statuses
            .iter()
            .map(|s| (s.phase.as_str(), s.progress.map(|p| p.current)))
            .collect();
        assert_eq!(ph, [("c", None), ("t", Some(5))]);
        assert_eq!(u.logs, vec![(LogLevel::Warning, "slow".to_string())]);
        // A late subscriber sees the current status and the logs so far.
        let mut b = job.subscribe();
        let u = b.next().await.unwrap();
        assert_eq!(u.logs.len(), 1);
        assert_eq!(
            u.statuses.len(),
            1,
            "a late subscriber gets the current status only"
        );
        r.finish(Err(anyhow::anyhow!("boom")));
        let u = a.next().await.unwrap();
        assert_eq!(u.outcome, Some(Outcome::Fatal));
        assert_eq!(u.logs, vec![(LogLevel::Fatal, "boom".to_string())]);
        assert!(a.next().await.is_none());
        let u = b.next().await.unwrap();
        assert_eq!(u.outcome, Some(Outcome::Fatal));
        // Subscribing after the end gives everything at once.
        let mut c = job.subscribe();
        let u = c.next().await.unwrap();
        assert_eq!((u.logs.len(), u.outcome), (2, Some(Outcome::Fatal)));
    }

    #[tokio::test]
    async fn dropped_reporter_is_fatal() {
        let (job, r) = Job::new("q");
        let extra = r.clone();
        drop(extra);
        assert_eq!(job.outcome(), None);
        drop(r);
        assert_eq!(job.outcome(), Some(Outcome::Fatal));
        let j = Job::start("q", |_r| async { panic!("oops") });
        let mut s = j.subscribe();
        while let Some(u) = s.next().await {
            if u.outcome.is_some() {
                break;
            }
        }
        assert_eq!(j.outcome(), Some(Outcome::Fatal));
    }

    #[tokio::test]
    async fn log_cap() {
        let (job, r) = Job::new("q");
        for i in 0..MAX_LOGS + 5 {
            r.info(format!("{i}"));
        }
        let u = job.subscribe().next().await.unwrap();
        assert_eq!(u.logs.len(), MAX_LOGS + 1);
        assert_eq!(u.logs[0].1, "(5 earlier log lines dropped)");
        assert_eq!(u.logs[1].1, "5");
    }

    #[tokio::test]
    async fn table_dedupes() {
        let t = JobTable::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let a = t.get_or_start("k", "q", |_r| async move {
            let _ = rx.await;
            Ok(())
        });
        let b = t.get_or_start("k", "q", |_r| async { panic!("not started twice") });
        assert!(a.same(&b));
        assert_eq!(t.len(), 1);
        tx.send(()).unwrap();
        let mut s = a.subscribe();
        while s.next().await.is_some() {}
        assert_eq!(a.outcome(), Some(Outcome::Success));
        for _ in 0..100 {
            if t.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(t.is_empty());
        let c = t.get_or_start("k", "q", |_r| async { Ok(()) });
        assert!(!c.same(&a));
    }
}
