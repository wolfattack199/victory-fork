//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//!
//! Library calls overlap: the host documents the library as safe to use
//! from several threads ("The pipeline library is thread-safe to use, and
//! will internally synchronize as necessary", `CreatePipelineLibrary`,
//! Thread Safety), with one exception kept here: two loads of the same
//! pipeline are made one after the other ([`same_name`]). Under one lock of
//! ours for every call, the eight workers made pipelines one at a time:
//! 87 to 96 of 110 looks at each worker found it waiting for that lock
//! (run 49), and an earlier run's 2,300 pipelines, all in the library, were
//! still being loaded 46 s into the opening cutscene.
//! `VICTORY_LIBRARY_LOCK=1`: one call at a time, as before.
//!
//! The library's file is mapped, not read into memory ([`Blob`]): the host
//! keeps pointing into the bytes it was made from for as long as it lives,
//! and read into a buffer they were 1,352 MB of the process's own memory
//! for the whole run (run 60), all of it in physical memory. Mapped, the
//! pages are the file's: only those the host reads are in memory, and the
//! system can drop them without writing them anywhere. A save writes the
//! new library through a mapping of the new file in the same way (a buffer
//! of the library's size was made, filled and written out at each save).
//! `VICTORY_LIBRARY_MAP=0`: read and written through buffers, as before.
//!
//! The mapped file's handle stays open for as long as the mapping lives
//! ([`holds`]). A save renames the new library over the file this run has
//! mapped, and with the handle closed Windows then gives up the old file's
//! contents: every page of the mapping that is not in memory at that
//! moment, or leaves memory later (the trim below, or Windows taking pages
//! back), reads as zeros from then on. The host's library reads such pages
//! as stored pipelines; the driver, handed a pipeline of zeros, removes the
//! device with DXGI_ERROR_DRIVER_INTERNAL_ERROR (runs 72, 73 and 82, each
//! within 5 ms of the first library call after a save and a trim; and the
//! library run 77 saved from such pages was refused by run 78). Measured
//! with a 64 MB file written past the system's cache, mapped and replaced:
//! 16,352 of 16,384 pages read zeros with the handle closed, none with it
//! open (the test below). `VICTORY_LIBRARY_HANDLE=0`: closed, as before.
//!
//! The library is kept below a size in generations ([`start`]). The host
//! stores a shader once however many pipelines use it (measured on an RTX
//! 5060 Ti: twelve pipelines of the same shaders are 65 KB in a library and
//! 763 KB one by one), so a library is the small way to keep them, but the
//! host has no call that takes a pipeline out of one: what a changed
//! translator leaves unused stayed for good. So a library that is too big,
//! or big and mostly unused, becomes the old one, and an empty one takes
//! its place. A pipeline the new one does not have is loaded from the old
//! one and stored in the new; what no run asks for goes with the old one,
//! three saved runs later.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// The most bytes one generation of the library takes; 0: no limit.
static CAP: AtomicU64 = AtomicU64::new(0);

/// Keeps the pipeline library and the early list in `dir` from now on; the
/// two generations of the library take `max_bytes` together at most.
pub fn use_pipeline_cache(dir: PathBuf, max_bytes: u64) {
    CAP.store(max_bytes / 2, Ordering::Relaxed);
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);
/// Of the loaded ones, those that came from the library's old generation
/// and were stored in the new one.
static CARRIED: AtomicU64 = AtomicU64::new(0);
/// For the memory lines: the bytes of the library's file, kept in memory
/// while the library lives, and the bytes the last save wrote (a buffer of
/// that size is made, filled and written every time pipelines were added,
/// at most every 20 s).
pub(crate) static BLOB: AtomicU64 = AtomicU64::new(0);
pub(crate) static SAVED: AtomicU64 = AtomicU64::new(0);

/// Whether the library's file is mapped ([`Blob`]) and saved through a
/// mapping. On by default; `VICTORY_LIBRARY_MAP=0` uses buffers, as before.
fn mapped() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_MAP").is_none_or(|v| v != "0"))
}

/// Whether the mapped library file's handle is kept open while it is
/// mapped (see the module's notes: without it a save in the same run turns
/// the mapping's pages into zeros as they leave memory). On by default;
/// `VICTORY_LIBRARY_HANDLE=0` closes it after mapping, as before.
fn holds() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_HANDLE").is_none_or(|v| v != "0"))
}

/// A whole file mapped into memory: read-only (the library's file) or
/// writable (the file a save fills). Unmapped when dropped.
struct Mapped {
    at: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    /// The file, for a mapping made by [`Mapped::open`] ([`holds`]): open
    /// until after the view is unmapped, so that the file a rename took
    /// the name from keeps its contents for the view.
    _file: Option<std::fs::File>,
}

impl Mapped {
    /// Maps `file` (`len` bytes, not zero) for reading, or for reading and
    /// writing.
    fn map(file: &std::fs::File, len: usize, write: bool) -> Option<Mapped> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, PAGE_READONLY, PAGE_READWRITE,
        };
        if len == 0 {
            return None;
        }
        // SAFETY: a file we hold open; the view keeps the file's pages
        // after the mapping object and the file are closed.
        let at = unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                if write { PAGE_READWRITE } else { PAGE_READONLY },
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return None;
            }
            let at = MapViewOfFile(mapping, if write { FILE_MAP_WRITE } else { FILE_MAP_READ }, 0, 0, 0);
            CloseHandle(mapping);
            at
        };
        (!at.Value.is_null()).then_some(Mapped { at, len, _file: None })
    }

    /// The file at `path`, mapped for reading; `None` if it is not there,
    /// is empty or cannot be mapped.
    fn open(path: &std::path::Path) -> Option<Mapped> {
        Mapped::open_holding(path, holds())
    }

    /// As [`Mapped::open`]; `hold` says whether the file stays open with
    /// the mapping.
    fn open_holding(path: &std::path::Path, hold: bool) -> Option<Mapped> {
        // Opened as the standard library opens a file to read: others may
        // read, write, rename and delete it meanwhile, so a save still
        // renames the new library over this one.
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        let mut mapped = Mapped::map(&file, len, false)?;
        if hold {
            mapped._file = Some(file);
        }
        Some(mapped)
    }

    /// A new file of `len` bytes at `path` (replacing what is there),
    /// mapped for writing.
    fn create(path: &std::path::Path, len: usize) -> Option<Mapped> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .ok()?;
        file.set_len(len as u64).ok()?;
        Mapped::map(&file, len, true)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the view holds `len` readable bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.at.Value.cast(), self.len) }
    }

    /// Only for a view made by [`Mapped::create`].
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: a writable view of `len` bytes, ours alone.
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: our view.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.at) };
    }
}

