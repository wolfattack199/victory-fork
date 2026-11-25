//! Rust's memory, on a heap of its own.
//!
//! Rust's allocator is the process heap, and so is that of everything else
//! in the process: the Direct3D runtime, the driver, and the driver's shader
//! compiler, whose background threads allocate and free blocks of every size
//! for minutes after the world's first frame (half of their time is spent in
//! the heap). Blocks over 16 KiB, and every block the heap must commit
//! memory for, go through one lock. In the opening cutscene the title's
//! audio thread stood at that lock for 17% of its time (46 KiB for each
//! Opus packet), the thread that copies back for 5%; with the compiler's
//! threads at a low priority (`crate::calm`) the one that held it was the
//! one that did not run.
//!
//! So Rust's blocks come from a heap of ours ("segment heap", which locks
//! per size and per processor), and big ones, which a heap takes from the
//! kernel and gives back each time (a fault and 4 KiB of zeroing for every
//! page: tens of thousands a second), are kept by size and used again. The
//! driver's and the runtime's own blocks are a matter of the process heap's
//! kind (the manifest of `apps/victory` asks for a segment heap).
//!
//! `VICTORY_HEAP=0`: the process heap, as before. Diagnostics builds count
//! blocks in an allocator of their own (`crate::copystat`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use windows_sys::Win32::System::Environment::GetEnvironmentVariableW;
use windows_sys::Win32::System::Memory::{
    GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapCreate, HeapDestroy, HeapFree, HeapReAlloc, MEM_COMMIT,
    MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

/// Blocks of this size and more are kept by size when they are given back.
const BIG: usize = 32 << 10;
/// The biggest ones kept; a bigger block is the kernel's each time.
const BIGGEST: usize = 64 << 20;
/// The most bytes kept.
const KEEP: usize = 384 << 20;
/// The alignment every block of a heap has.
const HEAP_ALIGN: usize = 16;
/// The alignment every block of the kernel's has.
const KERNEL_ALIGN: usize = 64 << 10;
/// "Segment heap" (`winnt.h`; not in windows-sys).
const HEAP_CREATE_SEGMENT_HEAP: u32 = 0x100;

/// The size class of `size`, from [`BIG`] to [`BIGGEST`], and the bytes of
/// its blocks: four to a doubling.
fn class(size: usize) -> (usize, usize) {
    let top = (size - 1).ilog2() as usize;
    let step = 1usize << (top - 2);
    let bytes = size.next_multiple_of(step);
    ((top - 14) * 4 + (bytes - (1 << top)) / step - 1, bytes)
}

/// Blocks given back: the first of them (each holds the next one's address)
/// and the bytes of all.
struct Kept {
    first: Mutex<usize>,
    bytes: AtomicUsize,
}

static CLASSES: [Kept; 48] = [const { Kept { first: Mutex::new(0), bytes: AtomicUsize::new(0) } }; 48];
static KEPT: AtomicUsize = AtomicUsize::new(0);

fn pop(index: usize, bytes: usize) -> *mut u8 {
    let kept = &CLASSES[index];
    if kept.bytes.load(Ordering::Relaxed) == 0 {
        return null_mut();
    }
    let mut first = kept.first.lock().unwrap_or_else(|e| e.into_inner());
    let at = *first as *mut u8;
    if at.is_null() {
        return null_mut();
    }
    // SAFETY: a block of this class nobody holds; its first bytes are the
    // next one's address.
    *first = unsafe { at.cast::<usize>().read() };
    kept.bytes.fetch_sub(bytes, Ordering::Relaxed);
    KEPT.fetch_sub(bytes, Ordering::Relaxed);
    at
}

fn push(index: usize, bytes: usize, at: *mut u8) {
    if KEPT.load(Ordering::Relaxed) + bytes > KEEP {
        // The blocks of the class that keeps most make room (those of a
        // time that is over), unless it is this one.
        let most = (0..CLASSES.len()).max_by_key(|&i| CLASSES[i].bytes.load(Ordering::Relaxed)).unwrap_or(index);
        if most == index {
            // SAFETY: the caller gives the block back.
            unsafe { give_back(at) };
            return;
        }
        let one = class_bytes(most);
        let mut room = 0;
        while room < bytes {
            let old = pop(most, one);
            if old.is_null() {
                break;
            }
            room += one;
            // SAFETY: a block nobody holds.
            unsafe { give_back(old) };
        }
    }
    let kept = &CLASSES[index];
    let mut first = kept.first.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the caller gives the block back; it has room for an address.
    unsafe { at.cast::<usize>().write(*first) };
    *first = at as usize;
    kept.bytes.fetch_add(bytes, Ordering::Relaxed);
    KEPT.fetch_add(bytes, Ordering::Relaxed);
}

/// The bytes of a block of class `index` (as [`class`] gives them).
fn class_bytes(index: usize) -> usize {
    let top = index / 4 + 14;
    (1usize << top) + (index % 4 + 1) * (1usize << (top - 2))
}

/// # Safety
/// `at` is a whole allocation of `VirtualAlloc`'s nobody holds.
unsafe fn give_back(at: *mut u8) {
    // SAFETY: as the caller says.
    unsafe { VirtualFree(at.cast(), 0, MEM_RELEASE) };
}

fn take(bytes: usize) -> *mut u8 {
    // SAFETY: new memory, anywhere.
    unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE).cast() }
}

