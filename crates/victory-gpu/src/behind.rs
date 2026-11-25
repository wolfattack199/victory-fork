//! Title memory written behind its guards.
//!
//! A host copy of view memory leaves the blocks it took read-only, so that
//! the title's first write to one faults and is noted (`crate::watch`). Our
//! own writes into title memory (what shaders wrote, copied back after a
//! flight) had to take the guard away first, at every view of the page, and
//! the next copy put it back: in the opening cutscene 4,300 blocks opened a
//! frame for 560 writes of the title's, and 1,400 to 4,000 protected again,
//! a call of the kernel each, with every other thread that asks what was
//! written waiting (340 to 450 ms a frame over all threads).
//!
//! The pages are slices of sections, and a guard is a protection of a view,
//! not of a page: each section is mapped once more, whole, for us alone and
//! writable for good, and what we write goes through that view. The guards
//! stay, the blocks are noted written at every view of them
//! ([`crate::watch::wrote_behind`]), after the bytes are there: a copy that
//! took the block while we wrote takes it again.
//!
//! `VICTORY_WRITE_BEHIND=1`; off by default. Memory that is no view of a
//! section is written as before.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// A view of a whole section, ours alone.
struct Mirror {
    base: usize,
    bytes: u64,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        // SAFETY: the view we mapped; nothing holds it any more.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut _ }) };
    }
}

/// By section handle.
static MIRRORS: RwLock<Option<HashMap<usize, Arc<Mirror>>>> = RwLock::new(None);

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_BEHIND").is_some_and(|v| v == "1"))
}

/// The runtime made a section of `bytes`.
pub(crate) fn section_made(section: usize, bytes: u64) {
    section_gone(section);
    if on() {
        mirror(section, bytes);
    }
}

/// The runtime is about to close `section`: its handle may be another's next.
pub(crate) fn section_gone(section: usize) {
    let gone = MIRRORS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
        .and_then(|mirrors| mirrors.remove(&section));
    drop(gone);
}

fn mirror(section: usize, bytes: u64) {
    use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
    // SAFETY: a section of the runtime's, of this size; a view of it takes
    // addresses, and memory only where its pages are committed and used.
    let at = unsafe { MapViewOfFile(section as _, FILE_MAP_WRITE, 0, 0, bytes as usize) };
    if at.Value.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(bytes, %error, "no view of our own on title memory: written at the title's views");
        return;
    }
    tracing::info!(mb = bytes >> 20, "title memory has a view of our own (VICTORY_WRITE_BEHIND)");
    MIRRORS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .insert(section, Arc::new(Mirror { base: at.Value as usize, bytes }));
}

/// The view of a whole section, held.
pub struct TitleSectionView {
    _mirror: Arc<Mirror>,
}

/// Where bytes `[offset, offset + len)` of `section` are in the view of ours
/// that shows all of it, if there is one, and what keeps that view. Nothing
/// is noted of what is written there.
pub fn title_section_view(section: usize, offset: u64, len: usize) -> Option<(usize, TitleSectionView)> {
    let mirrors = MIRRORS.read().unwrap_or_else(PoisonError::into_inner);
    let mirror = mirrors.as_ref()?.get(&section)?;
    if offset.checked_add(len as u64)? > mirror.bytes {
        return None;
    }
    Some((mirror.base + offset as usize, TitleSectionView { _mirror: mirror.clone() }))
}

/// A write of ours into title memory `[at, at + len)`; its blocks are noted
/// written when it is dropped.
pub(crate) struct Behind {
    at: usize,
    len: usize,
    /// Title address, ours, bytes; in address order.
    pieces: Vec<(usize, usize, usize)>,
    _held: Vec<Arc<Mirror>>,
}

impl Behind {
    /// Copies `len` bytes to `dst`, in the range the write was begun for.
    pub(crate) fn write(&self, dst: usize, src: *const u8, len: usize) {
        for (title, ours, n) in self.within(dst, len) {
            crate::host::write_mapped(ours as *mut u8, src.wrapping_add(title - dst), n);
        }
    }

    /// The pieces of `[dst, dst + len)`: title address, ours, bytes.
    pub(crate) fn within(&self, dst: usize, len: usize) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        let end = dst + len;
        self.pieces.iter().filter_map(move |&(title, ours, n)| {
            let (from, to) = (title.max(dst), (title + n).min(end));
            (from < to).then(|| (from, ours + (from - title), to - from))
        })
    }
}

impl Drop for Behind {
    fn drop(&mut self) {
        crate::watch::wrote_behind(self.at, self.len);
    }
}