/// The bytes a library is made from: the host reads them for as long as
/// the library lives, so they are kept beside it.
enum Blob {
    /// No file (an empty library).
    None,
    /// The file read into memory (`VICTORY_LIBRARY_MAP=0`, tests, or a file
    /// that could not be mapped).
    Held(Vec<u8>),
    /// The file itself, mapped.
    Mapped(Mapped),
}

impl Blob {
    /// The library file in `dir`: a library a save left beside it
    /// ([`NEXT_FILE`]) takes its place first.
    fn of(dir: &std::path::Path) -> Blob {
        let path = dir.join(LIBRARY_FILE);
        let next = dir.join(NEXT_FILE);
        if next.exists() && std::fs::rename(&next, &path).is_err() {
            tracing::warn!("the host pipeline library saved last could not take the old one's place");
        }
        Blob::file(&path)
    }

    /// The file at `path`, mapped if it can be.
    fn file(path: &std::path::Path) -> Blob {
        if mapped()
            && let Some(file) = Mapped::open(path)
        {
            return Blob::Mapped(file);
        }
        match std::fs::read(path) {
            Ok(bytes) if !bytes.is_empty() => Blob::Held(bytes),
            _ => Blob::None,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::None => &[],
            Blob::Held(bytes) => bytes,
            Blob::Mapped(file) => file.bytes(),
        }
    }
}

