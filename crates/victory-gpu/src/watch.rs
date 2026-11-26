//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! Writes into physical pages through another view are seen too: the
//! runtime tells us the views of the pages it wrote itself ([`writing`],
//! xmem's `write_physical` and DirectStorage's page destinations), and a
//! write by the title through any view of a guarded page faults like one
//! at the guarded address (the guard makes every view of the page
//! read-only, [`open`] gives them all back; not a page of more than
//! [`MANY_VIEWS`] views). A
//! system call writing into a guarded block fails (none is known to).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

/// A title-memory range whose changed pages another subsystem needs to read.
pub(crate) struct Tracked {
    start: usize,
    end: usize,
    seen: Mutex<BTreeMap<usize, u64>>,
}

/// Tracks a runtime GPU-memory range, including physical-page views when
/// `views` is true. Other process memory is not tracked.
pub(crate) fn track(start: usize, len: usize, views: bool) -> Option<Tracked> {
    let end = start.checked_add(len)?;
    if len == 0 || writes(start, end, views).is_none() {
        return None;
    }
    Some(Tracked {
        start,
        end,
        seen: Mutex::default(),
    })
}

/// Numbers the askings; 0 is "never".
static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The number of a new asking.
fn next_asking() -> u64 {
    ASKED.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
}

/// Page number → the asking that last found it written (0: none).
///
/// It was a map under the journal's lock, which every refresh of a host copy
/// took twice, from the submitting thread, the four beside it and the
/// relay's: 4 % of the submitting thread's time was the wait for it (run
/// m16). A page is noted under the locks of its range ([`asking`]) or the
/// journal's, as before; the number an asking gives is drawn first, then
/// its pages are noted, then the pages of its range are read: what another
/// notes meanwhile has a higher number than the asking's, or is seen by it.
/// (Who notes pages of view memory without the locks of their range,
/// [`wrote`] and [`wrote_own`], does so twice, with two numbers: an asking
/// that read before the first note has a number below the second.)
struct Writes {
    tables: [std::sync::atomic::AtomicPtr<WriteTable>; MOST_BLOCKS / WRITE_PAGES],
}

/// The pages of one table of [`Writes`]: 256 MiB of title memory.
const WRITE_PAGES: usize = 1 << 12;
type WriteTable = [std::sync::atomic::AtomicU64; WRITE_PAGES];

static NOTED: Writes = Writes {
    tables: [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; MOST_BLOCKS / WRITE_PAGES],
};

impl Writes {
    /// The table of `page`, if one of its pages was ever noted.
    fn table(&self, page: usize) -> Option<&WriteTable> {
        let at = self.tables.get(page / WRITE_PAGES)?.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: null or a table of ours; they are never freed.
        unsafe { at.as_ref() }
    }

    /// Notes `page` written at asking `now`; returns the asking that had
    /// last found it written (0: none).
    fn insert(&self, page: usize, now: u64) -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        let table = match self.table(page) {
            Some(table) => table,
            None => {
                // (Past the address space: no page of the title's.)
                let Some(place) = self.tables.get(page / WRITE_PAGES) else {
                    return 0;
                };
                let fresh: Box<WriteTable> = Box::new([const { AtomicU64::new(0) }; WRITE_PAGES]);
                let fresh = Box::into_raw(fresh);
                let made = place.compare_exchange(std::ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire);
                // SAFETY: `fresh` is ours alone if another thread was
                // first; whichever stands in the place stays for good.
                unsafe {
                    match made {
                        Ok(_) => &*fresh,
                        Err(first) => {
                            drop(Box::from_raw(fresh));
                            &*first
                        }
                    }
                }
            }
        };
        table[page % WRITE_PAGES].fetch_max(now, Ordering::AcqRel)
    }

    /// The pages of `[first, last)` that were ever noted, in order, each
    /// with its asking.
    fn range(&self, first: usize, last: usize) -> Vec<(usize, u64)> {
        let mut pages = Vec::new();
        let mut at = first;
        while at < last {
            let to = ((at / WRITE_PAGES + 1) * WRITE_PAGES).min(last);
            if let Some(table) = self.table(at) {
                let from = at % WRITE_PAGES;
                for (n, word) in table[from..from + (to - at)].iter().enumerate() {
                    match word.load(std::sync::atomic::Ordering::Acquire) {
                        0 => {}
                        asking => pages.push((at + n, asking)),
                    }
                }
            }
            at = to;
        }
        pages
    }
}

