//! Pluggable proof-of-work algorithms.
//!
//! The hot loop lives behind two traits arranged so that *no dynamic dispatch
//! happens per hash*. `Algorithm` is a trait object (one virtual call per
//! worker thread, at spawn time); it immediately hands off to a generic worker
//! monomorphised over a concrete `Hasher`, so the compiler can inline the
//! compression function into the loop.
//!
//! `Hasher` is deliberately split into `top` and `digest`: `top` returns just
//! the most significant 32 bits of the digest, which is all the share filter
//! needs. The full 32 bytes are only materialised for the ~1-in-4-billion
//! hashes that survive it.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sha2::compress256;
use sha2::digest::generic_array::GenericArray;
use sha2::digest::generic_array::typenum::U64;
use tokio::sync::mpsc::UnboundedSender;

use super::{Share, WorkSlot};

/// SHA-256 initial state.
const IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Whether the vectorised eight-way path is usable on this machine.
pub fn vector_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        super::sha256_avx2::available()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Human-readable name of the active backend, for the dashboard.
pub fn backend_name() -> &'static str {
    if vector_available() {
        "AVX2 8-way"
    } else {
        "scalar"
    }
}

#[inline(always)]
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let block = GenericArray::<u8, U64>::from_slice(block);
    compress256(state, std::slice::from_ref(block));
}

pub trait Hasher: Send + Sized {
    /// Build a hasher bound to a 76-byte header prefix (everything but the
    /// nonce). Any work that does not depend on the nonce belongs here.
    fn new(header: &[u8; 76]) -> Self;

    /// Hash `nonce` and return the most significant 32 bits of the digest,
    /// interpreted as a big-endian number — directly comparable with the top
    /// word of a target.
    fn top(&mut self, nonce: u32) -> u32;

    /// Write the full digest of the most recent `top` call, in the algorithm's
    /// native (little-endian, for SHA-256d) byte order.
    fn digest(&self, out: &mut [u8; 32]);

    /// Hash `LANES` consecutive nonces starting at `base`, writing each one's
    /// top word. Implementations with a vector path override this; the default
    /// is a scalar loop.
    fn top_lanes(&mut self, base: u32, tops: &mut [u32; LANES]) {
        for (i, top) in tops.iter_mut().enumerate() {
            *top = self.top(base.wrapping_add(i as u32));
        }
    }
}

/// Nonces hashed per vector step. Eight is the AVX2 width; the scalar
/// fallback just loops.
pub const LANES: usize = 8;

pub trait Algorithm: Send + Sync {
    fn id(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// Pool difficulty is multiplied by this before being turned into a target
    /// (SHA-256d uses 1, scrypt-family chains use 65536).
    fn diff_multiplier(&self) -> f64 {
        1.0
    }
    /// Run one hashing thread to completion. Implementations call
    /// `worker::run::<ConcreteHasher>` so the loop is fully monomorphised.
    fn run_worker(
        &self,
        slot: Arc<WorkSlot>,
        stop: Arc<AtomicBool>,
        shares: UnboundedSender<Share>,
        index: usize,
    );
    /// Same, for the standalone benchmark.
    fn bench(&self, threads: usize, seconds: u64) -> f64;
}

// ---------------------------------------------------------------- sha256d ---

/// Double SHA-256 over an 80-byte block header.
///
/// The first 64 bytes of the header never change within a work unit, so their
/// compression output (the "midstate") is computed once in `new`. Each nonce
/// then costs exactly two compression calls: one for the 16-byte tail block,
/// one for the second hash's single padded block.
pub struct Sha256dHasher {
    /// Set once at construction; the CPUID check must not happen per hash.
    vector: bool,
    midstate: [u32; 8],
    /// Header bytes 64..80 plus SHA-256 padding. Only 12..16 (the nonce)
    /// changes per iteration.
    tail: [u8; 64],
    /// First digest plus padding, reused as the second hash's only block.
    second: [u8; 64],
    state: [u32; 8],
}

impl Hasher for Sha256dHasher {
    fn new(header: &[u8; 76]) -> Self {
        let mut midstate = IV;
        let mut first = [0u8; 64];
        first.copy_from_slice(&header[0..64]);
        compress(&mut midstate, &first);

        // 80-byte message: 16 bytes of data, 0x80 terminator, length in bits.
        let mut tail = [0u8; 64];
        tail[0..12].copy_from_slice(&header[64..76]);
        tail[16] = 0x80;
        tail[56..64].copy_from_slice(&(80u64 * 8).to_be_bytes());

        // 32-byte message: digest, 0x80 terminator, length in bits.
        let mut second = [0u8; 64];
        second[32] = 0x80;
        second[56..64].copy_from_slice(&(32u64 * 8).to_be_bytes());

        Self {
            vector: vector_available(),
            midstate,
            tail,
            second,
            state: IV,
        }
    }

