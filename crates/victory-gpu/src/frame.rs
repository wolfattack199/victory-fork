//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let asked = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    // At most the interval of `VICTORY_FRAME_RATE` frames a second, if set.
    let interval = match frame_rate() {
        Some(rate) => asked.min(Duration::from_micros(1_000_000 / u64::from(rate))),
        None => asked,
    };
    tracing::info!(?asked, ?interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// The frame rate the title's clock may tick at, above the one it asks for
/// (`VICTORY_FRAME_RATE`: 31 to 240 frames a second; unset, as asked).
///
/// The title asks for a frame every 33.333 ms, the console's 30 a second,
/// and waits for each tick; that is its cap, which only matters once frames
/// are made faster. Faster ticks let it begin frames as soon as they are
/// ready, up to the rate given. Whatever in the title counts frames rather
/// than time then runs faster too: to be tested in play.
fn frame_rate() -> Option<u32> {
    static RATE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *RATE.get_or_init(|| rate_of(std::env::var("VICTORY_FRAME_RATE").ok().as_deref()))
}

/// The rate `VICTORY_FRAME_RATE` asks for: 31 to 240, else none.
fn rate_of(set: Option<&str>) -> Option<u32> {
    set?.trim().parse::<u32>().ok().filter(|r| (31..=240).contains(r))
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// The frames the title began in the last half second or more: the moment
/// and the frame counted from, and frames per thousand seconds.
static RATE: Mutex<Option<(Instant, u64, u64)>> = Mutex::new(None);

/// About how many frames the title begins in `ms` milliseconds at the pace
/// of the last half second, 1 at least and `most` at most.
///
/// Waits meant as a moment were counted in frames: four frames are 66 ms at
/// 60 a second, and half a second where the title begins eight.
pub(crate) fn frames_in(ms: u64, most: u64) -> u64 {
    let now = Instant::now();
    let frame = current();
    let mut rate = RATE.lock().unwrap_or_else(|e| e.into_inner());
    let (since, from, milli) = *rate.get_or_insert((now, frame, 60_000));
    let gone = now.saturating_duration_since(since).as_millis() as u64;
    let milli = if gone >= 500 {
        let measured = frame.saturating_sub(from).saturating_mul(1_000_000) / gone;
        *rate = Some((now, frame, measured));
        measured
    } else {
        milli
    };
    frames_at(milli, ms, most)
}

/// [`frames_in`] at `milli` frames per thousand seconds.
fn frames_at(milli: u64, ms: u64, most: u64) -> u64 {
    (milli.saturating_mul(ms) / 1_000_000).clamp(1, most.max(1))
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` took
/// (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it. A wait
/// that comes within `limit` takes the tick here, under the clock's lock,
/// so two threads waiting for one kind never take the same tick.
fn next_tick(kind: u32, limit: Duration) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let taken = last(kind).load(Ordering::Relaxed);
    let mut tick = (c.first + (elapsed / interval + 1) as u64).max(taken + 1);
    let at = u128::from(tick - c.first) * interval;
    let mut wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
    // A blocking wait for an event that has happened since the last wait
    // for it took its tick (a frame took longer than a tick) ends at once, on
    // the tick that passed: sleeping on to the next one cost 17 ms of every
    // 730 ms frame (run 58: "2.0 frame waits sleeping 16.8 ms"). Polls (a
    // time-out shorter than a tick) answer as before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1"))
        && limit >= wait
        && tick > taken + 1
    {
        tick -= 1;
        wait = Duration::ZERO;
    }
    if wait <= limit {
        last(kind).fetch_max(tick, Ordering::Relaxed);
    }
    (tick, wait)
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // The tick is only logged (diagnostics); taking it is what counts.
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (tick, wait) = next_tick(kind, limit);
    let hr = if wait > limit {
        std::thread::sleep(limit);
        TIMEOUT
    } else {
        std::thread::sleep(wait);
        S_OK
    };
    #[cfg(windows)]
    {
        let us = wait.min(limit).as_micros() as u64;
        crate::framelog::add(crate::framelog::T::PaceUs, us);
        crate::framelog::here(crate::framelog::H::PaceUs, us);
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_rate_is_asked_for_in_range() {
        assert_eq!(rate_of(None), None);
        assert_eq!(rate_of(Some("")), None);
        assert_eq!(rate_of(Some("30")), None);
        assert_eq!(rate_of(Some(" 60 ")), Some(60));
        assert_eq!(rate_of(Some("120")), Some(120));
        assert_eq!(rate_of(Some("240")), Some(240));
        assert_eq!(rate_of(Some("1000")), None);
        assert_eq!(rate_of(Some("fast")), None);
    }

    #[test]
    fn a_moment_is_fewer_frames_at_a_slow_pace() {
        // 60 a second: what the counts of frames were meant as.
        assert_eq!(frames_at(60_000, 67, 4), 4);
        assert_eq!(frames_at(60_000, 2000, 120), 120);
        // 8 a second: the same moments.
        assert_eq!(frames_at(8_000, 67, 4), 1);
        assert_eq!(frames_at(8_000, 500, 30), 4);
        assert_eq!(frames_at(8_000, 2000, 120), 16);
        // Never none, never more than asked; a title that stands still.
        assert_eq!(frames_at(0, 500, 30), 1);
        assert_eq!(frames_at(1_000_000, 2000, 120), 120);
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        assert!(before.elapsed() <= Duration::from_millis(50));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let mut token = 0;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                LAST[2].load(Ordering::Relaxed)
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP, Duration::ZERO);
        assert!(tick > flips[3]);
    }

    /// Two waits for one kind that begin together (two threads) take two
    /// ticks: the first takes its tick before it sleeps. A time-out takes
    /// none. The old code marked the tick after the sleep, so both got it.
    #[test]
    fn waits_begun_together_take_different_ticks() {
        const VBLANK: u32 = 1;
        let long = Duration::from_secs(1);
        let (first, _) = next_tick(VBLANK, long);
        let (second, _) = next_tick(VBLANK, long);
        assert!(second > first, "{second} after {first}");
        next_tick(VBLANK, Duration::ZERO);
        assert_eq!(last(VBLANK).load(Ordering::Relaxed), second);
    }
}
