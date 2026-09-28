//! Small formatting helpers shared by the messages the commands print.

use std::time::Duration;

/// A duration the way Go's `time.Duration.String()` prints one after rounding to a second:
/// `45s`, `1m0s`, `10m0s`, `1h30m0s`. The messages the Go implementation printed used it,
/// and they are kept verbatim.
pub fn go_duration(d: Duration) -> String {
    let secs = d.as_secs_f64().round() as u64;
    if secs == 0 {
        return "0s".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
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

/// A string slice the way Go's `%v` prints one: `[a b]`.
pub fn go_slice(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

/// Abbreviates a SHA for display without assuming it is well-formed.
pub fn short(sha: &str) -> &str {
    if sha.len() > 12 { &sha[..12] } else { sha }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_print_like_go() {
        assert_eq!(go_duration(Duration::from_secs(45)), "45s");
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(600)), "10m0s");
        assert_eq!(go_duration(Duration::from_secs(5400)), "1h30m0s");
        assert_eq!(go_duration(Duration::from_millis(1400)), "1s");
    }

    #[test]
    fn slices_print_like_go() {
        assert_eq!(go_slice(&["a".into(), "b".into()]), "[a b]");
        assert_eq!(go_slice(&[]), "[]");
    }
}
