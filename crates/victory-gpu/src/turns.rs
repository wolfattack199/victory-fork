//! Switches read from the environment that can also take turns: on and off
//! for a while each, so that one run measures both in the same scene (two
//! runs do not stand in the same scene at the same second).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A switch named after its environment variable: `0` off, `1` on, `alt`
/// twenty seconds one way and twenty the other, in turn, each change said in
/// the log; `alt40` the same with forty (two switches taking turns in one
/// run, one of them twice as long as the other, are seen in all four
/// combinations).
pub(crate) struct Switch {
    name: &'static str,
    /// What is done from a change to "on" on, for the log.
    what: &'static str,
    default: bool,
    /// 0 off, 1 on, else the seconds of a turn.
    mode: OnceLock<u64>,
    last: AtomicU8,
}

/// 0 off, 1 on, else the seconds of a turn.
fn mode(value: Option<&str>, default: bool) -> u64 {
    match value.map(str::trim) {
        Some("0") => 0,
        Some("1") => 1,
        Some(value) if value.starts_with("alt") => value["alt".len()..].parse().map_or(20, |seconds: u64| seconds.max(2)),
        _ => u64::from(default),
    }
}

impl Switch {
    pub(crate) const fn new(name: &'static str, default: bool, what: &'static str) -> Switch {
        Switch {
            name,
            what,
            default,
            mode: OnceLock::new(),
            last: AtomicU8::new(2),
        }
    }

    fn mode(&self) -> u64 {
        *self.mode.get_or_init(|| mode(std::env::var(self.name).ok().as_deref(), self.default))
    }

    /// Whether it is ever on.
    pub(crate) fn ever(&self) -> bool {
        self.mode() != 0
    }

    /// Whether it was set to off.
    pub(crate) fn said_off(&self) -> bool {
        static SAID: std::sync::Mutex<Vec<(&str, bool)>> = std::sync::Mutex::new(Vec::new());
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&(_, off)) = said.iter().find(|s| s.0 == self.name) {
            return off;
        }
        let off = std::env::var(self.name).is_ok_and(|value| value.trim() == "0");
        said.push((self.name, off));
        off
    }

    /// Whether it is on now.
    pub(crate) fn on(&self) -> bool {
        match self.mode() {
            0 => false,
            1 => true,
            seconds => {
                let on = (crate::host::now_us() / 1_000_000 / seconds) % 2 == 1;
                if self.last.load(Ordering::Relaxed) != u8::from(on) && self.last.swap(u8::from(on), Ordering::Relaxed) != u8::from(on) {
                    tracing::info!("{} from now ({}=alt): {}", self.what, self.name, if on { "ON" } else { "off" });
                }
                on
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_switch_is_off_on_or_takes_turns() {
        assert_eq!(super::mode(None, false), 0);
        assert_eq!(super::mode(None, true), 1);
        assert_eq!(super::mode(Some("0"), true), 0);
        assert_eq!(super::mode(Some(" 1 "), false), 1);
        assert_eq!(super::mode(Some("alt"), false), 20);
        assert_eq!(super::mode(Some("alt40"), false), 40);
        assert_eq!(super::mode(Some("alt0"), false), 2);
        assert_eq!(super::mode(Some("what"), true), 1);
    }
}
