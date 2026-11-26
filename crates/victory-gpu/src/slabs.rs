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

/// The free ranges of the blocks, by size and by place, and the free bytes
/// of each block.
#[derive(Default)]
struct Ranges {
    by_size: BTreeSet<(u64, u32, u64)>,
    by_place: BTreeMap<(u32, u64), u64>,
    free_in: Vec<u64>,
}

/// How many free ranges that fit [`Ranges::take`] weighs at most.
const WEIGHED: usize = 4096;

impl Ranges {
    fn remove(&mut self, block: u32, at: u64) -> Option<u64> {
        let size = self.by_place.remove(&(block, at))?;
        self.by_size.remove(&(size, block, at));
        self.free_in[block as usize] -= size;
        Some(size)
    }

    fn insert(&mut self, block: u32, at: u64, size: u64) {
        self.by_place.insert((block, at), size);
        self.by_size.insert((size, block, at));
        let block = block as usize;
        if self.free_in.len() <= block {
            self.free_in.resize(block + 1, 0);
        }
        self.free_in[block] += size;
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

    /// Takes `size` bytes of a range that has them: in the fullest block
    /// that has one (the fewest free bytes), its smallest such range.
    ///
    /// It was the smallest range of all blocks, wherever it lay: new
    /// textures went into the emptiest blocks as readily as into the
    /// fullest, and a block is given back only when no texture is left in
    /// it. In run m31 the 89 blocks (5.7 GB of the card's memory, which
    /// Windows charges to the PC's commit as well) were 41 % free, in
    /// pieces, and not one of them wholly. Filling the fullest first lets
    /// the others empty as their textures go.
    fn take(&mut self, size: u64) -> Option<(u32, u64)> {
        let mut best: Option<(u64, u64, u32, u64)> = None;
        for &(len, block, at) in self.by_size.range((size, 0, 0)..).take(WEIGHED) {
            let key = (self.free_in[block as usize], len, block, at);
            if best.is_none_or(|b| key < b) {
                best = Some(key);
            }
        }
        let (_, len, block, at) = best?;
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
        free_in: Vec::new(),
    },
    resting: VecDeque::new(),
});

/// For the log: blocks alive, textures placed since the start.
pub(crate) static PLACED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// For the log: the bytes of the blocks that hold no texture now (free, and
/// resting), and the blocks wholly free.
pub(crate) fn free_bytes() -> (u64, u64, usize) {
    let blocks = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    let free = blocks.free.by_place.values().sum();
    let resting = blocks.resting.iter().map(|(_, p)| p.size).sum();
    let whole = blocks.free.by_size.range((BLOCK, 0, 0)..).count();
    (free, resting, whole)
}

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

/// The bytes of a block of upload memory that staging buffers are cut from.
const PIECE_BLOCK: u64 = 32 << 20;
/// The largest staging buffer cut from a block; larger ones are upload
/// buffers of their own, as before.
const PIECE_LARGEST: u64 = 8 << 20;
/// What a piece's place and size are whole multiples of: more than the 512
/// bytes a copy from a buffer into a texture needs its start aligned to.
const PIECE_GRAIN: u64 = 4 << 10;

/// Whether staging buffers of up to [`PIECE_LARGEST`] bytes are cut from
/// blocks of upload memory made and mapped once (`VICTORY_UPLOAD_PIECES`:
/// `0` off, `alt` twenty seconds with and twenty without, in turn; on by
/// default).
///
/// Why: each staging buffer was an upload buffer of its own, and making one
/// is a new allocation of the driver's, mapped, which waits for the card's
/// paging of it. The title's recording threads make them when they first
/// use a texture. In the opening cutscene, run c12, frames of 1.8 to 2.4 s
/// made 1,400 to 2,700 of them each, while the card stood idle: five of the
/// title's threads were in that making (allocation, map, the wait) at the
/// moments its render thread waited for them. Spares were found for about
/// three in four; a scene's new textures came in bursts the spares did not
/// hold.
///
/// Measured, the opening cutscene, runs of the same length: 2.86, 3.97 and
/// 3.72 fps with it (c13-c15), 1.58 to 2.61 without (seven runs, about 2.0
/// on average); upload buffers made in a run went from about 25,000 to
/// some 50, and the longest frames from 2.2-3.7 s to 1.7-2.4 s.
///
/// A piece is given back when its staging buffer is dropped, which is after
/// the last list that named it ran: the same moment a spare buffer was kept
/// for the next texture before. Blocks wholly free for [`PIECES_IDLE`] go,
/// but one: in run c13 (64 MB blocks, ten seconds) bursts of new textures
/// left 28 to 35 blocks alive while 50 to 630 MB of them were in use.
fn pieces_wanted() -> bool {
    static SWITCH: crate::turns::Switch = crate::turns::Switch::new(
        "VICTORY_UPLOAD_PIECES",
        true,
        "staging buffers are cut from blocks of upload memory made once",
    );
    SWITCH.on()
}