/// The mapped library file's view (address, bytes; 0: not mapped), when a
/// pipeline was last loaded from the library (`crate::host::now_ms`), and
/// whether the view's pages were let go since ([`trim`]).
static VIEW: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
/// As [`VIEW`], of the library's old generation.
static OLD_VIEW: [std::sync::atomic::AtomicUsize; 2] = [const { std::sync::atomic::AtomicUsize::new(0) }; 2];
static LOADED_AT: AtomicU64 = AtomicU64::new(0);
static TRIMMED: AtomicBool = AtomicBool::new(false);
/// The bytes of the view let go so far, for the memory line.
pub(crate) static TRIMMED_BYTES: AtomicU64 = AtomicU64::new(0);
/// When the view's pages were last let go (`crate::host::now_ms`), and how
/// often and when a save of this run last renamed a new library over the
/// mapped file: for [`removal_note`].
static TRIMMED_AT: AtomicU64 = AtomicU64::new(0);
static REPLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The library calls running now: thread, what (load, make, store,
/// serialise), the pipeline, and since when (`crate::host::now_us`).
static CALLS: Mutex<Vec<(u32, &'static str, String, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// The pipeline this thread is making (its shaders), for [`CALLS`].
    static LABEL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the pipeline this thread makes next.
pub(crate) fn label(text: String) {
    LABEL.with(|l| *l.borrow_mut() = text);
}

/// A library call of this thread, listed in [`CALLS`] until dropped.
struct Call(u32, u64);

fn call(what: &'static str, name: &str) -> Call {
    let (thread, since) = (crate::host::thread_id(), crate::host::now_us());
    let label = LABEL.with(|l| l.borrow().clone());
    CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((thread, what, format!("{name} {label}"), since));
    Call(thread, since)
}

impl Drop for Call {
    fn drop(&mut self) {
        CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| (c.0, c.3) != (self.0, self.1));
    }
}

/// For the report when the host device is removed: the library calls
/// running at that moment, whether this run replaced the mapped file and
/// let its pages go, and how many pages of the mapping read as zeros now.
/// A library holds no page of zeros (the 259 MB one of run 83 has no run of
/// 2,048 zero bytes): each such page is one whose contents were lost (see
/// the module's notes), and a pipeline stored there is what the driver was
/// handed.
pub(crate) fn removal_note() -> String {
    use std::fmt::Write;
    let now = crate::host::now_us();
    let mut text = String::from("pipeline library: ");
    {
        let calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if calls.is_empty() {
            text.push_str("no call running");
        }
        for (i, (thread, what, name, since)) in calls.iter().enumerate() {
            let _ = write!(
                text,
                "{}{what} of {name} running for {:.1} ms on thread {thread}",
                if i == 0 { "" } else { ", " },
                now.saturating_sub(*since) as f64 / 1000.0
            );
        }
    }
    let (at, len) = (VIEW[0].load(Ordering::Relaxed), VIEW[1].load(Ordering::Relaxed));
    if at == 0 {
        text.push_str("; its file is not mapped");
        return text;
    }
    let ago = |ms: u64| crate::host::now_ms().saturating_sub(ms) as f64 / 1000.0;
    let _ = write!(
        text,
        "; its file is mapped ({len} bytes), the handle {}",
        if holds() { "kept open" } else { "closed (VICTORY_LIBRARY_HANDLE=0)" }
    );
    match OLD_VIEW[1].load(Ordering::Relaxed) {
        0 => {}
        old => {
            let _ = write!(text, "; so is its old generation's ({old} bytes)");
        }
    }
    match REPLACED[0].load(Ordering::Relaxed) {
        0 => text.push_str("; this run has not replaced the file"),
        n => {
            let _ = write!(
                text,
                "; this run replaced the file under the mapping {n} times, last {:.1} s ago",
                ago(REPLACED[1].load(Ordering::Relaxed))
            );
        }
    }
    if TRIMMED_BYTES.load(Ordering::Relaxed) > 0 {
        let _ = write!(text, "; its pages were last let go {:.1} s ago", ago(TRIMMED_AT.load(Ordering::Relaxed)));
    }
    // Within two seconds: pages not in memory come from the disk.
    let started = std::time::Instant::now();
    // SAFETY: the view lives as long as the library, which is never dropped.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
    let (mut zero, mut looked, mut first) = (0u64, 0u64, None);
    for (n, page) in bytes.chunks(4096).enumerate() {
        if n % 4096 == 0 && started.elapsed() > std::time::Duration::from_secs(2) {
            break;
        }
        looked += 1;
        if page.iter().all(|&b| b == 0) {
            zero += 1;
            if first.is_none() {
                first = Some(n);
            }
        }
    }
    let _ = write!(text, "; {zero} of the {looked} pages looked at read as zeros now");
    if let Some(first) = first {
        let _ = write!(text, " (the first is page {first}): the library's contents were lost there");
    }
    text
}

/// Lets the mapped library file's pages go from the process's memory once
/// no pipeline has been loaded for ten seconds. The host reads nearly the
/// whole file while the early pipelines load (run 65: all 1,490 MB of it
/// were in the process's memory a minute in, and 1,300 MB still at the
/// end, on a PC with 2 GB free); after that it reads the file again only
/// when a new pipeline loads. Pages let go stay in Windows' file cache
/// while nothing else wants the memory, and come back on the next read.
/// Called once a second by the sweep; does its work on a thread of its own
/// (hundreds of thousands of pages), once after each burst of loads.
/// `VICTORY_LIBRARY_TRIM=0`: the pages stay until Windows takes them.
pub(crate) fn trim() {
    static ON: OnceLock<bool> = OnceLock::new();
    let views: Vec<(usize, usize)> = [&VIEW, &OLD_VIEW]
        .iter()
        .map(|v| (v[0].load(Ordering::Relaxed), v[1].load(Ordering::Relaxed)))
        .filter(|&(at, _)| at != 0)
        .collect();
    if views.is_empty()
        || TRIMMED.load(Ordering::Relaxed)
        || !*ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_TRIM").is_none_or(|v| v != "0"))
    {
        return;
    }
    let quiet_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    if crate::host::now_ms() < LOADED_AT.load(Ordering::Relaxed) + quiet_ms
        || TRIMMED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    let work = move || {
        let mut len = 0;
        for (at, bytes) in views {
            // SAFETY: the view lives as long as the library, which is never
            // dropped. Unlocking pages that are not locked fails, as
            // expected, and takes them out of the working set: the
            // documented way to let pages of a mapped file go.
            unsafe { windows_sys::Win32::System::Memory::VirtualUnlock(at as *const _, bytes) };
            len += bytes;
        }
        TRIMMED_BYTES.fetch_add(len as u64, Ordering::Relaxed);
        TRIMMED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
        crate::host::trail(|| format!("pipeline library: the mapped file's pages let go from memory ({len} bytes)"));
    };
    if std::thread::Builder::new().name("victory-library-trim".into()).spawn(work).is_err() {
        TRIMMED.store(false, Ordering::Relaxed);
    }
}

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Blob,
    /// The generation before, if one is kept, and its blob: only loaded
    /// from.
    old: Option<(ID3D12PipelineLibrary1, Blob)>,
    /// The most bytes the library takes (0: no limit), and whether it takes
    /// no more pipelines because it does.
    cap: u64,
    full: AtomicBool,
    /// Pipelines stored since the last save.
    added: AtomicU64,
    /// Stores share it and a save takes it alone: the size the library
    /// asks for must still be its size when it is written out.
    saving: RwLock<()>,
}

// SAFETY: D3D12 objects are free-threaded, and the library synchronises
// its own calls (see the module's notes); what it does not, two loads of
// one name, is serialised by `same_name`.
unsafe impl Send for Library {}
// SAFETY: as above; the other fields are an atomic and a lock.
unsafe impl Sync for Library {}

/// Held by a load of the pipeline named `name`: loads of the same name
/// wait for each other (the host's one exception to the library's thread
/// safety: such a load may change that pipeline's entry). Names share 64
/// locks, so two different names seldom wait.
fn same_name(name: &str) -> MutexGuard<'static, ()> {
    static LOADING: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let at = name.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b)));
    LOADING[at % LOADING.len()].lock().unwrap_or_else(|e| e.into_inner())
}

/// With `VICTORY_LIBRARY_LOCK=1`: held by every library call, as before.
fn one_call() -> Option<MutexGuard<'static, ()>> {
    static ON: OnceLock<bool> = OnceLock::new();
    static CALLING: Mutex<()> = Mutex::new(());
    ON.get_or_init(|| std::env::var_os("VICTORY_LIBRARY_LOCK").is_some_and(|v| v == "1"))
        .then(|| CALLING.lock().unwrap_or_else(|e| e.into_inner()))
}

const LIBRARY_FILE: &str = "host-pipelines.bin";
/// A saved library that could not replace [`LIBRARY_FILE`] while this run
/// had it mapped (a system that refuses to replace a mapped file): the next
/// start renames it over the library first ([`Blob::of`]).
const NEXT_FILE: &str = "host-pipelines.next";
/// The library's generation before this one (see the module's notes).
const OLD_FILE: &str = "host-pipelines.old";
/// The pipelines of the library and when each was last used ([`Names`]).
const NAMES_FILE: &str = "host-pipelines.used";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

/// The pipelines of the library's file, by name, each with the number of
/// the run that used it last. Runs count when they save.
#[derive(Default)]
struct Names {
    /// This run's number.
    run: u32,
    /// The run whose library the old generation is; `None` without one.
    rotated: Option<u32>,
    used: HashMap<String, u32>,
    /// Whether there is something to write since the last save.
    changed: bool,
}