    #[inline(always)]
    fn top(&mut self, nonce: u32) -> u32 {
        self.tail[12..16].copy_from_slice(&nonce.to_be_bytes());
        let mut state = self.midstate;
        compress(&mut state, &self.tail);

        for (i, word) in state.iter().enumerate() {
            self.second[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        let mut state = IV;
        compress(&mut state, &self.second);
        self.state = state;

        // The digest's last four bytes are the most significant end of its
        // little-endian numeric value, and they are `state[7]` big-endian.
        state[7].swap_bytes()
    }

    #[inline(always)]
    fn digest(&self, out: &mut [u8; 32]) {
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
    }

    fn top_lanes(&mut self, base: u32, tops: &mut [u32; LANES]) {
        #[cfg(target_arch = "x86_64")]
        if self.vector {
            let mut nonces = [0u32; LANES];
            for (i, n) in nonces.iter_mut().enumerate() {
                *n = base.wrapping_add(i as u32);
            }
            // Safety: `vector` is only true when AVX2 was detected.
            unsafe {
                super::sha256_avx2::sha256d_top8(&self.midstate, &self.tail, &nonces, tops);
            }
            return;
        }
        for (i, top) in tops.iter_mut().enumerate() {
            *top = self.top(base.wrapping_add(i as u32));
        }
    }
}

pub struct Sha256d;

impl Algorithm for Sha256d {
    fn id(&self) -> &'static str {
        "sha256d"
    }
    fn description(&self) -> &'static str {
        "Double SHA-256 (Bitcoin, Bitcoin Cash, Peercoin, DigiByte...)"
    }
    fn run_worker(
        &self,
        slot: Arc<WorkSlot>,
        stop: Arc<AtomicBool>,
        shares: UnboundedSender<Share>,
        index: usize,
    ) {
        super::worker::run::<Sha256dHasher>(slot, stop, shares, index);
    }
    fn bench(&self, threads: usize, seconds: u64) -> f64 {
        super::worker::bench::<Sha256dHasher>(threads, seconds)
    }
}

// --------------------------------------------------------------- sha256 -----

/// Single SHA-256, used by a few merge-mined chains.
pub struct Sha256Hasher {
    vector: bool,
    midstate: [u32; 8],
    tail: [u8; 64],
    state: [u32; 8],
}

impl Hasher for Sha256Hasher {
    fn new(header: &[u8; 76]) -> Self {
        let mut midstate = IV;
        let mut first = [0u8; 64];
        first.copy_from_slice(&header[0..64]);
        compress(&mut midstate, &first);
        let mut tail = [0u8; 64];
        tail[0..12].copy_from_slice(&header[64..76]);
        tail[16] = 0x80;
        tail[56..64].copy_from_slice(&(80u64 * 8).to_be_bytes());
        Self {
            vector: vector_available(),
            midstate,
            tail,
            state: IV,
        }
    }

    #[inline(always)]
    fn top(&mut self, nonce: u32) -> u32 {
        self.tail[12..16].copy_from_slice(&nonce.to_be_bytes());
        let mut state = self.midstate;
        compress(&mut state, &self.tail);
        self.state = state;
        state[7].swap_bytes()
    }

    #[inline(always)]
    fn digest(&self, out: &mut [u8; 32]) {
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
    }