/// How long a block pieces are cut from is kept wholly free.
const PIECES_IDLE: Duration = Duration::from_secs(2);

/// The bytes all blocks pieces are cut from may hold
/// (`VICTORY_UPLOAD_PIECES_MB`, 1536); past them a staging buffer is an
/// upload buffer of its own, as before. Upload memory is the PC's memory,
/// and little of it is free in the opening cutscene: in run c14 the pass
/// over textures picked by index had 3.2 GB of staging buffers alive at
/// once, for about a second (8,500 of them made in two), and 17 MB the
/// second after. Made one by one, as before, they were too slow to pile
/// up so.
fn pieces_most() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let mb = std::env::var("VICTORY_UPLOAD_PIECES_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
        mb.unwrap_or(1536) << 20
    })
}

/// The blocks pieces are cut from (each an upload buffer and where it is
/// mapped), what of them is free, and since when each wholly free one is.
struct Pieces {
    blocks: Vec<Option<(ID3D12Resource, usize)>>,
    free: Ranges,
    empty_since: Vec<Option<Instant>>,
}

static PIECES: Mutex<Pieces> = Mutex::new(Pieces {
    blocks: Vec::new(),
    free: Ranges {
        by_size: BTreeSet::new(),
        by_place: BTreeMap::new(),
        free_in: Vec::new(),
    },
    empty_since: Vec::new(),
});

/// For the log: blocks of upload memory alive, pieces cut from them since
/// the start, and the bytes of those alive now.
pub(crate) static CUT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A mapped upload buffer for a staging buffer, from [`upload`].
pub(crate) struct Upload {
    pub(crate) buffer: ID3D12Resource,
    /// Where the staging buffer's bytes start, mapped.
    pub(crate) data: *mut u8,
    /// Where they start in `buffer`.
    pub(crate) origin: u64,
    /// Its bytes: at least those asked for.
    pub(crate) capacity: u64,
    /// Whether it was used before: its bytes are then what its last user
    /// left, not zeros.
    pub(crate) used: bool,
    /// The block and the place it was cut from, if it was.
    pub(crate) piece: Option<(u32, u64)>,
}

/// A piece of `size` bytes or more of a block, or `None` (no memory).
fn piece(host: &Host, size: u64) -> Option<Upload> {
    let size = size.max(1).next_multiple_of(PIECE_GRAIN);
    let cut = |pieces: &mut Pieces, block: u32, at: u64| {
        if let Some(e) = pieces.empty_since.get_mut(block as usize) {
            *e = None;
        }
        let (buffer, data) = pieces.blocks.get(block as usize)?.clone()?;
        CUT[1].fetch_add(1, Relaxed);
        CUT[2].fetch_add(size, Relaxed);
        Some(Upload {
            buffer,
            data: (data + at as usize) as *mut u8,
            origin: at,
            capacity: size,
            used: true,
            piece: Some((block, at)),
        })
    };
    {
        let mut pieces = PIECES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((block, at)) = pieces.free.take(size) {
            return cut(&mut pieces, block, at);
        }
    }
    if CUT[0].load(Relaxed) * PIECE_BLOCK >= pieces_most() {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::info!(
                mb = pieces_most() >> 20,
                "the blocks staging buffers are cut from hold all they may (VICTORY_UPLOAD_PIECES_MB): more staging \
                 buffers are upload buffers of their own until pieces come back"
            )
        });
        return None;
    }
    // Made with no lock held: other threads go on cutting meanwhile.
    let (buffer, data) = host.upload_buffer(PIECE_BLOCK)?;
    crate::host::name(&buffer, || "a block of upload memory staging buffers are cut from".into());
    let mut pieces = PIECES.lock().unwrap_or_else(|e| e.into_inner());
    let block = match pieces.blocks.iter().position(Option::is_none) {
        Some(i) => i,
        None => {
            pieces.blocks.push(None);
            pieces.blocks.len() - 1
        }
    };
    pieces.blocks[block] = Some((buffer, data as usize));
    if pieces.empty_since.len() <= block {
        pieces.empty_since.resize(block + 1, None);
    }
    CUT[0].fetch_add(1, Relaxed);
    pieces.free.add(block as u32, 0, PIECE_BLOCK);
    let (block, at) = pieces.free.take(size)?;
    cut(&mut pieces, block, at)
}

/// Gives a piece back.
fn piece_done(block: u32, at: u64, bytes: u64) {
    let mut pieces = PIECES.lock().unwrap_or_else(|e| e.into_inner());
    CUT[2].fetch_sub(bytes, Relaxed);
    if pieces.free.add(block, at, bytes) == PIECE_BLOCK
        && let Some(e) = pieces.empty_since.get_mut(block as usize)
    {
        *e = Some(Instant::now());
    }
}