/// A write into title memory `[at, at + len)` behind its guards; `None` if
/// part of it is not a view of a section with a view of ours.
pub(crate) fn of(at: usize, len: usize) -> Option<Behind> {
    if len == 0 {
        return None;
    }
    let mirrors = MIRRORS.read().unwrap_or_else(PoisonError::into_inner);
    let mirrors = mirrors.as_ref()?;
    let mut held: Vec<Arc<Mirror>> = Vec::new();
    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    for (title, n, section, offset) in crate::inplace::shown(at, len)? {
        let mirror = mirrors.get(&section)?;
        if offset + n as u64 > mirror.bytes {
            return None;
        }
        if !held.iter().any(|m| Arc::ptr_eq(m, mirror)) {
            held.push(mirror.clone());
        }
        let ours = mirror.base + offset as usize;
        match pieces.last_mut() {
            Some((t, o, k)) if *t + *k == title && *o + *k == ours => *k += n,
            _ => pieces.push((title, ours, n)),
        }
    }
    Some(Behind { at, len, pieces, _held: held })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
        SEC_RESERVE, UnmapViewOfFile, VirtualAlloc,
    };

    const K64: usize = 64 << 10;

    /// Two views of `blocks` blocks of a reserved section, as the runtime
    /// makes them, the pages committed.
    fn two_views(blocks: usize) -> (usize, usize, usize) {
        let bytes = blocks * K64;
        // SAFETY: plain creation and mapping; the caller unmaps.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert!(one != 0 && two != 0);
            assert!(!VirtualAlloc(one as *const _, bytes, MEM_COMMIT, PAGE_READWRITE).is_null());
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        section_gone(section);
        for &view in views {
            crate::title_memory_remapped(view, bytes);
            // SAFETY: the views `two_views` mapped.
            unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        }
        // SAFETY: the section `two_views` made.
        unsafe { CloseHandle(section as _) };
    }

    #[test]
    fn a_write_behind_the_guards_is_seen_and_reported_at_every_view() {
        let bytes = 4 * K64;
        let (section, one, two) = two_views(4);
        mirror(section, bytes as u64);
        // A copy has taken both views: their blocks are guarded.
        assert!(crate::watch::writes(one, one + bytes, true).is_some());
        assert!(crate::watch::writes(two, two + bytes, true).is_some());
        let (asked, _) = crate::watch::writes(one, one + bytes, true).unwrap();
        let (asked_two, _) = crate::watch::writes(two, two + bytes, true).unwrap();
        let data: Vec<u8> = (0..K64 + 100).map(|i| (i % 251) as u8).collect();
        {
            let behind = of(one + K64 - 50, data.len()).expect("views of a section with a view of ours");
            behind.write(one + K64 - 50, data.as_ptr(), data.len());
        }
        // SAFETY: committed views.
        let seen = unsafe { std::slice::from_raw_parts((two + K64 - 50) as *const u8, data.len()) };
        assert!(seen == data);
        let fresh = |start: usize, asked: u64| -> Vec<usize> {
            let (_, pages) = crate::watch::writes(start, start + bytes, true).unwrap();
            pages.into_iter().filter(|&(_, w)| w > asked).map(|(p, _)| p - start / K64).collect()
        };
        assert_eq!(fresh(one, asked), [0, 1, 2]);
        assert_eq!(fresh(two, asked_two), [0, 1, 2]);
        // Noted once, and the guards still take a write of the title's.
        let (asked, pages) = crate::watch::writes(one, one + bytes, true).unwrap();
        assert!(pages.iter().all(|&(_, w)| w < asked));
        // SAFETY: committed; the write faults into the guards' handler.
        unsafe { ((one + 3 * K64) as *mut u8).write_volatile(7) };
        assert_eq!(fresh(one, asked), [3]);
        drop_views(section, &[one, two], bytes);
    }

    #[test]
    fn memory_that_is_no_view_is_not_written_behind() {
        let bytes = 2 * K64;
        let (section, one, two) = two_views(2);
        // No view of ours yet.
        assert!(of(one, K64).is_none());
        mirror(section, bytes as u64);
        assert!(of(one, bytes).is_some());
        // Past the end of the view.
        assert!(of(one + K64, 2 * K64).is_none() || one + bytes == two || two + bytes == one);
        let own = [0u8; 64];
        assert!(of(own.as_ptr() as usize, own.len()).is_none());
        section_gone(section);
        assert!(of(one, K64).is_none());
        drop_views(section, &[one, two], bytes);
    }
}