/// A pipeline not used in this many runs is one nothing asks for any more.
const LIVE_RUNS: u32 = 4;
/// The old generation goes after this many runs.
const OLD_RUNS: u32 = 3;

impl Names {
    /// What `text` ([`Names::text`]) says; a line that says nothing is
    /// passed over.
    fn parse(text: &str) -> Names {
        let mut names = Names::default();
        for line in text.lines() {
            let Some((word, number)) = line.split_once(' ') else { continue };
            let Ok(number) = number.trim().parse::<u32>() else { continue };
            match word {
                "run" => names.run = number,
                "rotated" => names.rotated = Some(number),
                name => {
                    names.used.insert(name.to_owned(), number);
                }
            }
        }
        // The run that reads it is the next one.
        names.run = names.run.saturating_add(1);
        names
    }

    fn text(&self) -> String {
        use std::fmt::Write;
        let mut text = format!("run {}\n", self.run);
        if let Some(rotated) = self.rotated {
            let _ = writeln!(text, "rotated {rotated}");
        }
        for (name, run) in &self.used {
            let _ = writeln!(text, "{name} {run}");
        }
        text
    }

    /// The pipelines used lately, and all of them.
    fn live(&self) -> (usize, usize) {
        let live = self.used.values().filter(|&&run| self.run.saturating_sub(run) <= LIVE_RUNS);
        (live.count(), self.used.len())
    }

    fn note(&mut self, name: &str) {
        match self.used.get_mut(name) {
            Some(run) if *run == self.run => {}
            Some(run) => {
                *run = self.run;
                self.changed = true;
            }
            None => {
                self.used.insert(name.to_owned(), self.run);
                self.changed = true;
            }
        }
    }
}

static NAMES: Mutex<Option<Names>> = Mutex::new(None);

/// This run used the pipeline `name` of the library.
fn note_name(name: &str) {
    if let Some(names) = NAMES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        names.note(name);
    }
}

/// What a start does with the generations it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Keep,
    /// The library becomes the old one; this run starts an empty one.
    Rotate,
    /// The library is too big even to be the old one.
    Discard,
    /// The old one goes.
    DropOld,
}

/// `current` and `old`: the files' bytes. `cap`: the most one takes (0: no
/// limit). `live`: the library's pipelines used lately, of how many.
/// `old_runs`: the runs since the library last became the old one.
fn plan(current: u64, old: u64, cap: u64, live: (usize, usize), old_runs: Option<u32>) -> Plan {
    if cap == 0 {
        return Plan::Keep;
    }
    if current > 2 * cap {
        return Plan::Discard;
    }
    // Nearly full: it takes no more. Over half of it, and most of it unused
    // (or not known: a library made before this was counted).
    let full = current >= cap / 10 * 9;
    let mostly_unused = current > cap / 2 && live.0 * 10 <= live.1 * 6;
    if full || mostly_unused {
        return Plan::Rotate;
    }
    let worn = old_runs.is_none_or(|runs| runs >= OLD_RUNS);
    if old > 0 && (worn || current + old > 2 * cap) {
        return Plan::DropOld;
    }
    Plan::Keep
}

/// Readies the generations in `dir` for a run and opens their files: the
/// library's and the old one's, and the names of the pipelines in the first.
fn start(dir: &std::path::Path, cap: u64) -> (Blob, Blob, Names) {
    let (path, old_path) = (dir.join(LIBRARY_FILE), dir.join(OLD_FILE));
    // A save that was cut off.
    let _ = std::fs::remove_file(path.with_extension("tmp"));
    let next = dir.join(NEXT_FILE);
    if next.exists() && std::fs::rename(&next, &path).is_err() {
        tracing::warn!("the host pipeline library saved last could not take the old one's place");
    }
    let names_path = dir.join(NAMES_FILE);
    let mut names = Names::parse(&std::fs::read_to_string(&names_path).unwrap_or_default());
    let len = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
    let (current, old) = (len(&path), len(&old_path));
    let old_runs = names.rotated.map(|run| names.run.saturating_sub(run));
    let plan = plan(current, old, cap, names.live(), old_runs);
    let emptied = match plan {
        Plan::Keep => false,
        Plan::DropOld => {
            let _ = std::fs::remove_file(&old_path);
            names.rotated = None;
            names.changed = true;
            false
        }
        Plan::Rotate => std::fs::rename(&path, &old_path).is_ok(),
        Plan::Discard => {
            let _ = std::fs::remove_file(&old_path);
            std::fs::remove_file(&path).is_ok()
        }
    };
    if emptied {
        names.rotated = (plan == Plan::Rotate).then_some(names.run);
        names.used.clear();
        // Written now: a run that ends before its first save must not find
        // the old library's names for the empty one.
        let mut before = Names {
            run: names.run.saturating_sub(1),
            rotated: names.rotated,
            ..Default::default()
        };
        before.changed = false;
        let _ = write_atomically(&names_path, before.text().as_bytes());
    }
    if plan != Plan::Keep {
        tracing::info!(
            ?plan,
            current_mb = current >> 20,
            old_mb = old >> 20,
            cap_mb = cap >> 20,
            "host pipeline library: generations"
        );
    }
    (Blob::of(dir), Blob::file(&old_path), names)
}

fn library() -> Option<&'static Library> {
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let cap = CAP.load(Ordering::Relaxed);
            let (blob, old, names) = match DIR.get() {
                Some(dir) => {
                    let (blob, old, names) = start(dir, cap);
                    (blob, old, Some(names))
                }
                None => (Blob::None, Blob::None, None),
            };
            let had = !matches!(blob, Blob::None);
            let mut library = open(&device, blob);
            match &mut library {
                Some(library) => {
                    library.cap = cap;
                    library.old = open_old(&device, old);
                    // The host took none of it (another driver): the names
                    // are those of pipelines that are gone.
                    let refused = had && matches!(library._blob, Blob::None);
                    *NAMES.lock().unwrap_or_else(|e| e.into_inner()) = names.map(|mut names| {
                        if refused {
                            names.used.clear();
                            names.changed = true;
                        }
                        names
                    });
                }
                None => tracing::warn!("no host pipeline library: pipelines are not kept"),
            }
            library
        })
        .as_ref()
}