    fn top_lanes(&mut self, base: u32, tops: &mut [u32; LANES]) {
        #[cfg(target_arch = "x86_64")]
        if self.vector {
            let mut nonces = [0u32; LANES];
            for (i, n) in nonces.iter_mut().enumerate() {
                *n = base.wrapping_add(i as u32);
            }
            // Safety: `vector` is only true when AVX2 was detected.
            unsafe {
                super::sha256_avx2::sha256_top8(&self.midstate, &self.tail, &nonces, tops);
            }
            return;
        }
        for (i, top) in tops.iter_mut().enumerate() {
            *top = self.top(base.wrapping_add(i as u32));
        }
    }
}

pub struct Sha256Single;

impl Algorithm for Sha256Single {
    fn id(&self) -> &'static str {
        "sha256"
    }
    fn description(&self) -> &'static str {
        "Single SHA-256"
    }
    fn run_worker(
        &self,
        slot: Arc<WorkSlot>,
        stop: Arc<AtomicBool>,
        shares: UnboundedSender<Share>,
        index: usize,
    ) {
        super::worker::run::<Sha256Hasher>(slot, stop, shares, index);
    }
    fn bench(&self, threads: usize, seconds: u64) -> f64 {
        super::worker::bench::<Sha256Hasher>(threads, seconds)
    }
}

// -------------------------------------------------------------- registry ----

static SHA256D: Sha256d = Sha256d;
static SHA256: Sha256Single = Sha256Single;

pub fn lookup(id: &str) -> Option<&'static dyn Algorithm> {
    match id.to_ascii_lowercase().as_str() {
        "sha256d" | "btc" => Some(&SHA256D),
        "sha256" => Some(&SHA256),
        _ => None,
    }
}

pub fn names() -> Vec<&'static str> {
    vec!["sha256d", "sha256"]
}

pub fn all() -> Vec<&'static dyn Algorithm> {
    vec![&SHA256D, &SHA256]
}

// --------------------------------------------------------------- targets ----

/// Difficulty-1 target, big-endian: `0x00000000FFFF0000...`.
pub const DIFF1: [u8; 32] = {
    let mut t = [0u8; 32];
    t[4] = 0xff;
    t[5] = 0xff;
    t
};

/// Convert a pool difficulty into a 256-bit big-endian target.
///
/// This is the classic word-wise approximation used by cpuminer: exact enough
/// that a share we accept is a share the pool accepts, and it avoids dragging
/// in a bignum dependency for the hot path.
pub fn diff_to_target(difficulty: f64) -> [u8; 32] {
    let mut d = if difficulty.is_finite() && difficulty > 0.0 {
        difficulty
    } else {
        1.0
    };
    let mut k = 6usize;
    while k > 0 && d > 1.0 {
        d /= 4294967296.0;
        k -= 1;
    }
    let m = (4294901760.0f64 / d) as u64; // 0xFFFF0000 / d
    if m == 0 && k == 6 {
        return [0xff; 32];
    }
    let mut words = [0u32; 8]; // words[0] = least significant
    words[k] = m as u32;
    if k + 1 < 8 {
        words[k + 1] = (m >> 32) as u32;
    }
    let mut out = [0u8; 32];
    // The index is the point here: word i of a little-endian-word target lands
    // at the mirrored byte offset. Iterating the slice directly would obscure
    // that, so the range loop stays.
    #[allow(clippy::needless_range_loop)]
    for i in 0..8 {
        let off = (7 - i) * 4;
        out[off..off + 4].copy_from_slice(&words[i].to_be_bytes());
    }
    out
}

/// Most significant 32 bits of a target, for the worker's fast filter.
#[inline(always)]
pub fn target_top(target: &[u8; 32]) -> u32 {
    u32::from_be_bytes([target[0], target[1], target[2], target[3]])
}

/// Difficulty of a found hash, for "best share" reporting.
/// `hash` is little-endian (native SHA-256d output order).
pub fn hash_difficulty(hash: &[u8; 32]) -> f64 {
    let mut value = 0.0f64;
    // Big-endian numeric view, most significant byte first.
    for i in (0..32).rev() {
        value = value * 256.0 + hash[i] as f64;
    }
    if value <= 0.0 {
        return f64::INFINITY;
    }
    let mut diff1 = 0.0f64;
    for b in DIFF1.iter() {
        diff1 = diff1 * 256.0 + *b as f64;
    }
    diff1 / value
}

