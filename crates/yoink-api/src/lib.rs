//! The yoink mirroring API.
//!
//! `POST /v1/mirror` with a [`MirrorRequest`] starts (or attaches to) one
//! mirror job per PURL and answers with a Server-Sent Events stream of
//! [`Event`]s:
//!
//! 1. `phases`: the backend's [`PhaseDef`]s, always first.
//! 2. Per PURL, any number of `status` (phase and progress) and `log`
//!    events, then exactly one `result`.
//!
//! Every event after `phases` names the PURL exactly as it was given in the
//! request. The stream ends after the last `result`. Jobs belong to the
//! service, not the request: disconnecting does not stop them, and
//! re-posting the same PURLs attaches to the jobs still running, so a
//! client recovers from a dropped stream by posting again.

pub mod purl;

#[cfg(feature = "client")]
pub mod client;

use serde::{Deserialize, Serialize};

pub use purl::{NixTarget, Purl, PurlError};

pub const MIRROR_PATH: &str = "/v1/mirror";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorRequest {
    pub purls: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

/// A phase a job can be in. Phases describe work in progress; a finished
/// job is described by its [`Outcome`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseDef {
    /// Referenced by [`Event::Status`].
    pub id: String,
    /// Full name, e.g. `Transferring NAR`.
    pub name: String,
    /// One-character abbreviation for compact displays, e.g. `T`.
    pub abbrev: char,
}

impl PhaseDef {
    pub fn new(id: &str, name: &str, abbrev: char) -> Self {
        PhaseDef {
            id: id.into(),
            name: name.into(),
            abbrev,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Bytes,
}

/// Progress within a phase: `current` of `max`, in `unit` if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub current: u64,
    pub max: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
}

impl Progress {
    /// Whole percent, 0 to 100 (0 when `max` is 0).
    pub fn percent(&self) -> u8 {
        if self.max == 0 {
            0
        } else {
            ((self.current.min(self.max) as u128 * 100) / self.max as u128) as u8
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
    /// The job has failed; it will end with [`Outcome::Fatal`] (possibly
    /// after further logs).
    Fatal,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
            LogLevel::Fatal => "fatal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    /// The reason is in the job's preceding logs.
    Fatal,
}

/// An event of the mirror stream. The SSE event name is [`Event::name`];
/// its data is the JSON of the variant's fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Phases {
        phases: Vec<PhaseDef>,
    },
    Status {
        purl: String,
        /// A [`PhaseDef::id`].
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<Progress>,
    },
    Log {
        purl: String,
        level: LogLevel,
        message: String,
    },
    Result {
        purl: String,
        outcome: Outcome,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Phases { .. } => "phases",
            Event::Status { .. } => "status",
            Event::Log { .. } => "log",
            Event::Result { .. } => "result",
        }
    }

    /// The SSE `data:` payload (the fields without the tag).
    pub fn data(&self) -> String {
        let mut v = serde_json::to_value(self).expect("serialisable");
        if let Some(o) = v.as_object_mut() {
            o.remove("event");
        }
        v.to_string()
    }

    /// Parse an SSE event; `None` for event names this version doesn't know.
    pub fn from_sse(name: &str, data: &str) -> Option<serde_json::Result<Event>> {
        if !matches!(name, "phases" | "status" | "log" | "result") {
            return None;
        }
        let mut v: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        if let Some(o) = v.as_object_mut() {
            o.insert("event".into(), name.into());
        }
        Some(serde_json::from_value(v))
    }

    pub fn purl(&self) -> Option<&str> {
        match self {
            Event::Phases { .. } => None,
            Event::Status { purl, .. } | Event::Log { purl, .. } | Event::Result { purl, .. } => Some(purl),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_roundtrip() {
        let evs = [
            Event::Phases {
                phases: vec![PhaseDef::new("q", "Queued", 'Q')],
            },
            Event::Status {
                purl: "pkg:nix/x".into(),
                phase: "q".into(),
                progress: Some(Progress {
                    current: 1,
                    max: 4,
                    unit: Some(Unit::Bytes),
                }),
            },
            Event::Log {
                purl: "pkg:nix/x".into(),
                level: LogLevel::Warning,
                message: "retrying".into(),
            },
            Event::Result {
                purl: "pkg:nix/x".into(),
                outcome: Outcome::Fatal,
            },
        ];
        for e in evs {
            let d = e.data();
            assert!(!d.contains("\"event\""), "{d}");
            assert_eq!(Event::from_sse(e.name(), &d).unwrap().unwrap(), e);
        }
        assert!(Event::from_sse("future", "{}").is_none());
        assert_eq!(
            Progress {
                current: 1,
                max: 3,
                unit: None
            }
            .percent(),
            33
        );
    }
}