/// The old generation in `blob`; `None` if there is none or the host
/// rejects it (its file then goes at the next start but two).
fn open_old(device: &ID3D12Device1, blob: Blob) -> Option<(ID3D12PipelineLibrary1, Blob)> {
    if blob.bytes().is_empty() {
        return None;
    }
    // SAFETY: the bytes outlive the library (kept beside it).
    let made = unsafe { device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(blob.bytes()) };
    match made.and_then(|l| l.cast::<ID3D12PipelineLibrary1>()) {
        Ok(library) => {
            if let Blob::Mapped(file) = &blob {
                OLD_VIEW[1].store(file.len, Ordering::Relaxed);
                OLD_VIEW[0].store(file.at.Value as usize, Ordering::Relaxed);
            }
            tracing::info!(bytes = blob.bytes().len(), "host pipeline library: old generation loaded");
            Some((library, blob))
        }
        Err(e) => {
            tracing::info!(error = %e, "host pipeline library: old generation rejected");
            None
        }
    }
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: Blob) -> Option<Library> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    if !blob.bytes().is_empty() {
        match make(blob.bytes()) {
            Ok(library) => {
                let bytes = blob.bytes().len();
                BLOB.store(bytes as u64, Ordering::Relaxed);
                tracing::info!(bytes, mapped = matches!(blob, Blob::Mapped(_)), "host pipeline library loaded");
                if let Blob::Mapped(file) = &blob {
                    VIEW[1].store(file.len, Ordering::Relaxed);
                    VIEW[0].store(file.at.Value as usize, Ordering::Relaxed);
                    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
                }
                return Some(Library {
                    library,
                    _blob: blob,
                    old: None,
                    cap: 0,
                    full: AtomicBool::new(false),
                    added: AtomicU64::new(0),
                    saving: RwLock::new(()),
                });
            }
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    // The rejected bytes are let go (a mapped file is unmapped) before the
    // empty library is made: nothing reads them any more.
    drop(blob);
    let library = make(&[]).ok()?;
    Some(Library {
        library,
        _blob: Blob::None,
        old: None,
        cap: 0,
        full: AtomicBool::new(false),
        added: AtomicU64::new(0),
        saving: RwLock::new(()),
    })
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    match library() {
        Some(library) => pipeline_in(library, name, load, create),
        None => {
            MADE.fetch_add(1, Ordering::Relaxed);
            create()
        }
    }
}

/// [`pipeline`], with `library`.
fn pipeline_in(
    library: &Library,
    name: &str,
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    let began = std::time::Instant::now();
    let load_from = |from: &ID3D12PipelineLibrary1| {
        let _all = one_call();
        let _one = same_name(name);
        let _call = call("load", name);
        load(from, PCWSTR(wide.as_ptr()))
    };
    let loaded = load_from(&library.library);
    // The load read the library's file (see `trim`).
    LOADED_AT.store(crate::host::now_ms(), Ordering::Relaxed);
    TRIMMED.store(false, Ordering::Relaxed);
    let refused = match loaded {
        Ok(pso) => {
            LOADED.fetch_add(1, Ordering::Relaxed);
            note_name(name);
            crate::host::trail(|| {
                let label = LABEL.with(|l| l.borrow().clone());
                format!("pipeline library: loaded {label} in {:.1} ms", began.elapsed().as_secs_f64() * 1e3)
            });
            return Ok(pso);
        }
        Err(e) => e,
    };
    if let Some((old, _)) = &library.old
        && let Ok(pso) = load_from(old)
    {
        LOADED.fetch_add(1, Ordering::Relaxed);
        CARRIED.fetch_add(1, Ordering::Relaxed);
        keep(library, name, &wide, &pso);
        return Ok(pso);
    }
    // Compiled outside any lock: compiles take up to seconds.
    let pso = {
        let _call = call("make", name);
        create()?
    };
    MADE.fetch_add(1, Ordering::Relaxed);
    crate::host::trail(|| {
        let label = LABEL.with(|l| l.borrow().clone());
        format!(
            "pipeline library: made {label} in {:.1} ms (the library did not load it: {refused})",
            began.elapsed().as_secs_f64() * 1e3
        )
    });
    keep(library, name, &wide, &pso);
    Ok(pso)
}

/// Stores `pso` in the library, unless that is as big as it may be.
fn keep(library: &Library, name: &str, wide: &HSTRING, pso: &ID3D12PipelineState) {
    /// The size is asked for after every so many pipelines.
    const SIZE_EVERY: u64 = 16;
    if library.full.load(Ordering::Relaxed) {
        return;
    }
    let _all = one_call();
    let _stores = library.saving.read().unwrap_or_else(|e| e.into_inner());
    let _call = call("store", name);
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { library.library.StorePipeline(PCWSTR(wide.as_ptr()), pso) }.is_err() {
        return;
    }
    let added = library.added.fetch_add(1, Ordering::Relaxed) + 1;
    note_name(name);
    if library.cap == 0 || !added.is_multiple_of(SIZE_EVERY) {
        return;
    }
    // SAFETY: a plain query.
    let size = unsafe { library.library.GetSerializedSize() } as u64;
    if size >= library.cap && !library.full.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            size_mb = size >> 20,
            cap_mb = library.cap >> 20,
            "the host pipeline library is as big as it may be: pipelines made from here on are kept from the next run on \
             (VICTORY_CACHE_MAX_MB, or max_mb under [cache] in victory.toml, allows more)"
        );
    }
}

