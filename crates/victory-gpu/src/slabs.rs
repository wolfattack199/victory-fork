//! Memory for what the texture pass makes by the thousand: the host copies
//! of textures the title fills, and the upload buffers they are filled
//! through.
//!
//! Each was a committed resource of its own, and the driver takes 0.5-1.4 ms
//! to make one: the pass over the textures picked by index spent 750 ms of
//! every world frame on some 340 of them (200 ms making textures, 450 ms
//! making their upload buffers; 40-180 ms of reading and detiling them), and
//! the 20,000 textures of a scene took minutes. Textures are now placed in
//! blocks of the card's memory and upload buffers are used again.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::Host;

/// The bytes of a block of the card's memory.
const BLOCK: u64 = 64 << 20;
/// The largest texture placed in a block; larger ones are few and are
/// committed resources as before.
const LARGEST: u64 = 16 << 20;
/// What places and sizes are whole multiples of.
const GRAIN: u64 = 64 << 10;
/// How long after its texture went a place is given again. Lists hold the
/// textures they name until they have run; this is for what a shader still
/// reads through a stale index.
const REST: Duration = Duration::from_secs(3);

/// Whether textures are placed in blocks (`VICTORY_TEXTURE_BLOCKS=0`: each a
/// committed resource, as before).
pub(crate) fn blocks_wanted() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_BLOCKS").is_none_or(|v| v != "0"))
}

/// The free ranges of the blocks, by size and by place.
#[derive(Default)]
struct Ranges {
    by_size: BTreeSet<(u64, u32, u64)>,
    by_place: BTreeMap<(u32, u64), u64>,
}

impl Ranges {
    fn remove(&mut self, block: u32, at: u64) -> Option<u64> {
        let size = self.by_place.remove(&(block, at))?;
        self.by_size.remove(&(size, block, at));
        Some(size)
    }

    fn insert(&mut self, block: u32, at: u64, size: u64) {
        self.by_place.insert((block, at), size);
        self.by_size.insert((size, block, at));
    }

    /// Adds `[at, at + size)` of `block`, joined with what is free beside
    /// it; the bytes of the range it became.
    fn add(&mut self, block: u32, mut at: u64, mut size: u64) -> u64 {
        if let Some((&(b, before), &len)) = self.by_place.range(..(block, at)).next_back()
            && b == block
            && before + len == at
        {
            self.remove(block, before);
            at = before;
            size += len;
        }
        if let Some(len) = self.remove(block, at + size) {
            size += len;
        }
        self.insert(block, at, size);
        size
    }

    /// Takes `size` bytes of the smallest range that has them.
    fn take(&mut self, size: u64) -> Option<(u32, u64)> {
        let &(len, block, at) = self.by_size.range((size, 0, 0)..).next()?;
        self.remove(block, at);
        if len > size {
            self.insert(block, at + size, len - size);
        }
        Some((block, at))
    }
}

/// The blocks, what of them is free, and the places whose texture went
/// within [`REST`].
struct Blocks {
    heaps: Vec<Option<ID3D12Heap>>,
    free: Ranges,
    resting: VecDeque<(Instant, Place)>,
}

#[derive(Clone, Copy)]
struct Place {
    block: u32,
    at: u64,
    size: u64,
}

static BLOCKS: Mutex<Blocks> = Mutex::new(Blocks {
    heaps: Vec::new(),
    free: Ranges {
        by_size: BTreeSet::new(),
        by_place: BTreeMap::new(),
    },
    resting: VecDeque::new(),
});

/// For the log: blocks alive, textures placed since the start.
pub(crate) static PLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A texture's place in a block, given back when it is dropped (after the
/// texture itself).
pub(crate) struct Slot(Place);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut blocks = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        blocks.resting.push_back((Instant::now(), self.0));
    }
}

impl Blocks {
    /// Places that have rested are free again; a block that is wholly free
    /// goes if another one is too.
    fn settle(&mut self) {
        while let Some(&(since, place)) = self.resting.front() {
            if since.elapsed() < REST {
                break;
            }
            self.resting.pop_front();
            if self.free.add(place.block, place.at, place.size) == BLOCK
                && self.free.by_size.range((BLOCK, 0, 0)..).count() > 1
            {
                self.free.remove(place.block, 0);
                self.heaps[place.block as usize] = None;
                PLACED[0].fetch_sub(1, Relaxed);
            }
        }
    }
}

/// A texture like `desc` placed in a block, in `state`, or `None` (too
/// large, a kind blocks do not hold, no memory): the caller makes it as a
/// resource of its own. What it holds is whatever its place held last, until
/// it is filled. Not for targets, depth or multisampled textures.
pub(crate) fn texture(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<(ID3D12Resource, Slot)> {
    if !blocks_wanted() {
        return None;
    }
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    if info.SizeInBytes == 0 || info.SizeInBytes > LARGEST || info.Alignment > GRAIN {
        return None;
    }
    let size = info.SizeInBytes.next_multiple_of(GRAIN);
    let (heap, slot) = {
        let mut blocks = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
        blocks.settle();
        let (block, at) = match blocks.free.take(size) {
            Some(found) => found,
            None => {
                let wanted = D3D12_HEAP_DESC {
                    SizeInBytes: BLOCK,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED,
                };
                let mut heap: Option<ID3D12Heap> = None;
                // SAFETY: plain creation.
                if let Err(e) = unsafe { device.CreateHeap(&wanted, &mut heap) } {
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| tracing::warn!(error = %e, "no block of the card's memory for textures"));
                    return None;
                }
                let heap = heap?;
                let block = match blocks.heaps.iter().position(Option::is_none) {
                    Some(i) => i,
                    None => {
                        blocks.heaps.push(None);
                        blocks.heaps.len() - 1
                    }
                };
                blocks.heaps[block] = Some(heap);
                PLACED[0].fetch_add(1, Relaxed);
                let block = block as u32;
                blocks.free.add(block, 0, BLOCK);
                blocks.free.take(size)?
            }
        };
        (blocks.heaps[block as usize].clone()?, Slot(Place { block, at, size }))
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: the place is this texture's alone, of its size and alignment.
    let made = unsafe { device.CreatePlacedResource(&heap, slot.0.at, desc, state, None, &mut texture) };
    if let Err(e) = made {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::warn!(error = %e, ?desc, "a texture not placed in a block: made alone"));
        return None;
    }
    PLACED[1].fetch_add(1, Relaxed);
    Some((texture?, slot))
}