const UNKNOWN: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

/// Whether the heap is ours ([`ON`]); asked of the environment once, without
/// allocating.
fn mode() -> u8 {
    static MODE: AtomicU8 = AtomicU8::new(UNKNOWN);
    match MODE.load(Ordering::Relaxed) {
        UNKNOWN => {
            let name: [u16; 13] = {
                let mut wide = [0; 13];
                let mut i = 0;
                for b in b"VICTORY_HEAP" {
                    wide[i] = u16::from(*b);
                    i += 1;
                }
                wide
            };
            let mut value = [0u16; 8];
            // SAFETY: a terminated name and room for the value.
            let n = unsafe { GetEnvironmentVariableW(name.as_ptr(), value.as_mut_ptr(), value.len() as u32) };
            let mode = if n == 1 && value[0] == u16::from(b'0') { OFF } else { ON };
            // The first answer stands: what it gave out it takes back.
            match MODE.compare_exchange(UNKNOWN, mode, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => mode,
                Err(there) => there,
            }
        }
        mode => mode,
    }
}

fn heap() -> *mut c_void {
    static HEAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
    let heap = HEAP.load(Ordering::Acquire);
    if !heap.is_null() {
        return heap;
    }
    // SAFETY: plain creation; a heap that is not used is destroyed.
    unsafe {
        let mut made = HeapCreate(HEAP_CREATE_SEGMENT_HEAP, 0, 0);
        if made.is_null() {
            made = HeapCreate(0, 0, 0);
        }
        if made.is_null() {
            made = GetProcessHeap();
        }
        match HEAP.compare_exchange(null_mut(), made, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => made,
            Err(there) => {
                if made != GetProcessHeap() {
                    HeapDestroy(made);
                }
                there
            }
        }
    }
}

/// Where a block is: decided by its layout alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// The process heap (with [`mode`] off, or an alignment of its own).
    Process,
    Heap,
    Class(usize, usize),
    Kernel,
}

fn place(size: usize, align: usize) -> Place {
    if mode() != ON {
        Place::Process
    } else if size < BIG {
        if align <= HEAP_ALIGN { Place::Heap } else { Place::Process }
    } else if align > KERNEL_ALIGN {
        Place::Process
    } else if size <= BIGGEST {
        let (index, bytes) = class(size);
        Place::Class(index, bytes)
    } else {
        Place::Kernel
    }
}

/// The allocator.
pub struct Own;

// SAFETY: a block's place follows from its layout alone, which the caller
// gives again; blocks of each place go back to it; every place's blocks are
// aligned as `place` asks.
unsafe impl GlobalAlloc for Own {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match place(layout.size(), layout.align()) {
            // SAFETY: the caller's contract, passed on.
            Place::Process => unsafe { System.alloc(layout) },
            // SAFETY: a live heap.
            Place::Heap => unsafe { HeapAlloc(heap(), 0, layout.size()).cast() },
            Place::Class(index, bytes) => {
                let at = pop(index, bytes);
                if at.is_null() { take(bytes) } else { at }
            }
            Place::Kernel => take(layout.size()),
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        match place(layout.size(), layout.align()) {
            // SAFETY: the caller's contract, passed on.
            Place::Process => unsafe { System.alloc_zeroed(layout) },
            // SAFETY: a live heap.
            Place::Heap => unsafe { HeapAlloc(heap(), HEAP_ZERO_MEMORY, layout.size()).cast() },
            Place::Class(index, bytes) => {
                let at = pop(index, bytes);
                if at.is_null() {
                    return take(bytes);
                }
                // SAFETY: a block of at least that size, ours alone.
                unsafe { at.write_bytes(0, layout.size()) };
                at
            }
            Place::Kernel => take(layout.size()),
        }
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        match place(layout.size(), layout.align()) {
            // SAFETY: the caller's contract, passed on.
            Place::Process => unsafe { System.dealloc(at, layout) },
            // SAFETY: a block of this heap.
            Place::Heap => unsafe {
                HeapFree(heap(), 0, at.cast());
            },
            Place::Class(index, bytes) => push(index, bytes, at),
            // SAFETY: a whole allocation, given back.
            Place::Kernel => unsafe { give_back(at) },
        }
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        match (place(layout.size(), layout.align()), place(size, layout.align())) {
            // SAFETY: the caller's contract, passed on.
            (Place::Process, Place::Process) => unsafe { System.realloc(at, layout, size) },
            // SAFETY: a block of this heap.
            (Place::Heap, Place::Heap) => unsafe { HeapReAlloc(heap(), 0, at.cast(), size).cast() },
            // The block has the room (or is as small as it gets).
            (Place::Class(a, _), Place::Class(b, _)) if a == b => at,
            _ => {
                // SAFETY: the caller's contract: a valid size for the align.
                let layout_new = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
                // SAFETY: the new block is apart from the old one, and both
                // hold the smaller size.
                unsafe {
                    let new = self.alloc(layout_new);
                    if !new.is_null() {
                        std::ptr::copy_nonoverlapping(at, new, layout.size().min(size));
                        self.dealloc(at, layout);
                    }
                    new
                }
            }
        }
    }
}

