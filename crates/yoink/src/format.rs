//! Number formatting shared by the TUI and plain output.

use yoink_api::{Progress, Unit};

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

fn scale(n: u64) -> usize {
    let mut u = 0;
    let mut v = n as f64;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    u
}

fn in_unit(n: u64, u: usize) -> String {
    if u == 0 {
        n.to_string()
    } else {
        format!("{:.1}", n as f64 / 1024f64.powi(u as i32))
    }
}

/// `12.3 MiB`.
pub fn size(n: u64) -> String {
    let u = scale(n);
    format!("{} {}", in_unit(n, u), UNITS[u])
}

/// `12.3/45.6 MiB` (both in the unit of the larger), or `3/10` without a unit.
pub fn readout(p: &Progress) -> String {
    match p.unit {
        Some(Unit::Bytes) => {
            let u = scale(p.max.max(p.current));
            format!("{}/{} {}", in_unit(p.current, u), in_unit(p.max, u), UNITS[u])
        }
        None => format!("{}/{}", p.current, p.max),
    }
}

/// Truncate to `width` characters, ending in `…` when cut.
pub fn truncate(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// `1m05s`, `42s`.
pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(3 << 30), "3.0 GiB");
        let p = Progress {
            current: 1 << 20,
            max: 45 << 20,
            unit: Some(Unit::Bytes),
        };
        assert_eq!(readout(&p), "1.0/45.0 MiB");
        let p = Progress {
            current: 3,
            max: 10,
            unit: None,
        };
        assert_eq!(readout(&p), "3/10");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(duration(std::time::Duration::from_secs(65)), "1m05s");
    }
}