/// The bytes a spare upload buffer for `size` bytes has: a multiple of 64
/// KiB up to a megabyte, above it at most an eighth more.
fn upload_class(size: u64) -> u64 {
    let size = size.max(1);
    let step = if size <= 1 << 20 { GRAIN } else { (size.next_power_of_two() >> 4).max(GRAIN) };
    size.next_multiple_of(step)
}

/// An upload buffer nothing uses: mapped at `data`.
struct Spare {
    buffer: ID3D12Resource,
    data: usize,
    since: Instant,
}

struct Spares {
    by_class: BTreeMap<u64, Vec<Spare>>,
    bytes: u64,
}

static SPARES: Mutex<Spares> = Mutex::new(Spares {
    by_class: BTreeMap::new(),
    bytes: 0,
});

/// For the log: upload buffers made, and used again, since the start.
pub(crate) static UPLOADS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The bytes of spare upload buffers kept (`VICTORY_STAGING_POOL_MB`, 384;
/// 0: none).
fn spare_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let mb = std::env::var("VICTORY_STAGING_POOL_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        mb.unwrap_or(384) << 20
    })
}

/// A mapped upload buffer of `size` bytes or more: the buffer, its memory,
/// its bytes, and whether it was used before (its bytes are then what its
/// last user left, not zeros).
pub(crate) fn upload(host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8, u64, bool)> {
    if spare_bytes() == 0 {
        let (buffer, data) = host.upload_buffer(size)?;
        return Some((buffer, data, size, false));
    }
    let class = upload_class(size);
    let spare = {
        let mut spares = SPARES.lock().unwrap_or_else(|e| e.into_inner());
        let spare = spares.by_class.get_mut(&class).and_then(Vec::pop);
        if spare.is_some() {
            spares.bytes -= class;
        }
        spare
    };
    if let Some(spare) = spare {
        UPLOADS[1].fetch_add(1, Relaxed);
        return Some((spare.buffer, spare.data as *mut u8, class, true));
    }
    UPLOADS[0].fetch_add(1, Relaxed);
    let (buffer, data) = host.upload_buffer(class)?;
    Some((buffer, data, class, false))
}

/// An upload buffer of `bytes` bytes from [`upload`] that no list names any
/// more: kept for the next one.
pub(crate) fn upload_done(buffer: &ID3D12Resource, data: *mut u8, bytes: u64) {
    if bytes != upload_class(bytes) {
        return;
    }
    let mut spares = SPARES.lock().unwrap_or_else(|e| e.into_inner());
    if spares.bytes + bytes > spare_bytes() {
        return;
    }
    spares.bytes += bytes;
    spares.by_class.entry(bytes).or_default().push(Spare {
        buffer: buffer.clone(),
        data: data as usize,
        since: Instant::now(),
    });
}

/// Lets go of the spare upload buffers nothing took in ten seconds.
pub(crate) fn let_go_idle() {
    const IDLE: Duration = Duration::from_secs(10);
    let mut gone = Vec::new();
    {
        let mut spares = SPARES.lock().unwrap_or_else(|e| e.into_inner());
        let mut bytes = 0;
        for (class, all) in &mut spares.by_class {
            // The last used are last, and taken first.
            let idle = all.iter().position(|s| s.since.elapsed() < IDLE).unwrap_or(all.len());
            bytes += class * idle as u64;
            gone.extend(all.drain(..idle));
        }
        spares.bytes -= bytes;
    }
    drop(gone);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_split_and_join() {
        let mut r = Ranges::default();
        assert_eq!(r.add(0, 0, 100), 100);
        assert_eq!(r.take(30), Some((0, 0)));
        assert_eq!(r.take(30), Some((0, 30)));
        // 60..100 is left.
        assert_eq!(r.take(50), None);
        assert_eq!(r.add(0, 0, 30), 30);
        // The smallest range that fits is taken.
        assert_eq!(r.take(30), Some((0, 0)));
        assert_eq!(r.add(0, 0, 30), 30);
        // Joined with both neighbours it is whole again.
        assert_eq!(r.add(0, 30, 30), 100);
        assert_eq!(r.by_place.len(), 1);
        assert_eq!(r.by_size.len(), 1);
    }

    #[test]
    fn ranges_of_two_blocks_are_not_joined() {
        let mut r = Ranges::default();
        r.add(0, 0, 100);
        r.add(1, 0, 100);
        assert_eq!(r.by_place.len(), 2);
        assert_eq!(r.take(100), Some((0, 0)));
        assert_eq!(r.take(100), Some((1, 0)));
        assert_eq!(r.take(1), None);
    }

    #[test]
    fn upload_classes_hold_their_sizes() {
        for size in [0, 1, 65_536, 65_537, 1 << 20, (1 << 20) + 1, 5_592_405, 22_369_621] {
            let class = upload_class(size);
            assert!(class >= size, "{size}");
            assert!(class <= size.max(1).next_multiple_of(GRAIN) + size / 8 + GRAIN, "{size}: {class}");
            assert_eq!(upload_class(class), class, "{size}");
        }
    }
}
