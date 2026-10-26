//! Drives the mirror request: streams events, and re-posts the PURLs still
//! without a result whenever the stream ends early.

use crate::state::Conn;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use yoink_api::Event;
use yoink_api::client::{Client, EventStream, HttpError};

/// Reconnect delays: exponential from [`Backoff::FIRST`], capped at
/// [`Backoff::MAX`]; give up after [`Backoff::GIVE_UP`] without progress.
#[derive(Debug, Default)]
pub struct Backoff {
    failing_since: Option<Instant>,
    attempt: u32,
}

impl Backoff {
    pub const FIRST: Duration = Duration::from_millis(500);
    pub const MAX: Duration = Duration::from_secs(30);
    pub const GIVE_UP: Duration = Duration::from_secs(300);

    /// Progress was made: the next failure starts over.
    pub fn success(&mut self) {
        *self = Self::default();
    }

    /// Record a failure; the delay before retrying, or `None` to give up.
    pub fn failure(&mut self, now: Instant) -> Option<Duration> {
        let since = *self.failing_since.get_or_insert(now);
        if now.duration_since(since) >= Self::GIVE_UP {
            return None;
        }
        let d = Self::FIRST
            .saturating_mul(1 << self.attempt.min(16))
            .min(Self::MAX);
        self.attempt += 1;
        Some(d)
    }
}

pub enum Msg {
    Event(Event),
    Conn(Conn),
}

/// Whether a failed request should not be retried.
pub fn is_fatal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| (400..500).contains(&h.status) && !matches!(h.status, 408 | 429))
}

fn describe(e: &anyhow::Error) -> String {
    match e.downcast_ref::<HttpError>() {
        Some(h) if h.status == 401 => format!("{e}; run `yoink login` or set YOINK_TOKEN"),
        _ => format!("{e:#}"),
    }
}

/// Where to get a fresh token before reconnecting, when the token came from
/// the login cache (ID tokens can expire during a long pull).
pub struct Refresh {
    pub session: nfi_auth::client::Session,
    pub url: String,
}

/// Mirror `purls`, sending events and connection changes to `tx` until every
/// PURL has a result, the failure is fatal, or `tx` is closed. `first` is the
/// result of an initial request already made.
pub async fn drive(
    mut client: Client,
    purls: Vec<String>,
    first: Option<anyhow::Result<EventStream>>,
    refresh: Option<Refresh>,
    tx: UnboundedSender<Msg>,
) {
    use futures::StreamExt;
    let mut remaining = purls;
    let mut done = HashSet::new();
    let mut backoff = Backoff::default();
    let mut next = first;
    loop {
        remaining.retain(|p| !done.contains(p));
        if remaining.is_empty() {
            let _ = tx.send(Msg::Conn(Conn::Done));
            return;
        }
        let r = match next.take() {
            Some(r) => r,
            None => {
                if let Some(rf) = &refresh
                    && let Ok(Some(t)) = rf.session.cached_token(&rf.url).await
                {
                    client.set_token(Some(t));
                }
                client.mirror(&remaining).await
            }
        };
        let error = match r {
            Err(e) if is_fatal(&e) => {
                let _ = tx.send(Msg::Conn(Conn::Failed(describe(&e))));
                return;
            }
            Err(e) => describe(&e),
            Ok(mut s) => {
                if tx.send(Msg::Conn(Conn::Connected)).is_err() {
                    return;
                }
                let mut error = "the stream ended before every result".to_string();
                while let Some(ev) = s.next().await {
                    match ev {
                        Ok(ev) => {
                            match &ev {
                                Event::Phases { .. } => {}
                                Event::Result { purl, .. } => {
                                    done.insert(purl.clone());
                                    backoff.success();
                                }
                                _ => backoff.success(),
                            }
                            if tx.send(Msg::Event(ev)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            error = format!("{e:#}");
                            break;
                        }
                    }
                }
                remaining.retain(|p| !done.contains(p));
                if remaining.is_empty() {
                    continue;
                }
                error
            }
        };
        let Some(delay) = backoff.failure(Instant::now()) else {
            let _ = tx.send(Msg::Conn(Conn::Failed(format!(
                "gave up after {} of failures: {error}",
                crate::format::duration(Backoff::GIVE_UP)
            ))));
            return;
        };
        let at = Instant::now() + delay;
        if tx.send(Msg::Conn(Conn::Reconnecting { at, error })).is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
        if tx.is_closed() {
            return;
        }
        let _ = tx.send(Msg::Conn(Conn::Connecting));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        assert_eq!(b.failure(t0), Some(Duration::from_millis(500)));
        assert_eq!(
            b.failure(t0 + Duration::from_secs(1)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            b.failure(t0 + Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        for _ in 0..10 {
            b.failure(t0 + Duration::from_secs(3));
        }
        assert_eq!(b.failure(t0 + Duration::from_secs(4)), Some(Backoff::MAX));
        assert_eq!(b.failure(t0 + Backoff::GIVE_UP), None);
        b.success();
        assert_eq!(b.failure(t0 + Backoff::GIVE_UP), Some(Backoff::FIRST));
    }

    #[test]
    fn fatal_errors() {
        let e = |status| {
            anyhow::Error::new(HttpError {
                status,
                message: "x".into(),
            })
        };
        assert!(is_fatal(&e(401)));
        assert!(is_fatal(&e(403)));
        assert!(!is_fatal(&e(429)));
        assert!(!is_fatal(&e(503)));
        assert!(!is_fatal(&anyhow::anyhow!("connection refused")));
        assert!(describe(&e(401)).contains("yoink login"));
    }
}