struct Journal {
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`])
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    found: Vec::new(),
    ahead: Vec::new(),
    ahead_asked: 0,
    kept: false,
});

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if j.kept {
        return;
    }
    // View memory the guards hold is left to `writes`, which answers it
    // from the guards ([`known_view`]): the kernel's write watch can only
    // fail there. Asked here with the others, a run holding such a range
    // failed, was asked again by halves down to single ranges, failing
    // each time, and the range then went to `writes` all the same (after
    // triippywhale d0581a55). `VICTORY_VIEW_ASK=1` asks as before.
    #[cfg(feature = "diagnostics")]
    let all = pieces.len();
    pieces.retain(|&(first, _)| !known_view(first * PAGE));
    #[cfg(feature = "diagnostics")]
    AHEAD_VIEWS.fetch_add((all - pieces.len()) as u64, std::sync::atomic::Ordering::Relaxed);
    j.ahead_asked = next_asking();
    j.ahead.clear();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len()
            && pieces[k].0 <= last + AHEAD_GAP
            && last.max(pieces[k].1) - first <= AHEAD_MAX
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(j, &pieces[i..k]);
        i = k;
    }
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// Since the last frame line: ranges of view memory the guards hold that a
/// submission's askings ahead ([`ask_ahead`], [`ask_once`]) left out. Each
/// was a kernel asking that failed, or several, before.
#[cfg(feature = "diagnostics")]
pub(crate) static AHEAD_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        j.ahead.clear();
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`VICTORY_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    ask_ahead(ranges);
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    if j.ahead.is_empty() {
        // `ask_ahead` asked nothing (fewer than two ranges).
        j.ahead_asked = next_asking();
    }
    // `ask_ahead` leaves a range that lies alone to `writes`, which asks
    // once per flush: with one asking for the submission that was every
    // such range asked again at each of its 70 flushes (run 76: 2,300
    // askings a frame with the switch on, as without it). They are asked
    // here, once; what is not write-watched stays with `writes`.
    for &(start, end) in ranges {
        if start >= end {
            continue;
        }
        // View memory the guards hold is not asked about: the asking can
        // only fail there, one failed kernel call for each such copy at
        // every submission, and `writes` answers it from the guards
        // (after triippywhale cc368c7c).
        if known_view(start) {
            #[cfg(feature = "diagnostics")]
            AHEAD_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b) && ask_kernel(j, first, last) {
            j.ahead.push((first, last));
        }
    }
    j.kept = !j.ahead.is_empty();
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if j.kept {
        j.kept = false;
        j.ahead.clear();
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    if !j.kept || len == 0 {
        return;
    }
    let pages = || at / PAGE..at.saturating_add(len).div_ceil(PAGE);
    let now = next_asking();
    for page in pages() {
        NOTED.insert(page, now);
    }
    // (Once more: [`Writes`].)
    let now = next_asking();
    // The asking that stands moves with it: `asked` and `writes` give it as
    // "now", and a page noted written after "now" stays stale for every
    // copy that takes it (copied at `now`, written at `now + 1`): copied
    // again at every refresh, and a reason to settle flights
    // (fxceless/victory-fork#410).
    j.ahead_asked = now;
    for page in pages() {
        NOTED.insert(page, now);
    }
}

/// We wrote the 64 KiB page numbered `page` of title memory through a
/// view of our own (`crate::inplace::write_own`): no guard was opened and
/// no kernel write-watch bit set, so the journal is told here. The page,
/// and the same physical page at its other title addresses (`others`,
/// page numbers), count as written at a new asking, which every host copy
/// that holds them takes at its next refresh. Returns the asking that had
/// last found `page` written (0: never) and the new one, for the copy
/// that wrote to note itself current.
pub(crate) fn wrote_own(page: usize, others: &[usize]) -> (u64, u64) {
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    let pages = || std::iter::once(page).chain(others.iter().copied().filter(|&other| other != page));
    let mut before = None;
    let now = next_asking();
    for page in pages() {
        let was = NOTED.insert(page, now);
        before.get_or_insert(was);
    }
    // (Once more: [`Writes`].)
    let now = next_asking();
    // As in [`wrote`]: an asking that stands for a submission, or for a
    // flush, moves with it, or a copy taking the page at the standing
    // asking would find it written "later" at every refresh.
    if j.kept || !j.ahead.is_empty() {
        j.ahead_asked = now;
    }
    for page in pages() {
        NOTED.insert(page, now);
    }
    (before.unwrap_or(0), now)
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`].
fn ask_run(j: &mut Journal, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a);
    ask_run(j, b);
}

/// Asks the kernel for the written pages of pages `[first, last)` and notes
/// them in the journal under the ahead asking; false (and nothing changed)
/// if the range is not write-watched memory of one reservation.
fn ask_kernel(j: &mut Journal, first: usize, last: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            j.found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return false;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    let now = j.ahead_asked;
    for k in 0..count {
        let page = j.found[k] / PAGE;
        NOTED.insert(page, now);
    }
    true
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    j.ahead
        .iter()
        .any(|&(a, b)| a <= first && last <= b)
        .then(|| (j.ahead_asked, NOTED.range(first, last)))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    writes_held(start, end, guarded).map(|(now, pages, _)| (now, pages))
}

/// Pages of one lock of [`ASKING`]: 2 MiB of title memory.
const ASKING_PAGES: usize = 32;
/// Held while the guards are asked about a range, one for every 2 MiB of
/// it (by their number, its low bits), in the order of the array.
static ASKING: [Mutex<()>; 512] = [const { Mutex::new(()) }; 512];

/// The locks of [`ASKING`] for the pages `[first, last)`.
fn asking(first: usize, last: usize) -> Vec<std::sync::MutexGuard<'static, ()>> {
    let (a, b) = (first / ASKING_PAGES, last.max(first + 1).div_ceil(ASKING_PAGES));
    let mut places: Vec<usize> = if b - a >= ASKING.len() {
        (0..ASKING.len()).collect()
    } else {
        (a..b).map(|n| n % ASKING.len()).collect()
    };
    places.sort_unstable();
    places.into_iter().map(|n| ASKING[n].lock().unwrap_or_else(|e| e.into_inner())).collect()
}

/// Whether the guards are asked under the locks of the range alone
/// (`VICTORY_ASK_APART=0`: under the journal's, all of it, as before).
///
/// Every refresh of a host copy asks here, from the submitting thread and
/// the four that refresh beside it, and an asking of the guards makes
/// blocks read-only again and asks Windows what a hole is: 23 ms a frame in
/// the kernel, with the journal's one lock held and the other threads in
/// line for it. The five of them stood there 160 to 180 ms a frame, a
/// quarter of the submitting thread's time. The journal is now held only to
/// look into it and to write into it.
///
/// Askings of the same page still follow one another: a page found written
/// is in the journal, with a number higher than any asking of it before,
/// when the locks of its range are let go.
fn ask_apart() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_APART").is_none_or(|v| v != "0"))
}

/// The journal's answer for pages `[first, last)`, from the guards' `found`
/// pages: a new asking.
fn noted(first: usize, last: usize, found: Vec<usize>) -> (u64, Vec<(usize, u64)>) {
    let now = next_asking();
    for page in found {
        NOTED.insert(page, now);
    }
    (now, NOTED.range(first, last))
}

/// Since the last frame-rate line: 4 KiB pages the kernel's write
/// watch found written, the 64 KiB pages they lie in (what a refresh copies
/// for them), and 64 KiB blocks the guards found written.
pub(crate) static GRAIN: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// [`writes`], and whether every block of the range is guarded and clean
/// from here on: until its count changes ([`changes`]) nothing is written.
pub(crate) fn writes_held(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>, bool)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    // Whatever asks the guards about a page does it under the locks of its
    // range, whichever way it takes below: a page one asking has found
    // written, and not noted yet, is clean to the guards, and another that
    // found it so would take its copy for current.
    let waiting = std::time::Instant::now();
    let range = (guarded && ask_apart()).then(|| asking(first, last));
    if range.is_some() && known_view(first * PAGE) {
        // (Not asked whether it was asked ahead: view memory is not.)
        count_us(8, waiting);
        let guarding = std::time::Instant::now();
        let found = guard_held(first * PAGE, last * PAGE);
        count(0, 1);
        count_us(1, guarding);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardUs, guarding.elapsed().as_micros() as u64);
        let (found, clean) = found?;
        GRAIN[2].fetch_add(found.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let (now, pages) = noted(first, last, found);
        drop(range);
        return Some((now, pages, clean));
    }
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    count_us(8, waiting);
    // Asked ahead with the submission's other ranges: the journal has it.
    if j.ahead.iter().any(|&(a, b)| a <= first && last <= b) {
        return Some((j.ahead_asked, NOTED.range(first, last), false));
    }
    let mut found = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(found.max(j.found.len()), 0);
    let mut granularity = 0;
    let started = std::time::Instant::now();
    // View memory the guards already hold cannot be write-watched: the
    // kernel asking would only fail. Copies of it are flushed many times a
    // frame, each time paying that failed call before the guards answer.
    let known_view = guarded && known_view(first * PAGE);
    // SAFETY: the answer buffer holds `found` entries.
    let failed = known_view
        || unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut found,
                &mut granularity,
            )
        } != 0;
    if !known_view {
        count(6, 1);
        count_us(7, started);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        if known_view {
            crate::copystat::C::WatchKnownView
        } else {
            crate::copystat::C::WatchFailed
        },
        failed as u64,
    );
    #[cfg(feature = "diagnostics")]
    if failed && !known_view && guarded {
        failed_asking(first * PAGE, last * PAGE);
    }
    let (guarded, held) = if failed {
        found = 0;
        if !guarded {
            return None;
        }
        let guarding = std::time::Instant::now();
        let written = guard_held(first * PAGE, last * PAGE);
        count(0, 1);
        count_us(1, guarding);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::GuardUs, guarding.elapsed().as_micros() as u64);
        written?
    } else {
        (Vec::new(), false)
    };
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        let mut pages = 0;
        let mut last_page = usize::MAX;
        for &at in &j.found[..found] {
            if at / PAGE != last_page {
                last_page = at / PAGE;
                pages += 1;
            }
        }
        GRAIN[0].fetch_add(found as u64, Relaxed);
        GRAIN[1].fetch_add(pages, Relaxed);
        GRAIN[2].fetch_add(guarded.len() as u64, Relaxed);
    }
    let now = next_asking();
    for page in j.found[..found].iter().map(|&at| at / PAGE).chain(guarded) {
        NOTED.insert(page, now);
    }
    Some((now, NOTED.range(first, last), held))
}

/// Diagnostics: one in 2,048 of the askings the kernel's write watch
/// refused for memory the guards did not know (some 200 a frame in the
/// first rooms, each followed by a question to Windows about the memory):
/// what the memory is and what the guards hold of its first block.
#[cfg(feature = "diagnostics")]
fn failed_asking(start: usize, end: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 2048 != 0 {
        return;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(start as *const _, &mut info, size_of_val(&info)) };
    let [protected, clean, absent, viewed, aliased] =
        [PROTECTED, CLEAN, ABSENT, VIEWED, ALIASED].map(|bit| BLOCKS.get(start / BLOCK) & bit != 0);
    tracing::info!(
        "a failed write-watch asking (one in 2048): {start:#x}+{:#x}; Windows says (answered {n}): base {:#x} region {:#x} state {:#x} type {:#x} protect {:#x} allocation protect {:#x}; \
         the guards of its first block: protected {protected} clean {clean} absent {absent} viewed {viewed} aliased {aliased}; is a view: {}",
        end - start,
        info.BaseAddress as usize,
        info.RegionSize,
        info.State,
        info.Type,
        info.Protect,
        info.AllocationProtect,
        is_view(start, end)
    );
}

impl Tracked {
    /// Takes changed pages of `[from, to)`, cut to the tracked range. A
    /// partial page stays changed so a later take can read its other bytes.
    pub(crate) fn take(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return Vec::new();
        }
        let Some((_, pages)) = writes(from, to, true) else {
            return Vec::new();
        };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (page, write) in pages {
            if seen.get(&page).is_some_and(|&last| last >= write) {
                continue;
            }
            let (start, end) = ((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
            if start >= end {
                continue;
            }
            // Keep partial pages dirty: the caller has not taken the rest.
            if (page * PAGE).max(self.start) >= from
                && ((page + 1) * PAGE).min(self.end) <= to
            {
                seen.insert(page, write);
            }
            match ranges.last_mut() {
                Some((_, last)) if *last == start => *last = end,
                _ => ranges.push((start, end)),
            }
        }
        ranges
    }
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

/// For the log, since the last line: askings of the guards and their
/// microseconds, blocks made read-only and the calls that did it, faults
/// taken, blocks opened, askings of the kernel and their microseconds, and
/// the microseconds waited for the journal.
/// Last: refreshes answered without an asking ([`changes`]).
pub(crate) static GUARD_STATS: [std::sync::atomic::AtomicU64; 10] = [const { std::sync::atomic::AtomicU64::new(0) }; 10];

/// A refresh that asked nobody ([`changes`]).
pub(crate) fn quiet() {
    count(9, 1);
}

/// How often each guarded block stopped being as its copies took it
/// (written, opened, mapped anew): by block number, the low bits. Two blocks
/// may share a count: one is then asked about for the other.
///
/// Who found a block clean ([`writes_held`]) and finds its count unchanged
/// knows without a lock that nothing happened to it: a block leaves the
/// clean ones only where its count goes up. 1,800 askings a frame were
/// answered under two locks, with 115 to 175 ms a frame waiting for one
/// another, and nearly all of them "nothing".
static CHANGES: [std::sync::atomic::AtomicU32; 1 << 18] = [const { std::sync::atomic::AtomicU32::new(0) }; 1 << 18];

fn change(block: usize) {
    CHANGES[block & (CHANGES.len() - 1)].fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// The count of the block that is page `page` ([`CHANGES`]). Taken before an
/// asking, it stands for what the asking found.
pub(crate) fn changes(page: usize) -> u32 {
    const { assert!(PAGE == BLOCK) };
    CHANGES[page & (CHANGES.len() - 1)].load(std::sync::atomic::Ordering::Acquire)
}

/// All counts of `[start, end)` as one: unchanged exactly if each of them is
/// (they only go up).
pub(crate) fn changes_all(start: usize, end: usize) -> u64 {
    (start / PAGE..end.div_ceil(PAGE)).fold(0, |sum, page| sum.wrapping_add(u64::from(changes(page))))
}

fn count(i: usize, n: u64) {
    GUARD_STATS[i].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// For the log, since the last line: guard faults; of them, faults on a
/// block that faulted in the frame before too, in this frame before, and
/// two to four frames before. A block the title writes every frame pays a
/// guard and a fault each frame, where copying it whole each time might
/// cost less.
pub(crate) static FAULT_AGAIN: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The last fault of each block, as (block << 24 | frame), by the block's
/// low bits: blocks that share a slot only make a repeat look new.
static LAST_FAULT: [std::sync::atomic::AtomicU64; 1 << 16] = [const { std::sync::atomic::AtomicU64::new(0) }; 1 << 16];

/// The frame numbers [`LAST_FAULT`] keeps.
const FRAME_BITS: u64 = 0xff_ffff;

/// Notes a guard fault on `block` for [`FAULT_AGAIN`]. No allocation and
/// no lock: it runs in the fault handler.
fn fault_again(block: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let frame = crate::frame::current() & FRAME_BITS;
    let was = LAST_FAULT[block & 0xffff].swap(((block as u64) << 24) | frame, Relaxed);
    FAULT_AGAIN[0].fetch_add(1, Relaxed);
    if was >> 24 != block as u64 || was == 0 {
        return;
    }
    let before = frame.wrapping_sub(was & FRAME_BITS) & FRAME_BITS;
    match before {
        0 => FAULT_AGAIN[2].fetch_add(1, Relaxed),
        1 => FAULT_AGAIN[1].fetch_add(1, Relaxed),
        2..=4 => FAULT_AGAIN[3].fetch_add(1, Relaxed),
        _ => 0,
    };
}

/// For how many frames a block that took a guard fault is left open
/// (`VICTORY_GUARD_HOT`; 0, the default, none, as before). Such a block is
/// not made read-only again: every asking counts it written, so its copies
/// take it whole each time, and no write to it can be missed. Once the
/// frames are over the next asking guards it again.
///
/// Of the guard faults of run c51, 12 to 29 % were on a block that faulted
/// in the frame before too, 3 to 30 % on one that faulted earlier in the
/// same frame, 25 to 53 % on one that faulted two to four frames before:
/// the title writes the same blocks frame after frame, and each time pays
/// for making them read-only (about 35 µs a call, with the address
/// space's lock that the title's remaps wait for) and for the fault that
/// opens them again; a copy of 64 KiB is about 10 µs.
fn hot_frames() -> u64 {
    #[cfg(test)]
    if let Some(frames) = tests::HOT.with(std::cell::Cell::get) {
        return frames;
    }
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| std::env::var("VICTORY_GUARD_HOT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// Blocks left open by [`hot_frames`] at an asking, since the last line.
pub(crate) static HOT_LEFT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether `block` took a guard fault in the last [`hot_frames`] frames
/// (as [`LAST_FAULT`] has it: a block whose slot another took counts as
/// not, and is guarded as before).
fn hot(block: usize) -> bool {
    let frames = hot_frames();
    if frames == 0 {
        return false;
    }
    let was = LAST_FAULT[block & 0xffff].load(std::sync::atomic::Ordering::Relaxed);
    if was == 0 || was >> 24 != block as u64 {
        return false;
    }
    let frame = crate::frame::current() & FRAME_BITS;
    frame.wrapping_sub(was & FRAME_BITS) & FRAME_BITS < frames
}

fn count_us(i: usize, since: std::time::Instant) {
    count(i, since.elapsed().as_micros() as u64);
}

/// Whether the block holding `start` is view memory the guards already
/// hold (read-only by our hand, or noted absent): asking the kernel's write
/// watch about it can only fail. `VICTORY_VIEW_ASK=1` (or
/// `VICTORY_GUARD_ASK=1`) asks the kernel anyway, as before.
fn known_view(start: usize) -> bool {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ask = *ASK.get_or_init(|| {
        ["VICTORY_VIEW_ASK", "VICTORY_GUARD_ASK"]
            .iter()
            .any(|n| std::env::var_os(n).is_some_and(|v| v == "1"))
    });
    if ask {
        return false;
    }
    held_view(start / BLOCK)
}

/// [`known_view`] for a block, whatever the switches say.
fn held_view(block: usize) -> bool {
    let known = BLOCKS.get(block);
    known & (PROTECTED | ABSENT) != 0 || (view_known() && known & VIEWED != 0)
}

// What the guards know of a 64 KiB block, as bits:
/// Made read-only by our hand.
const PROTECTED: u8 = 1;
/// Of those, not written since a copy took it.
const CLEAN: u8 = 2;
/// Already copied as zeros. While it remains wholly uncommitted, the copy
/// can skip hundreds of MB of failing kernel reads per world frame.
const ABSENT: u8 = 4;
/// Its physical page was guarded together with its other views (those
/// views' blocks have the bit too): a write to one opens them all. A write
/// range can be huge (`fread`), so only these are looked up.
const ALIASED: u8 = 8;
/// With [`view_known`]: made read-only at least once since the runtime last
/// mapped or unmapped it. It is view memory whether its guard stands now or
/// a write has opened it.
const VIEWED: u8 = 16;

/// Blocks of one table of [`Blocks`]: 4 GiB of address space.
const TABLE_BLOCKS: usize = 1 << 16;
/// All blocks there are: 47 bits of address.
const MOST_BLOCKS: usize = 1 << 31;
/// Blocks of a region, which one lock ([`SHARDS`]) goes for: 16 MiB.
const REGION: usize = 256;
/// Eight blocks' bits in a word.
type Table = [std::sync::atomic::AtomicU64; TABLE_BLOCKS / 8];

/// The bits of every block. Read without a lock; changed only under the
/// lock of the block's region ([`hold`]).
///
/// They were four sets and a map of runs behind one lock, which every
/// asking took twice, every fault and every mapping once, and which an
/// asking held while Windows made its blocks read-only: the submitting
/// thread stood in line for it for a tenth of its time once the journal's
/// lock no longer stood before it. Nearly every question is about a range
/// that is clean, and that is some hundred words to look at.
///
/// A block stops being clean, and its count goes up ([`change`]), before
/// it is made writable: who reads that it is clean, with its count taken
/// first, knows that no write to it has come through.
struct Blocks {
    tables: [std::sync::atomic::AtomicPtr<Table>; MOST_BLOCKS / TABLE_BLOCKS],
}

static BLOCKS: Blocks = Blocks { tables: [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; MOST_BLOCKS / TABLE_BLOCKS] };

/// `bits` in every one of a word's eight blocks.
const fn eight(bits: u8) -> u64 {
    0x0101_0101_0101_0101 * bits as u64
}

/// All bits of a word's blocks `a` to `b` (exclusive; at most eight).
fn blocks_mask(a: usize, b: usize) -> u64 {
    let below = |n: usize| if n >= 8 { u64::MAX } else { (1u64 << (n * 8)) - 1 };
    below(b) & !below(a)
}

impl Blocks {
    /// The table of `block`, if one of its blocks ever had a bit.
    fn table(&self, block: usize) -> Option<&Table> {
        let at = self.tables.get(block / TABLE_BLOCKS)?.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: null or a table of ours; they are never freed.
        unsafe { at.as_ref() }
    }

    /// The table of `block`, made if there is none (`None`: past the address
    /// space).
    fn table_made(&self, block: usize) -> Option<&Table> {
        use std::sync::atomic::{AtomicU64, Ordering};
        if let Some(table) = self.table(block) {
            return Some(table);
        }
        let place = self.tables.get(block / TABLE_BLOCKS)?;
        let fresh: Box<Table> = Box::new([const { AtomicU64::new(0) }; TABLE_BLOCKS / 8]);
        let fresh = Box::into_raw(fresh);
        let made = place.compare_exchange(std::ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire);
        // SAFETY: `fresh` is ours alone if another thread was first;
        // whichever stands in the place stays for good.
        unsafe {
            match made {
                Ok(_) => fresh.as_ref(),
                Err(first) => {
                    drop(Box::from_raw(fresh));
                    first.as_ref()
                }
            }
        }
    }

    /// The bits of `block`.
    fn get(&self, block: usize) -> u8 {
        self.table(block).map_or(0, |t| (t[block % TABLE_BLOCKS / 8].load(std::sync::atomic::Ordering::Acquire) >> (block % 8 * 8)) as u8)
    }

    /// Sets `bits` of the blocks `[from, to)`.
    fn set(&self, from: usize, to: usize, bits: u8) {
        let (mut at, to) = (from, to.min(MOST_BLOCKS));
        while at < to {
            let base = at & !7;
            let end = (base + 8).min(to);
            if let Some(table) = self.table_made(at) {
                table[base % TABLE_BLOCKS / 8].fetch_or(eight(bits) & blocks_mask(at - base, end - base), std::sync::atomic::Ordering::SeqCst);
            }
            at = end;
        }
    }

    /// Clears `bits` of `block`: those of them it had.
    fn clear(&self, block: usize, bits: u8) -> u8 {
        let Some(table) = self.table(block) else {
            return 0;
        };
        let shift = block % 8 * 8;
        let before = table[block % TABLE_BLOCKS / 8].fetch_and(!(u64::from(bits) << shift), std::sync::atomic::Ordering::SeqCst);
        (before >> shift) as u8 & bits
    }

    /// The words that hold blocks of `[from, to)`: each with its first
    /// block, its value, and all bits of those of its blocks that are
    /// meant. Stretches without a table are left out. Stops where `f`
    /// returns false.
    fn words(&self, from: usize, to: usize, mut f: impl FnMut(usize, u64, u64) -> bool) {
        let (mut at, to) = (from, to.min(MOST_BLOCKS));
        while at < to {
            let stop = ((at / TABLE_BLOCKS + 1) * TABLE_BLOCKS).min(to);
            let Some(table) = self.table(at) else {
                at = stop;
                continue;
            };
            while at < stop {
                let base = at & !7;
                let end = (base + 8).min(stop);
                let word = table[base % TABLE_BLOCKS / 8].load(std::sync::atomic::Ordering::Acquire);
                if !f(base, word, blocks_mask(at - base, end - base)) {
                    return;
                }
                at = end;
            }
        }
    }

    /// Whether every block of `[from, to)` has `bit`.
    fn all(&self, from: usize, to: usize, bit: u8) -> bool {
        let mut meant = 0;
        let mut all = true;
        self.words(from, to, |_, word, mask| {
            meant += mask.count_ones() as usize / 8;
            all = word & mask & eight(bit) == mask & eight(bit);
            all
        });
        all && meant == to.saturating_sub(from)
    }

    /// Whether any block of `[from, to)` has one of `bits`.
    fn any(&self, from: usize, to: usize, bits: u8) -> bool {
        let mut any = false;
        self.words(from, to, |_, word, mask| {
            any = word & mask & eight(bits) != 0;
            !any
        });
        any
    }

    /// The blocks of `[from, to)` with one of `bits`, in order.
    fn with(&self, from: usize, to: usize, bits: u8) -> Vec<usize> {
        let mut out = Vec::new();
        self.words(from, to, |base, word, mask| {
            let mut left = word & mask & eight(bits);
            while left != 0 {
                let n = left.trailing_zeros() as usize / 8;
                out.push(base + n);
                left &= !(0xff << (n * 8));
            }
            true
        });
        out
    }

    /// The runs of blocks of `[from, to)` without `bit`, in order.
    fn without(&self, from: usize, to: usize, bit: u8) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut next = from;
        for block in self.with(from, to, bit) {
            if next < block {
                out.push((next, block));
            }
            next = block + 1;
        }
        if next < to {
            out.push((next, to));
        }
        out
    }
}

/// One lock for every 64th region of 16 MiB: whoever changes the bits of a
/// block ([`BLOCKS`]), or its protection, holds its region's.
static SHARDS: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];

/// The lock of `block`'s region, as a bit of a set of them.
fn shard(block: usize) -> u64 {
    1 << (block / REGION % SHARDS.len())
}

/// Takes the locks of the set `shards`, in the order of [`SHARDS`].
///
/// A block's other views may lie in other regions, and which those are can
/// only be asked with its own lock held (a view mapped meanwhile must not be
/// missed). Who finds that it needs more lets go and takes them all anew.
fn hold(shards: u64) -> [Option<std::sync::MutexGuard<'static, ()>>; 64] {
    let mut held = [const { None }; 64];
    let mut left = shards;
    while left != 0 {
        let n = left.trailing_zeros() as usize;
        held[n] = Some(SHARDS[n].lock().unwrap_or_else(|e| e.into_inner()));
        left &= left - 1;
    }
    held
}

/// Calls `f` with the part of `[first, last)` in each region. Of a huge
/// range (the length is sometimes the title's, saturated) only where the
/// guards hold anything.
fn each_region(first: usize, last: usize, mut f: impl FnMut(usize, usize)) {
    let last = last.min(MOST_BLOCKS);
    let many = last.saturating_sub(first) > 64 * REGION;
    let mut at = first;
    while at < last {
        let to = ((at / REGION + 1) * REGION).min(last);
        if many && BLOCKS.table(at).is_none() {
            at = ((at / TABLE_BLOCKS + 1) * TABLE_BLOCKS).min(last);
            continue;
        }
        if !many || BLOCKS.any(at, to, u8::MAX) {
            f(at, to);
        }
        at = to;
    }
}

/// Whether a block once guarded stays known as view memory while its
/// guard is open (`VICTORY_VIEW_KNOWN=1`; off by default).
///
/// A range whose first block the title wrote since the last check has
/// that block neither read-only nor noted absent, so nothing says it is a
/// view: the kernel's write watch is asked and fails, and then Windows is
/// asked what the memory is (`is_view`), a walk of the page tables to the
/// end of the pages that share the first one's state. In runs 222 and 225
/// that was 338-450 failed askings a frame and 6.5-7.8 ms a frame of the
/// submitting thread in `VirtualQuery`. With the switch the block is
/// remembered from the time it was first made read-only until the runtime
/// maps or unmaps it ([`title_memory_remapped`]).
///
/// `VICTORY_VIEW_KNOWN=alt`: twenty seconds with, twenty without, in turn,
/// each change said in the log, for measuring it in one scene. (Blocks
/// noted in one phase are forgotten at a remapping in either.)
fn view_known() -> bool {
    /// 0 off, 1 on, 2 in turn.
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    match *MODE.get_or_init(|| match std::env::var("VICTORY_VIEW_KNOWN").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    }) {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "view memory stays known while its guard is open from now (VICTORY_VIEW_KNOWN=alt): {}",
                    if on { "ON" } else { "off, as usual" }
                );
            }
            on
        }
    }
}

/// Whether the block `page` is read-only by our hand and unwritten since a
/// copy took it (diagnostics of `VICTORY_BACK_OWN_CHECK`).
pub(crate) fn clean(page: usize) -> bool {
    BLOCKS.get(page) & CLEAN != 0
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them; and whether all its blocks are guarded
/// and clean now. `None` if the memory is not a view.
fn guard_held(start: usize, end: usize) -> Option<(Vec<usize>, bool)> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows. The
    // question walks the page tables to the end of the pages that share the
    // first one's state, and a guarded view is one long run of read-only
    // pages: 0.6 ms for the title's 174 MB heap, 16 times a frame, and most
    // of the 68 ms a frame of "guards" (run 58).
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let known = !*ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1")) && held_view(start / BLOCK);
    if !known && !is_view(start, end) {
        return None;
    }
    install();
    let mut written = Vec::new();
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Nearly all of a range is clean (a frame asks about some 10 GB of view
    // memory, 150,000 blocks): only the gaps between clean runs are walked,
    // and only regions with one are locked.
    let mut uncommitted_until = first;
    let mut held = true;
    let mut at = first;
    while at < last {
        let to = ((at / REGION + 1) * REGION).min(last);
        if !BLOCKS.all(at, to, CLEAN) {
            guard_region(at, to, &mut written, &mut uncommitted_until);
            held = held && BLOCKS.all(at, to, CLEAN);
        }
        at = to;
    }
    Some((written, held))
}

/// [`guard_held`] for `[from, to)` within one region.
fn guard_region(from: usize, to: usize, written: &mut Vec<usize>, uncommitted_until: &mut usize) {
    let mut shards = shard(from);
    loop {
        let _held = hold(shards);
        let gaps = BLOCKS.without(from, to, CLEAN);
        // The other views of the blocks that may be made read-only here.
        let mut others: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut need = 0;
        if view_guard() {
            for &(a, b) in &gaps {
                for (base, bytes) in crate::inplace::views_between(a.saturating_mul(BLOCK), b.saturating_mul(BLOCK)) {
                    let first = (base / BLOCK).max(a);
                    let last = (base.saturating_add(bytes).div_ceil(BLOCK)).min(b);
                    for block in first..last {
                        if BLOCKS.get(block) & PROTECTED != 0 {
                            continue;
                        }
                        let shown = crate::inplace::pages_of(block * BLOCK);
                        // One view (nothing else to guard), or the page the
                        // title shows wherever it has nothing (thousands of
                        // views): each view of that one is guarded by itself.
                        if shown.len() < 2 || shown.len() > MANY_VIEWS {
                            continue;
                        }
                        let shown: Vec<usize> = shown.into_iter().map(|at| at / BLOCK).collect();
                        need |= shown.iter().fold(0, |set, &alias| set | shard(alias));
                        others.push((block, shown));
                    }
                }
            }
        }
        if need & !shards != 0 {
            shards |= need;
            continue;
        }
        for (a, b) in gaps {
            let mut run = None;
            for block in a..b {
                let known = BLOCKS.get(block);
                let mut open = true;
                if known & ABSENT != 0 {
                    // Ordinary commits need no runtime mapping notification. Reuse
                    // this asking's query for the following blocks in the same region.
                    if block >= *uncommitted_until {
                        *uncommitted_until = uncommitted_end(block);
                    }
                    open = block >= *uncommitted_until;
                    if open {
                        BLOCKS.clear(block, ABSENT);
                    }
                }
                if open {
                    written.extend(block * BLOCK / PAGE..(block + 1) * BLOCK / PAGE);
                    if known & PROTECTED != 0 {
                        // Written through the runtime, still read-only.
                        BLOCKS.set(block, block + 1, CLEAN);
                    } else if hot(block) {
                        // Left open: counted written at every asking
                        // while it is hot (the run before it is guarded).
                        HOT_LEFT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        run.get_or_insert(block);
                        continue;
                    }
                }
                if let Some(begin) = run.take() {
                    protect(begin, block, &others);
                }
            }
            if let Some(begin) = run {
                protect(begin, b, &others);
            }
        }
        return;
    }
}

#[cfg(test)]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    guard_held(start, end).map(|(written, _)| written)
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // A view the runtime told us of: no question to Windows, which walks
    // the page tables with the address space's lock held (the title's
    // mappings waited for it).
    if crate::inplace::in_view(start) {
        return true;
    }
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes = *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// block by block if they span views, leaving out any that fail, and
/// noting those not committed (unmapped) absent. The same physical pages
/// at every other view of them are made read-only too: a write through
/// one of those is this memory written, and only a fault can say so.
/// `VICTORY_VIEW_GUARD=0` leaves the other views as they are, as before.
///
/// Under the locks of the blocks and of `others`: those blocks of them that
/// have other views, each with all its views' blocks.
fn protect(from: usize, to: usize, others: &[(usize, Vec<usize>)]) {
    protect_here(from, to);
    for (block, shown) in others.iter().filter(|o| (from..to).contains(&o.0)) {
        #[cfg(feature = "diagnostics")]
        ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for &alias in shown {
            BLOCKS.set(alias, alias + 1, ALIASED);
            if alias != *block {
                protect_alias(alias);
            }
        }
    }
}

/// Views of one page past which its views are not guarded together.
pub(crate) const MANY_VIEWS: usize = 64;

/// Whether all the views of a guarded page are guarded together
/// (`VICTORY_VIEW_GUARD`).
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

/// Diagnostics: blocks guarded together with their other views, blocks
/// opened because another view of theirs was written, and blocks opened
/// because a view of theirs was mapped; since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Makes `block` read-only: another view of pages a copy has just taken
/// through its own view, so that a write through this one faults too. It is
/// not noted clean. The copies of title memory at this view have not taken
/// the pages: noted clean, the block would not be reported written at their
/// next check and they would keep the bytes from before the write (`guard`
/// reports a block that is read-only and not clean as written, then notes it
/// clean). A block that is clean stays so: had the pages been written since
/// its copies took them, `open` would have opened it.
fn protect_alias(block: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    if BLOCKS.get(block) & PROTECTED != 0 {
        return;
    }
    let mut old = 0;
    // SAFETY: title view memory; only its protection changes.
    if unsafe { VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old) } != 0 {
        BLOCKS.set(block, block + 1, if view_known() { PROTECTED | VIEWED } else { PROTECTED });
    }
}

/// [`protect`] for one range, without the other views of its pages.
fn protect_here(from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title view memory; only its protection changes.
        unsafe {
            VirtualProtect(
                (from * BLOCK) as *const _,
                (to - from) * BLOCK,
                PAGE_READONLY,
                &mut old,
            ) != 0
        }
    };
    let note = |from: usize, to: usize| {
        count(2, (to - from) as u64);
        count(3, 1);
        BLOCKS.set(from, to, if view_known() { PROTECTED | CLEAN | VIEWED } else { PROTECTED | CLEAN });
    };
    if set(from, to) {
        note(from, to);
        return;
    }
    for block in from..to {
        if set(block, block + 1) {
            note(block, block + 1);
        } else {
            #[cfg(feature = "diagnostics")]
            let error = std::io::Error::last_os_error();
            if block < uncommitted_end(block) {
                BLOCKS.set(block, block + 1, ABSENT);
            } else {
                // Diagnostics: a committed block that will not be made
                // read-only is reported written at every asking and never
                // known to the guards (one in 1,024 said).
                #[cfg(feature = "diagnostics")]
                {
                    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 1024 == 0 {
                        tracing::info!("a block of title memory could not be made read-only (one in 1024): {:#x}: {error}", block * BLOCK);
                    }
                }
            }
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same physical pages at
/// every other view of them are opened too: the write that faulted (or is
/// ours, `crate::host::copy_mapped`) goes through one view, and every
/// guard over the pages must copy them again.
fn open(start: usize, end: usize) -> bool {
    let mut any = false;
    each_region(start / BLOCK, end.div_ceil(BLOCK), |from, to| {
        let mut shards = shard(from);
        loop {
            let _held = hold(shards);
            match open_held(from, to, start < end, shards) {
                Ok(opened) => {
                    any |= opened;
                    return;
                }
                Err(need) => shards |= need,
            }
        }
    });
    any
}

/// [`open`] for blocks of one region, with the locks `shards` held, and
/// with the other views of the pages if `others`. `Err`: those lie under
/// more locks, the whole set, and nothing was done.
fn open_held(from: usize, to: usize, others: bool, shards: u64) -> Result<bool, u64> {
    let aliased = if others { BLOCKS.with(from, to, ALIASED) } else { Vec::new() };
    let (shown, need) = views_of(&aliased);
    if need & !shards != 0 {
        return Err(need);
    }
    let any = open_here(from, to);
    for (block, shown) in aliased.into_iter().zip(shown) {
        BLOCKS.clear(block, ALIASED);
        for alias in shown {
            if alias != block && open_here(alias, alias + 1) {
                #[cfg(feature = "diagnostics")]
                ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            BLOCKS.clear(alias, ALIASED);
        }
    }
    Ok(any)
}

/// For each of `blocks`, the blocks of all views of its page; and the locks
/// of all those.
fn views_of(blocks: &[usize]) -> (Vec<Vec<usize>>, u64) {
    let mut need = 0;
    let shown = blocks
        .iter()
        .map(|&block| {
            let shown: Vec<usize> = crate::inplace::pages_of(block * BLOCK).into_iter().map(|at| at / BLOCK).collect();
            need |= shown.iter().fold(0, |set, &alias| set | shard(alias));
            shown
        })
        .collect();
    (shown, need)
}

/// [`open`] for one range, without the other views of its pages, its locks
/// held.
fn open_here(from: usize, to: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let blocks = BLOCKS.with(from, to, PROTECTED);
    count(5, blocks.len() as u64);
    // No longer clean, and its count up, before any of it can be written
    // (see [`Blocks`]).
    for &block in &blocks {
        BLOCKS.clear(block, PROTECTED | CLEAN);
        change(block);
    }
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: blocks we made read-only.
        unsafe { VirtualProtect((from * BLOCK) as *const _, (to - from) * BLOCK, PAGE_READWRITE, &mut old) != 0 }
    };
    let mut i = 0;
    while i < blocks.len() {
        // Blocks in a row in one call, unless they are of several views.
        let mut k = i + 1;
        while k < blocks.len() && blocks[k] == blocks[k - 1] + 1 {
            k += 1;
        }
        if !set(blocks[i], blocks[k - 1] + 1) && k > i + 1 {
            for &block in &blocks[i..k] {
                set(block, block + 1);
            }
        }
        i = k;
    }
    !blocks.is_empty()
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // The length is the title's (`fread`'s size times count, saturated).
    open(at, at.saturating_add(len));
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// We wrote title memory `[at, at + len)` through a view of our own
/// ([`crate::behind`]), and the bytes are there: the guards stay, and every
/// guarded block over the pages counts as written ([`guard`] knows a block
/// that is read-only and not clean).
pub(crate) fn wrote_behind(at: usize, len: usize) {
    note_written(at, at.saturating_add(len));
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// The guarded blocks of `[start, end)`, and those of the same pages at
/// their other views, are no longer as their copies took them.
fn note_written(start: usize, end: usize) {
    if start >= end {
        return;
    }
    let stale = |block: usize| {
        if BLOCKS.clear(block, CLEAN) != 0 {
            change(block);
        }
    };
    each_region(start / BLOCK, end.div_ceil(BLOCK), |from, to| {
        let mut shards = shard(from);
        loop {
            let _held = hold(shards);
            let aliased = BLOCKS.with(from, to, ALIASED);
            let (shown, need) = views_of(&aliased);
            if need & !shards != 0 {
                shards |= need;
                continue;
            }
            for block in BLOCKS.with(from, to, CLEAN) {
                stale(block);
            }
            for alias in shown.into_iter().flatten() {
                stale(alias);
            }
            return;
        }
    });
}

/// What `VICTORY_PAGE_WRITES` asks to be done with a write the guards do
/// not see once its bytes are there ([`title_memory_written`]), as bits:
/// 1, the guarded blocks under it are opened and count as written (host
/// copies of buffers and textures read through tables take them again);
/// 2, the placed textures picked by index that lie under it are due again;
/// 4, both also for reads to plain addresses when they have finished.
/// Unset or `all`: everything. `guards`: bit 1 alone (the default until
/// the slots were counted: 6,165 of 20,109 texture slots in 25 minutes, and
/// 4,451 of 14,841 in three, were uploaded before the title's read had
/// filled them and kept what was there before, until a look frames later
/// happened on it). `0`: nothing, counted only (the default until run 199).
/// `1`: bits 1 and 2. `slots`: bit 2 alone.
/// (Bit 1 by default: in every run 10,000 to 50,000 guarded blocks of host
/// copies lay under such writes and kept the bytes from before; runs 197
/// and 198 opened them, 9,766 and 10,796 blocks, with nothing worse for
/// it. Bit 2 makes texture slots due and is the one that flooded the
/// queue in an earlier form: still asked for by name.)
pub(crate) fn page_writes() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_PAGE_WRITES").as_deref().map(str::trim) {
        Ok("0") => 0,
        Ok("1") => 3,
        Ok("slots") => 2,
        Ok("guards") => 1,
        _ => 7,
    })
}

/// Since the start: writes into physical pages through the runtime's own
/// views that landed at addresses the title has mapped (one for each run
/// of addresses); their bytes; guarded blocks that lay under them (host
/// copies that would otherwise keep the bytes from before); reads to
/// plain addresses that finished.
pub(crate) static WRITTEN: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The bytes of a write the guards do not see are there: title memory
/// `[at, at + len)`. `pages`: DirectStorage wrote them into physical pages
/// through a view of the runtime's own, and this is an address where the
/// title has those pages mapped (no title address faulted, nothing was
/// mapped or unmapped; the shortcut this file's header names). Otherwise a
/// read to a plain address has finished, of which [`writing`] was told
/// before it began.
///
/// Upstream's c75fdba does the same for its trackers (its `Writing` marks
/// the pages once more when dropped; page destinations are reported at
/// each address showing them). Here it is counted always and acted on as
/// [`page_writes`] says. Called without the runtime's page-table lock;
/// takes the guards' lock, then, alone, the lock of the ranges kept for
/// the texture pass.
pub fn title_memory_written(at: usize, len: usize, pages: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    let mode = page_writes();
    let end = at.saturating_add(len);
    if pages {
        WRITTEN[0].fetch_add(1, Relaxed);
        WRITTEN[1].fetch_add(len as u64, Relaxed);
    } else {
        WRITTEN[3].fetch_add(1, Relaxed);
        if mode & 4 == 0 {
            return;
        }
    }
    // The bytes are there for every thread before the guards are looked at:
    // an asking that makes a block read-only after this look takes its copy
    // after that.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
    if pages {
        WRITTEN[2].fetch_add(BLOCKS.with(first, last, PROTECTED).len() as u64, Relaxed);
    }
    if mode & 1 != 0 && BLOCKS.any(first, last, PROTECTED) {
        if crate::behind::on() {
            note_written(at, end);
        } else {
            open(at, end);
        }
    }
    // A placed texture there changed with no descriptor written.
    crate::descriptor::host::page_written(at, len);
}

/// The runtime mapped a view of the physical bytes `[offset, offset + len)`
/// of `section` (`crate::inplace::title_memory_view`): the view is
/// writable, so the same bytes guarded at another view must be checked
/// again — and are, the pages noted written, which also has the next check
/// guard the new view (`protect`).
pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
    if !view_guard() {
        return;
    }
    let ranges = crate::inplace::views_of_section(section, offset, len);
    if ranges.is_empty() {
        return;
    }
    for &(at, n) in &ranges {
        each_region(at / BLOCK, at.saturating_add(n).div_ceil(BLOCK), |from, to| {
            let _held = hold(shard(from));
            if open_here(from, to) {
                #[cfg(feature = "diagnostics")]
                ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
static MAPPING: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Whether reads in place and changes of mapping hold only the addresses
/// they touch, so that they wait only for one another where those meet
/// (`VICTORY_MAP_RANGES=0`: all of title memory, one lock as before).
///
/// Why: in the opening cutscene the runtime remaps title memory hundreds of
/// times a frame, and every remap, wherever it was, held up every texture
/// read and every texture read every remap: in run c21 texture refreshes on
/// all threads waited up to 346 ms a frame "for title memory to hold still"
/// (of 498 ms refreshing), and remaps 10-30 ms a frame for readers.
fn map_ranges() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MAP_RANGES").is_none_or(|v| v != "0"))
}

/// With [`map_ranges`]: the ranges `[from, to)` read in place now (`false`)
/// or being remapped now (`true`), and those of remaps waiting for reads to
/// end. A read waits for both kinds where they meet its range, so that a
/// stream of reads cannot keep a remap waiting for ever.
struct Held {
    now: Vec<(usize, usize, bool)>,
    asking: Vec<(usize, usize)>,
}

static HELD: Mutex<Held> = Mutex::new(Held { now: Vec::new(), asking: Vec::new() });
static HELD_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn meets(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn held() -> std::sync::MutexGuard<'static, Held> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

impl Held {
    fn read_must_wait(&self, range: (usize, usize)) -> bool {
        self.now.iter().any(|&(f, t, alone)| alone && meets(range, (f, t))) || self.asking.iter().any(|&r| meets(range, r))
    }

    /// Takes out one entry `(from, to, alone)` of those held now.
    fn let_go(&mut self, entry: (usize, usize, bool)) {
        if let Some(i) = self.now.iter().position(|&e| e == entry) {
            self.now.swap_remove(i);
        }
    }
}

/// The range `[at, at + len)`, empty for none, all of memory for `usize::MAX`.
fn range_of(at: usize, len: usize) -> (usize, usize) {
    (at, at.saturating_add(len))
}

/// What a lock of title memory's mapping holds: all of it, or a range.
enum MapHeld {
    Read(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>),
    Write(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>),
    Range(usize, usize, bool),
}

impl Drop for MapHeld {
    fn drop(&mut self) {
        if let MapHeld::Range(from, to, alone) = *self {
            held().let_go((from, to, alone));
            HELD_MOVED.notify_all();
        }
    }
}

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping(#[allow(dead_code)] MapHeld, std::time::Instant);

impl Drop for Remapping {
    fn drop(&mut self) {
        // From asking for the lock until it is let go (the frame log).
        crate::framelog::add(crate::framelog::T::RemapUs, self.1.elapsed().as_micros() as u64);
    }
}

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again.
pub fn title_memory_remapping() -> Remapping {
    title_memory_remapping_at(0, usize::MAX)
}

/// [`title_memory_remapping`] for a change of what `[at, at + len)` shows
/// and nothing else: reads elsewhere go on meanwhile. The range must hold
/// every address whose mapping goes, even for a moment (the runtime unmaps
/// whole views to change a page of one).
pub fn title_memory_remapping_at(at: usize, len: usize) -> Remapping {
    let asked = std::time::Instant::now();
    let held = if map_ranges() {
        let range = range_of(at, len);
        let mut h = held();
        h.asking.push(range);
        while h.now.iter().any(|&(f, t, _)| meets(range, (f, t))) {
            h = HELD_MOVED.wait(h).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(i) = h.asking.iter().position(|&r| r == range) {
            h.asking.swap_remove(i);
        }
        h.now.push((range.0, range.1, true));
        MapHeld::Range(range.0, range.1, true)
    } else {
        MapHeld::Write(MAPPING.write().unwrap_or_else(|e| e.into_inner()))
    };
    crate::framelog::add(crate::framelog::T::Remaps, 1);
    crate::framelog::add(crate::framelog::T::RemapWaitUs, asked.elapsed().as_micros() as u64);
    Remapping(held, asked)
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] MapHeld);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory.
pub fn title_memory_reading() -> Reading {
    title_memory_reading_at(0, usize::MAX)
}

/// [`title_memory_reading`] for `[at, at + len)` only: what is mapped there
/// stays mapped; elsewhere the runtime may remap meanwhile.
pub fn title_memory_reading_at(at: usize, len: usize) -> Reading {
    if !map_ranges() {
        return Reading(MapHeld::Read(MAPPING.read().unwrap_or_else(|e| e.into_inner())));
    }
    let range = range_of(at, len);
    let mut h = held();
    while h.read_must_wait(range) {
        h = HELD_MOVED.wait(h).unwrap_or_else(|e| e.into_inner());
    }
    h.now.push((range.0, range.1, false));
    Reading(MapHeld::Range(range.0, range.1, false))
}

/// [`title_memory_reading_at`] if it can be had at once: `None` while the
/// runtime changes a mapping there (or waits to), for a caller that has
/// another way to read and would rather not wait.
pub fn title_memory_reading_now_at(at: usize, len: usize) -> Option<Reading> {
    if !map_ranges() {
        return match MAPPING.try_read() {
            Ok(guard) => Some(Reading(MapHeld::Read(guard))),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(Reading(MapHeld::Read(e.into_inner()))),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
    }
    let range = range_of(at, len);
    let mut h = held();
    if h.read_must_wait(range) {
        return None;
    }
    h.now.push((range.0, range.1, false));
    Some(Reading(MapHeld::Range(range.0, range.1, false)))
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    // With `view_known`: the runtime calls this for two things only
    // (`xmem`: `map` and `unmap`): a range it has just mapped a section's
    // pages into, and a view it is about to make a placeholder again. Either
    // way what stands there is view memory or a hole in it, which is what
    // `is_view` would answer for a range that begins there; and the write
    // watch's asking can only fail there. The title remaps some 83 times a
    // frame, and each time the blocks were forgotten, so that the next
    // asking about each range went to the kernel twice (run 235: 266 to 565
    // failed askings a frame with the blocks remembered from their guards
    // alone, no fewer than without).
    let known = view_known();
    let forget = if known { PROTECTED | CLEAN | ABSENT | ALIASED } else { u8::MAX };
    each_region(at / BLOCK, at.saturating_add(len).div_ceil(BLOCK), |from, to| {
        let _held = hold(shard(from));
        for block in BLOCKS.with(from, to, forget) {
            if BLOCKS.clear(block, forget) & PROTECTED != 0 {
                change(block);
            }
        }
        if known {
            BLOCKS.set(from, to, VIEWED);
        }
    });
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let block = at / BLOCK;
        let mut shards = shard(block);
        loop {
            let _held = hold(shards);
            match open_held(block, block + 1, true, shards) {
                Err(need) => shards |= need,
                Ok(true) => {
                    count(4, 1);
                    fault_again(block);
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::GuardFaults, 1);
                    crate::framelog::add(crate::framelog::T::GuardFaults, 1);
                    return EXCEPTION_CONTINUE_EXECUTION;
                }
                Ok(false) if writable(at) => return EXCEPTION_CONTINUE_EXECUTION,
                Ok(false) => return EXCEPTION_CONTINUE_SEARCH,
            }
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// The bits of blocks against a plain set of them, over a table's end.
    #[test]
    fn block_bits_match_per_block_tracking() {
        // A table nothing else here has blocks in (64 TiB up).
        let origin = (1usize << 30) + 5 * TABLE_BLOCKS - 24;
        let mut blocks = std::collections::BTreeSet::new();
        let check = |blocks: &std::collections::BTreeSet<usize>| {
            for from in 0..56 {
                for to in from..56 {
                    let (a, b) = (origin + from, origin + to);
                    let with: Vec<usize> = (a..b).filter(|block| blocks.contains(block)).collect();
                    let without: Vec<usize> = (a..b).filter(|block| !blocks.contains(block)).collect();
                    assert_eq!(BLOCKS.with(a, b, CLEAN), with, "{from}..{to}");
                    let gaps = BLOCKS.without(a, b, CLEAN);
                    assert!(gaps.windows(2).all(|w| w[0].1 < w[1].0), "runs apart, in order");
                    assert_eq!(gaps.into_iter().flat_map(|(a, b)| a..b).collect::<Vec<_>>(), without, "{from}..{to}");
                    assert_eq!(BLOCKS.all(a, b, CLEAN), without.is_empty(), "{from}..{to}");
                    assert_eq!(BLOCKS.any(a, b, CLEAN), !with.is_empty(), "{from}..{to}");
                }
            }
        };
        check(&blocks);
        for (from, to) in [(5, 12), (1, 3), (3, 7), (23, 33), (17, 20), (9, 26), (2, 3), (40, 56)] {
            BLOCKS.set(origin + from, origin + to, CLEAN);
            blocks.extend(origin + from..origin + to);
            check(&blocks);
        }
        for block in [0, 1, 11, 14, 15, 32, 13, 12, 47, 48] {
            let had = blocks.remove(&(origin + block));
            assert_eq!(BLOCKS.clear(origin + block, CLEAN | ABSENT), if had { CLEAN } else { 0 });
            check(&blocks);
        }
        // Other bits of the blocks are their own.
        BLOCKS.set(origin, origin + 9, ABSENT);
        check(&blocks);
        assert_eq!(BLOCKS.get(origin + 8) & ABSENT, ABSENT);
        assert_eq!(BLOCKS.with(origin, origin + 56, ABSENT).len(), 9);
        // Past the address space: nothing, and no harm.
        BLOCKS.set(MOST_BLOCKS - 1, MOST_BLOCKS + 5, VIEWED);
        assert_eq!(BLOCKS.get(MOST_BLOCKS + 1), 0);
        assert!(!BLOCKS.all(MOST_BLOCKS, MOST_BLOCKS + 1, VIEWED));
        // A huge range: only where there is something, and all of that.
        let mut found = Vec::new();
        each_region(origin, usize::MAX, |from, to| found.extend(BLOCKS.with(from, to, CLEAN)));
        assert!(blocks.iter().all(|b| found.contains(b)));
        for block in origin..origin + 56 {
            BLOCKS.clear(block, u8::MAX);
        }
        BLOCKS.clear(MOST_BLOCKS - 1, u8::MAX);
    }

    /// Threads write guarded memory, at its address and through another view
    /// of it, while others ask for what was written and take it: no write is
    /// lost, whichever way it came and whoever was asking.
    #[test]
    fn no_write_is_lost_between_askers() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        // Over three regions, so that views' blocks lie under other locks.
        const BLOCKS_N: usize = 24;
        const SIZE: usize = BLOCKS_N * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let done = AtomicBool::new(false);
        let next = AtomicU32::new(1);
        std::thread::scope(|s| {
            let writers: Vec<_> = (0..4u64)
                .map(|n| {
                    let next = &next;
                    s.spawn(move || {
                        let mut seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(n + 1);
                        for _ in 0..20_000 {
                            seed = seed.wrapping_mul(0xd134_2543_de82_ef95).wrapping_add(1);
                            let view = if seed >> 63 == 0 { one } else { two };
                            // A word of its own for each writer.
                            let at = (seed >> 20) as usize % (SIZE / 64) * 64 + n as usize * 4;
                            // SAFETY: inside the view; only this thread writes the word.
                            unsafe { ((view + at) as *mut u32).write_volatile(next.fetch_add(1, Ordering::Relaxed)) };
                        }
                    })
                })
                .collect();
            // Askers of the first view, each with a copy of its own.
            let askers: Vec<_> = (0..3usize)
                .map(|n| {
                    let done = &done;
                    s.spawn(move || {
                        let mut copy = vec![0u8; SIZE];
                        let mut took = vec![0u64; BLOCKS_N];
                        let mut counts = vec![None; BLOCKS_N];
                        let mut take = |from: usize, to: usize| {
                            // As a copy asks: nobody, while the counts stand.
                            let count = changes_all(one + from * BLOCK, one + to * BLOCK);
                            if (from..to).all(|b| counts[b] == Some((from, to, count))) {
                                return;
                            }
                            let (now, pages, held) = writes_held(one + from * BLOCK, one + to * BLOCK, true).expect("a view");
                            for (page, at) in pages {
                                let block = page - one / PAGE;
                                if at > took[block] {
                                    // SAFETY: a block of the view, and of the copy. Torn words are
                                    // written again, and reported again.
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            (one + block * BLOCK) as *const u8,
                                            copy.as_mut_ptr().add(block * BLOCK),
                                            BLOCK,
                                        )
                                    };
                                    took[block] = now;
                                }
                            }
                            counts[from..to].fill(held.then_some((from, to, count)));
                        };
                        take(0, BLOCKS_N);
                        let mut k = n;
                        while !done.load(Ordering::Acquire) {
                            let from = k * 7 % BLOCKS_N;
                            take(from, (from + 1 + k % 5).min(BLOCKS_N));
                            k += 1;
                        }
                        take(0, BLOCKS_N);
                        copy
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            done.store(true, Ordering::Release);
            // SAFETY: the view, which nothing writes any more.
            let now = unsafe { std::slice::from_raw_parts(one as *const u8, SIZE) };
            for asker in askers {
                let copy = asker.join().unwrap();
                let differing: Vec<usize> = (0..BLOCKS_N).filter(|b| copy[b * BLOCK..(b + 1) * BLOCK] != now[b * BLOCK..(b + 1) * BLOCK]).collect();
                assert!(differing.is_empty(), "blocks with a write nobody was told of: {differing:?}");
            }
        });
        drop_views(section, &[one, two], SIZE);
    }

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
        title_memory_remapped(view, 2 * BLOCK);
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()), "nothing written since");
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
        title_memory_remapped(p, 3 * BLOCK);
    }

    /// A pagefile section mapped twice: two views of the same physical
    /// pages, the second left for the test; the section handle, the two
    /// addresses.
    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        // SAFETY: a fresh pagefile section, mapped twice; the views keep
        // the section alive and the handle is closed by the caller.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    /// The section of [`two_views`] is closed and its views unmapped; the
    /// guard is told they are gone first.
    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, bytes);
            // SAFETY: a view of the section mapped by `two_views`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        // SAFETY: the section's last reference.
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    /// The title wrote the pages of a guarded texture through another view
    /// of them: the fault at that view is the guard's to see, and the
    /// guarded view's next check reports the pages written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        // The first check takes it in (and every view of its pages).
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        // Writing through the other view faults into the handler, which
        // opens that view and the guarded one: the next check sees it.
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    thread_local! {
        /// [`hot_frames`] for the asking thread's checks, in place of
        /// `VICTORY_GUARD_HOT` (other tests run beside on threads of their
        /// own).
        pub(super) static HOT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    }

    /// With [`hot_frames`]: a block that took a guard fault is left open
    /// and counts as written at every check while it is hot (frames do not
    /// pass here), so a write to it without a fault is not missed; a block
    /// that took none stays guarded. Once it is not hot, the next check
    /// guards it again.
    #[test]
    fn a_block_that_faulted_lately_stays_open_and_counts_as_written() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        HOT.with(|h| h.set(Some(4)));
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        // SAFETY: the guarded view, read-write once the handler has opened it.
        unsafe { ((guarded + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert!(writable(guarded + BLOCK), "left open");
        assert!(!writable(guarded), "the block that took no fault is guarded");
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]), "written at every check");
        // SAFETY: as above; open, so no fault.
        unsafe { ((guarded + BLOCK + 9) as *mut u8).write(2) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        HOT.with(|h| h.set(None));
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]), "guarded again");
        assert!(!writable(guarded + BLOCK));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A view of guarded pages is mapped anew: it is writable, so the
    /// guarded view checks its pages again (and guards the new view).
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        title_memory_remapped(other, SIZE);
        crate::inplace::title_memory_view(other, SIZE, section, 0);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A write of the runtime's own, told at one view of the pages
    /// (`writing`): the other views' next checks report the pages written.
    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()), "nothing written yet");
        writing(other + BLOCK, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        // Half a page and the next: both blocks, not just the first's.
        writing(other + BLOCK / 2, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(0), pages(1)]));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// Both views of the pages are checked, each for a copy of its own
    /// (the title shows one physical page at several addresses, and each
    /// address has its host copy): a write through either is reported at
    /// both, whichever is checked first. Guarding the pages for the first
    /// view's check must not note the second view clean, or its copy would
    /// keep the bytes from before the write.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0), pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0), pages(two, 1)]), "the second view's first check");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()), "nothing written yet");
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()), "nothing written yet");
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 1)]), "written through this view, checked second");
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 0)]), "written through this view, checked second");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        drop_views(section, &[one, two], SIZE);
    }

    /// A page shown in more than `MANY_VIEWS` views is guarded at each by
    /// itself: the others stay writable, and nothing looks through them all.
    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(writable(two), "another view of it is left alone");
        // SAFETY: the guarded view, opened by the handler.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        title_memory_remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{
        title_memory_reading, title_memory_reading_at, title_memory_reading_now_at, title_memory_remapping,
        title_memory_remapping_at,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    /// One test of the lock at a time: a whole-memory hold of one would
    /// stop the other's reads elsewhere.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// A change of mapping waits for a read in place that is under way, and
    /// a read in place waits for a change of mapping.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!changed.load(Ordering::SeqCst), "the mapping changed under a read in place");
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!read.load(Ordering::SeqCst), "a read in place began during a change of mapping");
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }

    /// Held by range, reads and changes of mapping wait for one another only
    /// where their ranges meet. (Addresses only: nothing is read.)
    #[test]
    fn ranges_wait_only_where_they_meet() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let a = 0x7100_0000_0000usize;
        let changing = title_memory_remapping_at(a, 0x10000);
        // A read just after it, and one just before, are had at once.
        let after = std::thread::spawn(move || title_memory_reading_now_at(a + 0x10000, 0x1000).is_some());
        assert!(after.join().unwrap(), "a read beside a remap waited for it");
        let before = std::thread::spawn(move || title_memory_reading_now_at(a - 0x1000, 0x1000).is_some());
        assert!(before.join().unwrap(), "a read beside a remap waited for it");
        // Ones that meet it are not, nor is a read of all of memory.
        let meets = std::thread::spawn(move || title_memory_reading_now_at(a + 0xF000, 0x2000).is_some());
        assert!(!meets.join().unwrap(), "a read began inside a remap");
        let all = std::thread::spawn(|| title_memory_reading_now_at(0, usize::MAX).is_some());
        assert!(!all.join().unwrap(), "a read of all memory began during a remap");
        // A read that meets it waits until it is done.
        let (told, heard) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _reading = title_memory_reading_at(a + 0x8000, 0x10);
            told.send(()).unwrap();
        });
        assert!(heard.recv_timeout(Duration::from_millis(100)).is_err(), "a read began inside a remap");
        drop(changing);
        heard.recv_timeout(Duration::from_secs(10)).expect("the read went on after the remap");
        reader.join().unwrap();
        // A remap waits for a read that meets it, not for one elsewhere.
        let reading = title_memory_reading_at(a, 0x1000);
        let (told, heard) = mpsc::channel();
        let elsewhere = std::thread::spawn(move || {
            let _changing = title_memory_remapping_at(a + 0x1000, 0x1000);
            told.send(()).unwrap();
        });
        heard.recv_timeout(Duration::from_secs(10)).expect("a remap waited for a read elsewhere");
        elsewhere.join().unwrap();
        let (told, heard) = mpsc::channel();
        let over = std::thread::spawn(move || {
            let _changing = title_memory_remapping_at(a + 0x800, 0x1000);
            told.send(()).unwrap();
        });
        assert!(heard.recv_timeout(Duration::from_millis(100)).is_err(), "a remap went on under a read");
        // And while it waits, a new read where it is waits behind it (no
        // stream of reads keeps it out), but one elsewhere does not.
        let behind = std::thread::spawn(move || title_memory_reading_now_at(a + 0x900, 0x10).is_some());
        assert!(!behind.join().unwrap(), "a read went ahead of a waiting remap");
        let beside = std::thread::spawn(move || title_memory_reading_now_at(a + 0x4000, 0x10).is_some());
        assert!(beside.join().unwrap(), "a read elsewhere waited for a waiting remap");
        drop(reading);
        heard.recv_timeout(Duration::from_secs(10)).expect("the remap went on after the read");
        over.join().unwrap();
    }
}