/// Lets go of the blocks wholly free for `idle`, all but one.
fn let_go_idle_pieces(idle: Duration) {
    let mut gone = Vec::new();
    {
        let mut pieces = PIECES.lock().unwrap_or_else(|e| e.into_inner());
        let empty: Vec<usize> = (0..pieces.empty_since.len())
            .filter(|&b| pieces.empty_since[b].is_some_and(|since| since.elapsed() >= idle))
            .collect();
        let wholly = pieces.free.by_size.range((PIECE_BLOCK, 0, 0)..).count();
        for &b in empty.iter().take(wholly.saturating_sub(1)) {
            if pieces.free.by_place.get(&(b as u32, 0)) == Some(&PIECE_BLOCK) {
                pieces.free.remove(b as u32, 0);
                pieces.empty_since[b] = None;
                gone.extend(pieces.blocks[b].take());
                CUT[0].fetch_sub(1, Relaxed);
            }
        }
    }
    drop(gone);
}

/// A mapped upload buffer of `size` bytes or more for a staging buffer.
pub(crate) fn upload(host: &Host, size: u64) -> Option<Upload> {
    if pieces_wanted() && size <= PIECE_LARGEST
        && let Some(piece) = piece(host, size)
    {
        return Some(piece);
    }
    let whole = |buffer, data, capacity, used| Upload { buffer, data, origin: 0, capacity, used, piece: None };
    if spare_bytes() == 0 {
        let (buffer, data) = host.upload_buffer(size)?;
        return Some(whole(buffer, data, size, false));
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
        return Some(whole(spare.buffer, spare.data as *mut u8, class, true));
    }
    UPLOADS[0].fetch_add(1, Relaxed);
    let (buffer, data) = host.upload_buffer(class)?;
    Some(whole(buffer, data, class, false))
}

/// An upload buffer of `bytes` bytes from [`upload`] that no list names any
/// more: a piece is given back to its block, a whole buffer kept for the
/// next one.
pub(crate) fn upload_done(buffer: &ID3D12Resource, data: *mut u8, bytes: u64, piece: Option<(u32, u64)>) {
    if let Some((block, at)) = piece {
        piece_done(block, at, bytes);
        return;
    }
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

/// Lets go of the spare upload buffers nothing took in ten seconds, and of
/// the blocks pieces are cut from wholly free for [`PIECES_IDLE`] (all but
/// one).
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
    let_go_idle_pieces(PIECES_IDLE);
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
    fn the_fullest_block_is_filled_first() {
        let mut r = Ranges::default();
        r.add(0, 0, 100);
        r.add(1, 0, 100);
        // Block 1 has a texture: 40 of it free less.
        assert_eq!(r.take(40), Some((0, 0)));
        assert_eq!(r.free_in, vec![60, 100]);
        // The next goes into block 0 too, though block 1 has a bigger range.
        assert_eq!(r.take(50), Some((0, 40)));
        // Too big for block 0: block 1.
        assert_eq!(r.take(20), Some((1, 0)));
        assert_eq!(r.free_in, vec![10, 80]);
        // What goes comes back, joined.
        assert_eq!(r.add(0, 0, 40), 40);
        assert_eq!(r.free_in, vec![50, 80]);
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

    /// Pieces of a block of upload memory: each its own bytes, aligned for a
    /// copy into a texture, and the block whole again when all are back.
    #[test]
    fn pieces_are_their_own_and_come_back() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let sizes = [1u64, 896, 4096, 70_000, 1 << 20, PIECE_LARGEST];
        let taken: Vec<Upload> = sizes.iter().map(|&s| piece(host, s).expect("a piece")).collect();
        for (i, (u, &size)) in taken.iter().zip(&sizes).enumerate() {
            assert!(u.capacity >= size && u.origin.is_multiple_of(512), "{size}");
            let (block, at) = u.piece.expect("cut from a block");
            assert_eq!(at, u.origin);
            // SAFETY: the piece's own mapped bytes.
            unsafe { std::ptr::write_bytes(u.data, i as u8 + 1, u.capacity as usize) };
            for o in &taken[..i] {
                let (b, a) = o.piece.unwrap();
                assert!(b != block || a + o.capacity <= at || at + u.capacity <= a, "pieces overlap");
            }
        }
        for (i, u) in taken.iter().enumerate() {
            // SAFETY: as above; no other piece wrote these bytes.
            let bytes = unsafe { std::slice::from_raw_parts(u.data, u.capacity as usize) };
            assert!(bytes.iter().all(|&b| b == i as u8 + 1), "a piece written by another");
        }
        let back: Vec<(u32, u64, u64)> = taken.iter().map(|u| (u.piece.unwrap().0, u.origin, u.capacity)).collect();
        for u in taken {
            upload_done(&u.buffer, u.data, u.capacity, u.piece);
        }
        // Free again, joined with what is free beside them (other tests may
        // hold pieces of the same blocks).
        let pieces = PIECES.lock().unwrap();
        for (b, at, len) in back {
            let (&(fb, start), &free) = pieces.free.by_place.range(..=(b, at)).next_back().expect("a free range");
            assert!(fb == b && start + free >= at + len, "piece at {at} of block {b} not free again");
        }
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