#[cfg(not(feature = "diagnostics"))]
#[global_allocator]
static HEAP: Own = Own;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_hold_their_sizes_four_to_a_doubling() {
        assert_eq!(class(BIG), (3, BIG));
        assert_eq!(class(BIG + 1), (4, 40 << 10));
        assert_eq!(class(46_080), (5, 48 << 10));
        assert_eq!(class(64 << 10), (7, 64 << 10));
        assert_eq!(class(BIGGEST), (47, BIGGEST));
        let mut size = BIG;
        while size <= BIGGEST {
            let (index, bytes) = class(size);
            assert!(index < CLASSES.len() && bytes >= size && bytes <= size + size / 4 + 1, "{size}");
            assert_eq!(class_bytes(index), bytes);
            assert_eq!(class(bytes), (index, bytes), "the class of a block is the class it was made for");
            size += size / 7 + 1;
        }
    }

    #[test]
    fn blocks_keep_their_bytes_from_place_to_place() {
        let own = Own;
        // Small, kept by size, over the biggest, and back.
        let sizes = [1, 100, BIG - 1, BIG, 46_080, 200 << 10, BIGGEST, BIGGEST + 1, 300 << 10, 64];
        let mut layout = Layout::from_size_align(sizes[0], 8).unwrap();
        // SAFETY: the blocks are used within their layouts and given back.
        unsafe {
            let mut at = own.alloc(layout);
            assert!(!at.is_null());
            at.write(0xa5);
            let mut held = 1;
            for &size in &sizes[1..] {
                at = own.realloc(at, layout, size);
                assert!(!at.is_null());
                layout = Layout::from_size_align(size, 8).unwrap();
                let same = held.min(size).min(4096);
                assert!(std::slice::from_raw_parts(at, same).iter().all(|&b| b == 0xa5), "{size}");
                held = size.min(4096);
                at.write_bytes(0xa5, held);
            }
            own.dealloc(at, layout);
        }
    }

    #[test]
    fn a_zeroed_block_is_zero_also_when_it_was_used_before() {
        let own = Own;
        for size in [64, 46_080, 3 << 20] {
            let layout = Layout::from_size_align(size, 4096.min(if size < BIG { 16 } else { 4096 })).unwrap();
            for _ in 0..3 {
                // SAFETY: the block is used within its layout and given back.
                unsafe {
                    let at = own.alloc_zeroed(layout);
                    assert!(at as usize % layout.align() == 0);
                    assert!(std::slice::from_raw_parts(at, size).iter().all(|&b| b == 0));
                    at.write_bytes(0xff, size);
                    own.dealloc(at, layout);
                }
            }
        }
    }

    #[test]
    fn blocks_go_between_threads() {
        let threads: Vec<_> = (0..6)
            .map(|t| {
                std::thread::spawn(move || {
                    let mut all: Vec<Vec<u8>> = Vec::new();
                    for i in 0..400usize {
                        let size = (i * 7919 + t * 104_729) % (600 << 10) + 1;
                        all.push(vec![t as u8; size]);
                        if i % 3 == 0 {
                            let v = all.swap_remove(i % all.len());
                            assert!(v.iter().all(|&b| b == t as u8));
                        }
                    }
                    all
                })
            })
            .collect();
        for (t, thread) in threads.into_iter().enumerate() {
            for v in thread.join().unwrap() {
                assert!(v.iter().all(|&b| b == t as u8));
            }
        }
        assert!(KEPT.load(Ordering::Relaxed) <= KEEP + BIGGEST);
    }
}