/// How long after a save the next one may be: every save writes the whole
/// library, which at 20 s for one of 1.4 GB was 70 MB/s to the disk for as
/// long as pipelines were being made.
fn save_every(bytes: u64) -> std::time::Duration {
    std::time::Duration::from_secs((20 + (bytes >> 23)).min(180))
}

/// Writes the library and the early list if pipelines were added since the
/// last save and [`save_every`] has passed (or `now`).
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let every = save_every(SAVED.load(Ordering::Relaxed).max(BLOB.load(Ordering::Relaxed)));
        if !now && last.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(error = %e, "host pipeline library not saved");
        return;
    }
    let tmp = dir.join(LIBRARY_FILE).with_extension("tmp");
    // The bytes of the library written to `tmp`; 0 when no pipeline was
    // added: the library on disk is then what it was, and only the early
    // list is written (a title pipeline first drawn with wrote all 1,352 MB
    // of the library again: run 60's second save).
    let bytes = {
        let _all = one_call();
        // No store between asking the size and writing; loads go on.
        let _alone = library.saving.write().unwrap_or_else(|e| e.into_inner());
        let added = library.added.load(Ordering::Relaxed);
        let names_changed = NAMES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .is_some_and(|names| std::mem::take(&mut names.changed));
        if !USED_CHANGED.swap(false, Ordering::Relaxed) && added == 0 && !names_changed {
            return;
        }
        if added == 0 {
            0
        } else {
            // SAFETY: a plain query.
            let size = unsafe { library.library.GetSerializedSize() };
            let _call = call("serialise", "the whole library");
            if let Err(e) = serialise(&library.library, &tmp, size) {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            library.added.store(0, Ordering::Relaxed);
            SAVED.store(size as u64, Ordering::Relaxed);
            size
        }
    };
    let used: Vec<String> = USED.lock().unwrap_or_else(|e| e.into_inner()).iter().flatten().cloned().collect();
    let library_written = if bytes > 0 { replace_library(dir, &tmp) } else { Ok(()) };
    if bytes > 0 && library_written.is_ok() {
        // In place unless the system refused and the new one waits beside it.
        let in_place = !dir.join(NEXT_FILE).exists();
        if in_place && VIEW[0].load(Ordering::Relaxed) != 0 {
            REPLACED[1].store(crate::host::now_ms(), Ordering::Relaxed);
            REPLACED[0].fetch_add(1, Ordering::Relaxed);
        }
        crate::host::trail(|| {
            format!(
                "pipeline library: saved {bytes} bytes, {}",
                if in_place { "renamed over the library's file" } else { "left beside the library's file for the next start" }
            )
        });
    }
    let names = NAMES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(Names::text);
    let written = library_written
        .and_then(|()| write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()))
        .and_then(|()| match names {
            Some(text) => write_atomically(&dir.join(NAMES_FILE), text.as_bytes()),
            None => Ok(()),
        });
    match written {
        Ok(()) => tracing::info!(
            bytes,
            early = used.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            carried = CARRIED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipeline library saved"
        ),
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

/// Writes the library's `size` serialised bytes to the file `tmp`: through
/// a mapping of that file, so that the bytes are the file's pages and not
/// 1.3 GB more of the process's own memory for the seconds a save takes;
/// through a buffer where the file cannot be mapped (and with
/// `VICTORY_LIBRARY_MAP=0`).
fn serialise(library: &ID3D12PipelineLibrary1, tmp: &std::path::Path, size: usize) -> std::io::Result<()> {
    let failed = |e: windows::core::Error| std::io::Error::other(e.to_string());
    if mapped()
        && let Some(mut file) = Mapped::create(tmp, size)
    {
        // SAFETY: a buffer of the size the library asks for.
        return unsafe { library.Serialize(file.bytes_mut()) }.map_err(failed);
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: a buffer of the size the library asks for.
    unsafe { library.Serialize(&mut bytes) }.map_err(failed)?;
    std::fs::write(tmp, &bytes)
}

/// Renames the written library `tmp` over the library's file in `dir`. This
/// run may have that file mapped: Windows 11 replaces a mapped file, and
/// the mapping goes on reading the old one for as long as a handle on it
/// is open ([`holds`]; with none, only the pages in memory at that moment
/// stay: the rest read as zeros). Where a system refuses, the new library
/// is left beside it as [`NEXT_FILE`] for the next start.
fn replace_library(dir: &std::path::Path, tmp: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(tmp, dir.join(LIBRARY_FILE)).or_else(|_| std::fs::rename(tmp, dir.join(NEXT_FILE)))
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part. Serialised so concurrent worker and
/// shutdown saves do not race on the same staging file or target rename.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::AtomicU64;
    static WRITING: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{n}", std::process::id()));
    let tmp = path.with_file_name(name);
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Title pipeline keys requested by draws or dispatches (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was requested in this or an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// A draw or dispatch requested the title pipeline with `key`.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, Blob::Held(b"not a pipeline library".repeat(16))).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(matches!(library._blob, Blob::None));
    }

    /// A mapped file reads as the file's bytes, a file written through a
    /// mapping holds what was written, and the library's file can be
    /// replaced while it is mapped (or the new one waits as the next).
    #[test]
    fn the_library_file_is_mapped_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        assert!(matches!(Blob::of(dir.path()), Blob::None), "no file yet");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(Blob::of(dir.path()), Blob::None), "an empty file");
        let old: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&path, &old).unwrap();
        let blob = Blob::of(dir.path());
        assert!(matches!(blob, Blob::Mapped(_)) || !mapped());
        assert_eq!(blob.bytes(), &old[..]);
        // A save while the old file is mapped.
        let tmp = path.with_extension("tmp");
        let new: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        {
            let mut file = Mapped::create(&tmp, new.len()).expect("a new file, mapped");
            file.bytes_mut().copy_from_slice(&new);
        }
        assert_eq!(std::fs::read(&tmp).unwrap(), new);
        replace_library(dir.path(), &tmp).unwrap();
        assert!(!tmp.exists());
        // The mapping still reads the old library.
        assert_eq!(blob.bytes(), &old[..]);
        drop(blob);
        // The next start reads the new one, wherever the save left it.
        assert_eq!(Blob::of(dir.path()).bytes(), &new[..]);
        assert!(!dir.path().join(NEXT_FILE).exists());
    }

    const MB: u64 = 1 << 20;

    #[test]
    fn a_start_keeps_the_library_below_its_size() {
        let cap = 700 * MB;
        let live = (100, 100);
        // No limit: nothing is ever let go.
        assert_eq!(plan(9000 * MB, 0, 0, (0, 100), None), Plan::Keep);
        assert_eq!(plan(0, 0, cap, (0, 0), None), Plan::Keep);
        assert_eq!(plan(300 * MB, 0, cap, live, None), Plan::Keep);
        // Small: kept whatever is in it. Big and in use: kept.
        assert_eq!(plan(300 * MB, 0, cap, (1, 100), None), Plan::Keep);
        assert_eq!(plan(500 * MB, 0, cap, live, None), Plan::Keep);
        // Big and mostly unused, or not known, or nearly full.
        assert_eq!(plan(500 * MB, 0, cap, (60, 100), None), Plan::Rotate);
        assert_eq!(plan(500 * MB, 0, cap, (0, 0), None), Plan::Rotate);
        assert_eq!(plan(640 * MB, 0, cap, live, None), Plan::Rotate);
        // Of a run without the limit: too big to keep beside a new one.
        assert_eq!(plan(1300 * MB, 0, cap, live, None), Plan::Rotate);
        assert_eq!(plan(1500 * MB, 0, cap, live, None), Plan::Discard);
        // The old one: for a few runs, or while there is room.
        assert_eq!(plan(100 * MB, 600 * MB, cap, live, Some(1)), Plan::Keep);
        assert_eq!(plan(100 * MB, 600 * MB, cap, live, Some(OLD_RUNS)), Plan::DropOld);
        assert_eq!(plan(100 * MB, 600 * MB, cap, live, None), Plan::DropOld);
        assert_eq!(plan(200 * MB, 1300 * MB, cap, live, Some(1)), Plan::DropOld);
    }

    #[test]
    fn names_remember_the_run_that_used_them_last() {
        let mut names = Names::parse("");
        assert_eq!((names.run, names.rotated, names.live()), (1, None, (0, 0)));
        names.note("a");
        names.note("b");
        assert!(std::mem::take(&mut names.changed));
        names.note("a"); // again in the same run: nothing new to write
        assert!(!names.changed);
        // Runs that use `b` only.
        for run in 2..=1 + LIVE_RUNS {
            names = Names::parse(&names.text());
            assert_eq!((names.run, names.live()), (run, (2, 2)));
            names.note("b");
            assert!(names.changed);
        }
        names = Names::parse(&names.text());
        assert_eq!(names.live(), (1, 2), "a was not used for too long");
        names.rotated = Some(7);
        assert_eq!(Names::parse(&names.text()).rotated, Some(7));
        // Damage is passed over.
        let odd = Names::parse("run x\n\nc\nd 4 5\ne 3\n\u{0}\u{0}");
        assert_eq!((odd.run, odd.used.len()), (1, 1));
    }

    #[test]
    fn saves_of_a_big_library_are_further_apart() {
        let secs = |mb| save_every(mb * MB).as_secs();
        assert_eq!((secs(0), secs(80), secs(800), secs(90_000)), (20, 30, 120, 180));
    }

    /// The generations on disk over several starts, without a host.
    #[test]
    fn generations_take_turns_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let (path, old) = (dir.join(LIBRARY_FILE), dir.join(OLD_FILE));
        let cap = 1000;
        let save = |names: &Names| write_atomically(&dir.join(NAMES_FILE), names.text().as_bytes()).unwrap();

        // A library of a build that did not count names, and a cut-off save.
        std::fs::write(&path, vec![1u8; 600]).unwrap();
        std::fs::write(path.with_extension("tmp"), b"part").unwrap();
        let (now, before, mut names) = start(dir, cap);
        assert!(now.bytes().is_empty());
        assert_eq!(before.bytes(), vec![1u8; 600]);
        assert!(!path.with_extension("tmp").exists());
        assert_eq!(names.rotated, Some(names.run));
        drop((now, before));

        // That run ends without a save: the next start finds both as they were.
        let (now, before, again) = start(dir, cap);
        assert!(now.bytes().is_empty());
        assert_eq!(before.bytes().len(), 600);
        assert_eq!(again.rotated, Some(names.run));
        drop((now, before));

        // Runs that save: the old one stays for the next few, then goes.
        std::fs::write(&path, vec![2u8; 300]).unwrap();
        for _ in 0..OLD_RUNS - 1 {
            names.note("p");
            save(&names);
            let (now, before, next) = start(dir, cap);
            assert_eq!((now.bytes().len(), before.bytes().len()), (300, 600));
            names = next;
            drop((now, before));
        }
        names.note("p");
        save(&names);
        let (now, before, names) = start(dir, cap);
        assert_eq!(now.bytes().len(), 300);
        assert!(before.bytes().is_empty() && !old.exists());
        assert_eq!((names.rotated, names.live()), (None, (1, 1)));
        drop((now, before));

        // Larger than both may be together: gone.
        std::fs::write(&path, vec![3u8; 2001]).unwrap();
        let (now, before, names) = start(dir, cap);
        assert!(now.bytes().is_empty() && before.bytes().is_empty());
        assert_eq!((names.rotated, names.used.len()), (None, 0));
        assert!(!path.exists() && !old.exists());
    }

    /// On the host: a pipeline the library does not have comes from the old
    /// generation, uncompiled, and is in the library from then on; a full
    /// library takes no more.
    #[test]
    fn a_pipeline_of_the_old_generation_is_carried_over() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let Some(dxc) = victory_shader::emit::validator::Validator::default_path()
            .map(|p| p.with_file_name("dxc.exe"))
            .filter(|p| p.exists())
        else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let (hlsl, out) = (dir.path().join("c.hlsl"), dir.path().join("c.dxil"));
        let code = "[RootSignature(\"UAV(u0)\")] [numthreads(1, 1, 1)] void main() { u[0] = 7; }";
        std::fs::write(&hlsl, format!("RWStructuredBuffer<uint> u : register(u0); {code}")).unwrap();
        let args = ["-nologo", "-T", "cs_6_0", "-E", "main", "-Fo"];
        let status = std::process::Command::new(dxc).args(args).arg(&out).arg(&hlsl).status().unwrap();
        assert!(status.success());
        let dxil = std::fs::read(&out).unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: dxil.as_ptr().cast(),
                BytecodeLength: dxil.len(),
            },
            ..Default::default()
        };
        let compiled = AtomicU64::new(0);
        let get = |library: &Library, name: &str| {
            pipeline_in(
                library,
                name,
                // SAFETY: the description points at live data.
                |l, name| unsafe { l.LoadComputePipeline(name, &desc) },
                || {
                    compiled.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: as above.
                    unsafe { device.CreateComputePipelineState(&desc) }
                },
            )
            .expect("a pipeline")
        };
        let bytes = |library: &Library| {
            // SAFETY: plain queries, and a buffer of the size asked for.
            unsafe {
                let mut bytes = vec![0u8; library.library.GetSerializedSize()];
                library.library.Serialize(&mut bytes).unwrap();
                bytes
            }
        };
        let first = open(&device, Blob::None).expect("an empty library");
        get(&first, "one");
        assert_eq!(compiled.load(Ordering::Relaxed), 1);
        let saved = bytes(&first);

        let mut second = open(&device, Blob::None).expect("an empty library");
        second.old = open_old(&device, Blob::Held(saved));
        assert!(second.old.is_some(), "the host takes its own library back");
        get(&second, "one");
        assert_eq!(compiled.load(Ordering::Relaxed), 1, "from the old generation");
        assert_eq!(second.added.load(Ordering::Relaxed), 1, "stored in the new one");
        get(&second, "two");
        assert_eq!(compiled.load(Ordering::Relaxed), 2, "in neither");

        // What was carried is in what the new one saves, the old one gone.
        let third = open(&device, Blob::Held(bytes(&second))).expect("the saved library");
        get(&third, "one");
        get(&third, "two");
        assert_eq!(compiled.load(Ordering::Relaxed), 2);

        // Full: made, used and not stored.
        third.full.store(true, Ordering::Relaxed);
        get(&third, "three");
        get(&third, "three");
        assert_eq!(compiled.load(Ordering::Relaxed), 4);
        assert_eq!(third.added.load(Ordering::Relaxed), 0);
    }

    /// The bytes at `at` of the pattern the test below writes.
    fn pattern(at: usize, out: &mut [u8]) {
        for (i, word) in out.chunks_exact_mut(4).enumerate() {
            let v = ((at / 4 + i) as u32).wrapping_mul(2_654_435_761).wrapping_add(1);
            word.copy_from_slice(&v.to_le_bytes());
        }
    }

    /// A file of `len` bytes (whole MiB) of [`pattern`], written past the
    /// system's cache: none of its pages is in memory afterwards. False
    /// where the folder's file system does not take such writes.
    fn write_uncached(path: &std::path::Path, len: usize) -> bool {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH
        const UNCACHED: u32 = 0x2000_0000 | 0x8000_0000;
        const CHUNK: usize = 1 << 20;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(UNCACHED)
            .open(path)
        else {
            return false;
        };
        // Such writes want a buffer on a sector boundary.
        let mut store = vec![0u8; CHUNK + 4096];
        let start = store.as_ptr().align_offset(4096);
        for at in (0..len).step_by(CHUNK) {
            let chunk = &mut store[start..start + CHUNK];
            pattern(at, chunk);
            if file.write_all(chunk).is_err() {
                return false;
            }
        }
        true
    }

    /// The device loss of runs 72, 73 and 82: the library's file is
    /// replaced while it is mapped, and pages of the mapping that were not
    /// in memory then are read afterwards (in the runs: pages the trim let
    /// go and Windows gave to something else). With the file's handle kept
    /// open they still read as the old file. (With it closed, as before,
    /// all but the first few read as zeros on the PC this was found on.)
    #[test]
    fn a_replaced_library_file_still_reads_from_the_disk() {
        const LEN: usize = 8 << 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LIBRARY_FILE);
        if !write_uncached(&path, LEN) {
            return; // no uncached writes here: nothing to show
        }
        let mapped = Mapped::open_holding(&path, true).expect("the file, mapped");
        // A save: the new library, renamed over the mapped one.
        let tmp = path.with_extension("tmp");
        {
            let mut file = Mapped::create(&tmp, LEN).expect("a new file, mapped");
            file.bytes_mut().fill(0xEE);
        }
        replace_library(dir.path(), &tmp).unwrap();
        // Every page of the old mapping, read for the first time now.
        let mut expected = vec![0u8; 4096];
        for (page, bytes) in mapped.bytes().chunks(4096).enumerate() {
            pattern(page * 4096, &mut expected);
            assert!(bytes == &expected[..], "page {page} of the replaced library no longer reads as the old file");
        }
    }

    #[test]
    fn concurrent_write_atomically_never_fails_or_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-pipelines.bin");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let target = target.clone();
                std::thread::spawn(move || {
                    let payload = vec![b'a' + t as u8; 4096];
                    for _ in 0..30 {
                        write_atomically(&target, &payload).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let final_bytes = std::fs::read(&target).unwrap();
        assert_eq!(final_bytes.len(), 4096);
        assert!(final_bytes.iter().all(|&b| b == final_bytes[0]));
    }
}
