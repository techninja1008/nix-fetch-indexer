use yoink::client::from_sse;
use yoink_api::{Event, LogLevel, Outcome, PhaseDef, Progress, Unit};
use yoink_server::api::event_data;

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
        let d = event_data(&e);
        assert!(!d.contains("\"event\""), "{d}");
        assert_eq!(from_sse(e.name(), &d).unwrap().unwrap(), e);
    }
    assert!(from_sse("future", "{}").is_none());
}
