//! Eight-way SHA-256 using AVX2.
//!
//! Without the SHA-NI extensions (absent on most Xeon/Skylake-era parts) a
//! software SHA-256 compression costs on the order of a thousand cycles, and
//! that — not dispatch or bookkeeping — is where all the time goes. AVX2 lets
//! us run eight independent hashes in parallel across the lanes of a 256-bit
//! register: the same round function, applied to eight nonces at once.
//!
//! Mining is the ideal shape for this because the eight inputs differ only in
//! the nonce and nothing depends on another lane's result.
//!
//! Safety: every function here requires AVX2. Callers must check
//! `is_x86_feature_detected!("avx2")` — `Sha256dHasher` does so once at
//! construction and stores the answer.

#![cfg(target_arch = "x86_64")]

use std::arch::x86_64::*;

pub const LANES: usize = 8;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Rotate right by `R`. `L` must be `32 - R` (Rust cannot compute it in a
/// const-generic position on stable).
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn rotr<const R: i32, const L: i32>(x: __m256i) -> __m256i {
    _mm256_or_si256(_mm256_srli_epi32::<R>(x), _mm256_slli_epi32::<L>(x))
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn small_sigma0(x: __m256i) -> __m256i {
    unsafe {
        _mm256_xor_si256(
            _mm256_xor_si256(rotr::<7, 25>(x), rotr::<18, 14>(x)),
            _mm256_srli_epi32::<3>(x),
        )
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn small_sigma1(x: __m256i) -> __m256i {
    unsafe {
        _mm256_xor_si256(
            _mm256_xor_si256(rotr::<17, 15>(x), rotr::<19, 13>(x)),
            _mm256_srli_epi32::<10>(x),
        )
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn big_sigma0(x: __m256i) -> __m256i {
    unsafe {
        _mm256_xor_si256(
            _mm256_xor_si256(rotr::<2, 30>(x), rotr::<13, 19>(x)),
            rotr::<22, 10>(x),
        )
    }
}

#[inline]
#[target_feature(enable = "avx2")]
unsafe fn big_sigma1(x: __m256i) -> __m256i {
    unsafe {
        _mm256_xor_si256(
            _mm256_xor_si256(rotr::<6, 26>(x), rotr::<11, 21>(x)),
            rotr::<25, 7>(x),
        )
    }
}

/// `(e & f) ^ (!e & g)`, written to avoid a separate NOT.
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn choose(e: __m256i, f: __m256i, g: __m256i) -> __m256i {
    _mm256_xor_si256(g, _mm256_and_si256(e, _mm256_xor_si256(f, g)))
}

/// `(a & b) ^ (a & c) ^ (b & c)`, via the usual two-operation identity.
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn majority(a: __m256i, b: __m256i, c: __m256i) -> __m256i {
    _mm256_xor_si256(
        _mm256_and_si256(a, b),
        _mm256_and_si256(c, _mm256_xor_si256(a, b)),
    )
}

/// One full 64-round compression of `w` into `state` (state is updated in
/// place, including the final feed-forward addition).
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn compress(state: &mut [__m256i; 8], w: &mut [__m256i; 64]) {
    unsafe {
        for i in 16..64 {
            w[i] = _mm256_add_epi32(
                _mm256_add_epi32(w[i - 16], small_sigma0(w[i - 15])),
                _mm256_add_epi32(w[i - 7], small_sigma1(w[i - 2])),
            );
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for i in 0..64 {
            let t1 = _mm256_add_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(h, big_sigma1(e)),
                    _mm256_add_epi32(choose(e, f, g), _mm256_set1_epi32(K[i] as i32)),
                ),
                w[i],
            );
            let t2 = _mm256_add_epi32(big_sigma0(a), majority(a, b, c));
            h = g;
            g = f;
            f = e;
            e = _mm256_add_epi32(d, t1);
            d = c;
            c = b;
            b = a;
            a = _mm256_add_epi32(t1, t2);
        }

        state[0] = _mm256_add_epi32(state[0], a);
        state[1] = _mm256_add_epi32(state[1], b);
        state[2] = _mm256_add_epi32(state[2], c);
        state[3] = _mm256_add_epi32(state[3], d);
        state[4] = _mm256_add_epi32(state[4], e);
        state[5] = _mm256_add_epi32(state[5], f);
        state[6] = _mm256_add_epi32(state[6], g);
        state[7] = _mm256_add_epi32(state[7], h);
    }
}

/// Double SHA-256 of eight 80-byte headers that differ only in the nonce.
///
/// `midstate` is the compression of the first 64 header bytes; `tail` is the
/// padded 16-byte remainder (with the nonce at bytes 12..16, overwritten here).
/// Writes each lane's most significant digest word — the value the share filter
/// compares against the target's top word.
///
/// # Safety
/// Requires AVX2.
#[target_feature(enable = "avx2")]
pub unsafe fn sha256d_top8(
    midstate: &[u32; 8],
    tail: &[u8; 64],
    nonces: &[u32; LANES],
    tops: &mut [u32; LANES],
) {
    unsafe {
        // First hash: the padded tail block. Every word is shared across lanes
        // except word 3, which holds the nonce.
        let mut w = [_mm256_setzero_si256(); 64];
        for i in 0..16 {
            let word = u32::from_be_bytes([
                tail[i * 4],
                tail[i * 4 + 1],
                tail[i * 4 + 2],
                tail[i * 4 + 3],
            ]);
            w[i] = _mm256_set1_epi32(word as i32);
        }
        w[3] = _mm256_setr_epi32(
            nonces[0] as i32,
            nonces[1] as i32,
            nonces[2] as i32,
            nonces[3] as i32,
            nonces[4] as i32,
            nonces[5] as i32,
            nonces[6] as i32,
            nonces[7] as i32,
        );

        let mut state = [_mm256_setzero_si256(); 8];
        for i in 0..8 {
            state[i] = _mm256_set1_epi32(midstate[i] as i32);
        }
        compress(&mut state, &mut w);

        // Second hash: the 32-byte first digest, padded. Now every word differs
        // per lane.
        let mut w2 = [_mm256_setzero_si256(); 64];
        w2[..8].copy_from_slice(&state[..8]);
        w2[8] = _mm256_set1_epi32(0x8000_0000u32 as i32);
        // w2[9..15] stay zero.
        w2[15] = _mm256_set1_epi32(256);

        let mut state2 = [_mm256_setzero_si256(); 8];
        for i in 0..8 {
            state2[i] = _mm256_set1_epi32(IV[i] as i32);
        }
        compress(&mut state2, &mut w2);

        // The digest's numeric top word, byte-swapped to match the little-endian
        // convention the rest of the miner uses.
        let mut lanes = [0u32; LANES];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, state2[7]);
        for i in 0..LANES {
            tops[i] = lanes[i].swap_bytes();
        }
    }
}

/// Single SHA-256 variant of [`sha256d_top8`].
///
/// # Safety
/// Requires AVX2.
#[target_feature(enable = "avx2")]
pub unsafe fn sha256_top8(
    midstate: &[u32; 8],
    tail: &[u8; 64],
    nonces: &[u32; LANES],
    tops: &mut [u32; LANES],
) {
    unsafe {
        let mut w = [_mm256_setzero_si256(); 64];
        for i in 0..16 {
            let word = u32::from_be_bytes([
                tail[i * 4],
                tail[i * 4 + 1],
                tail[i * 4 + 2],
                tail[i * 4 + 3],
            ]);
            w[i] = _mm256_set1_epi32(word as i32);
        }
        w[3] = _mm256_setr_epi32(
            nonces[0] as i32,
            nonces[1] as i32,
            nonces[2] as i32,
            nonces[3] as i32,
            nonces[4] as i32,
            nonces[5] as i32,
            nonces[6] as i32,
            nonces[7] as i32,
        );
        let mut state = [_mm256_setzero_si256(); 8];
        for i in 0..8 {
            state[i] = _mm256_set1_epi32(midstate[i] as i32);
        }
        compress(&mut state, &mut w);

        let mut lanes = [0u32; LANES];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, state[7]);
        for i in 0..LANES {
            tops[i] = lanes[i].swap_bytes();
        }
    }
}

pub fn available() -> bool {
    is_x86_feature_detected!("avx2")
}
