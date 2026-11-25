//! Hash maps and sets for keys of our own: addresses, slot numbers, the
//! words of descriptor records.
//!
//! The standard library's hash with SipHash, which no one who picks the
//! keys can make collide, at 20 to 40 ns a key. Nobody picks these against
//! us, and the recording threads look one up several times a descriptor.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// A map hashed with [`Fold`].
pub(crate) type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fold>>;

/// A set hashed with [`Fold`].
pub(crate) type Set<K> = HashSet<K, BuildHasherDefault<Fold>>;

/// Multiplies each word into what came before and folds the 128-bit
/// product: equal words give equal hashes, and two different runs of words
/// the same 64 bits about as often as chance does.
#[derive(Default)]
pub(crate) struct Fold(u64);

impl Fold {
    pub(crate) fn add(&mut self, word: u64) {
        let mixed = (self.0.rotate_left(23) ^ word).wrapping_add(0x9e37_79b9_7f4a_7c15);
        let wide = u128::from(mixed) * u128::from(0xd6e8_feb8_6659_fd93_u64);
        self.0 = wide as u64 ^ (wide >> 64) as u64;
    }
}

impl Hasher for Fold {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            self.add(u64::from_le_bytes(word.try_into().unwrap_or([0; 8])));
        }
        let rest = words.remainder();
        if !rest.is_empty() {
            let mut last = [0; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn of(value: impl Hash) -> u64 {
        let mut h = Fold::default();
        value.hash(&mut h);
        h.finish()
    }

    #[test]
    fn keys_near_each_other_hash_apart() {
        // Addresses, slots and record words come in runs: no two of a run
        // share a hash, nor too many of them the low bits a map takes.
        let mut seen = std::collections::HashSet::new();
        for a in 0..64u64 {
            for b in 0..256u64 {
                assert!(seen.insert(of((0x1_4000_0000 + (a << 16), b * 32, 7usize))));
            }
        }
        for shift in [0, 4, 8, 12, 16, 32] {
            let mut low = [0u32; 256];
            for i in 0..(256 * 64) as u64 {
                low[(of(i << shift) & 255) as usize] += 1;
            }
            assert!(low.iter().all(|&n| (16..=160).contains(&n)), "{shift}: {low:?}");
        }
        assert_ne!(of([1u32, 2, 3]), of([2u32, 1, 3]));
        assert_eq!(of((5u64, [1u32; 14])), of((5u64, [1u32; 14])));
    }
}
