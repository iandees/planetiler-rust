//! Port of `util/Format.java`: number and duration formatting for logs.
//!
//! TODO(port): remaining Format helpers (storage, numeric, percent, padding).

use std::time::Duration;

/// Format a duration like Java Planetiler: `1h2m3s`, `2m3s`, `3s`, or `0.5s`.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        let s = format!("{secs:.1}");
        return format!("{}s", if s == "0.0" { "0" } else { s.as_str() });
    }
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}s"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!("0s", duration(Duration::ZERO));
        assert_eq!("0.5s", duration(Duration::from_millis(500)));
        assert_eq!("3s", duration(Duration::from_secs(3)));
        assert_eq!("1m1s", duration(Duration::from_secs(61)));
        assert_eq!("1h0m1s", duration(Duration::from_secs(3601)));
    }
}