/// `true` if the little-endian `hash` is numerically <= the big-endian `target`.
#[inline(always)]
pub fn meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    for i in 0..32 {
        let h = hash[31 - i];
        let t = target[i];
        if h != t {
            return h < t;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff1_roundtrip() {
        assert_eq!(diff_to_target(1.0), DIFF1);
    }

    #[test]
    fn higher_difficulty_is_a_smaller_target() {
        let a = diff_to_target(1.0);
        let b = diff_to_target(1024.0);
        assert!(b < a, "target should shrink as difficulty grows");
    }

    #[test]
    fn target_comparison_respects_endianness() {
        let mut hash = [0u8; 32];
        hash[31] = 0x00;
        assert!(meets_target(&hash, &DIFF1));
        hash[31] = 0xff;
        assert!(!meets_target(&hash, &DIFF1));
    }

    fn genesis_prefix() -> ([u8; 76], u32) {
        let header = hex::decode(concat!(
            "0100000000000000000000000000000000000000000000000000000000000000",
            "000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa",
            "4b1e5e4a29ab5f49ffff001d1dac2b7c"
        ))
        .unwrap();
        let mut prefix = [0u8; 76];
        prefix.copy_from_slice(&header[0..76]);
        let nonce = u32::from_be_bytes(header[76..80].try_into().unwrap());
        (prefix, nonce)
    }

    #[test]
    fn sha256d_matches_the_bitcoin_genesis_block() {
        let (prefix, nonce) = genesis_prefix();
        let mut hasher = Sha256dHasher::new(&prefix);
        hasher.top(nonce);
        let mut out = [0u8; 32];
        hasher.digest(&mut out);
        out.reverse();
        assert_eq!(
            hex::encode(out),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
    }

    /// `top` must agree with the full digest, otherwise the worker's filter
    /// would silently drop or invent shares.
    #[test]
    fn top_word_matches_the_full_digest() {
        let (prefix, base) = genesis_prefix();
        let mut hasher = Sha256dHasher::new(&prefix);
        let mut out = [0u8; 32];
        for offset in 0..512u32 {
            let nonce = base.wrapping_add(offset);
            let top = hasher.top(nonce);
            hasher.digest(&mut out);
            let expected = u32::from_le_bytes([out[28], out[29], out[30], out[31]]);
            assert_eq!(top, expected, "nonce {nonce}");
        }
    }

    /// The eight-way vector path must agree with the scalar one exactly --
    /// a mismatch would mean silently dropped or invented shares.
    #[test]
    fn vector_path_agrees_with_scalar() {
        let (prefix, base) = genesis_prefix();
        let mut hasher = Sha256dHasher::new(&prefix);
        let mut single = Sha256Hasher::new(&prefix);
        for step in 0..16u32 {
            let start = base.wrapping_add(step * LANES as u32);

            let mut vector = [0u32; LANES];
            hasher.top_lanes(start, &mut vector);
            for (i, got) in vector.iter().enumerate() {
                let expected = hasher.top(start.wrapping_add(i as u32));
                assert_eq!(*got, expected, "sha256d lane {i} at {start}");
            }

            let mut vector = [0u32; LANES];
            single.top_lanes(start, &mut vector);
            for (i, got) in vector.iter().enumerate() {
                let expected = single.top(start.wrapping_add(i as u32));
                assert_eq!(*got, expected, "sha256 lane {i} at {start}");
            }
        }
    }

    /// The optimised compress256 path must produce exactly what the plain
    /// `Digest` API does.
    #[test]
    fn compress_path_matches_the_reference_digest_api() {
        use sha2::{Digest, Sha256};
        let (prefix, base) = genesis_prefix();
        let mut hasher = Sha256dHasher::new(&prefix);
        let mut single = Sha256Hasher::new(&prefix);
        let mut out = [0u8; 32];
        for offset in 0..64u32 {
            let nonce = base.wrapping_add(offset);
            let mut header = [0u8; 80];
            header[0..76].copy_from_slice(&prefix);
            header[76..80].copy_from_slice(&nonce.to_be_bytes());

            hasher.top(nonce);
            hasher.digest(&mut out);
            let reference = Sha256::digest(Sha256::digest(header));
            assert_eq!(
                out.as_slice(),
                reference.as_slice(),
                "sha256d nonce {nonce}"
            );

            single.top(nonce);
            single.digest(&mut out);
            let reference = Sha256::digest(header);
            assert_eq!(out.as_slice(), reference.as_slice(), "sha256 nonce {nonce}");
        }
    }
}
