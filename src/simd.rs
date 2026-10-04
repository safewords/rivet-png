//! Vector kernels, chosen at run time, each with the portable code it
//! replaces kept as the reference.
//!
//! - CRC-32 by carry-less multiplication (PCLMULQDQ on x86-64, PMULL on
//!   AArch64): 64-byte strides folded four lanes at a time, then the last
//!   128 bits reduced through the table-driven CRC.
//! - Adler-32 32 bytes (AVX2) or 16 bytes (NEON) at a time.
//! - The Sub, Average and Paeth reverse filters a pixel at a time in 16-bit
//!   lanes (SSE4.1, or NEON, which is part of the AArch64 baseline).
//! - [`with_wide_vectors`] compiles the encoder's filter loops, which the
//!   compiler vectorises, for AVX2.
//!
//! Every kernel is integer arithmetic and gives exactly the portable
//! result; the tests below compare them on random and edge-case input. The
//! `force-scalar` feature compiles all of this out.

#![allow(unsafe_code)]

/// CRC-32 register update (the raw register, before the final inversion).
#[inline]
pub(crate) fn crc32_update(crc: u32, data: &[u8], table: impl Fn(u32, &[u8]) -> u32) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
    if data.len() >= 64 && std::arch::is_x86_feature_detected!("pclmulqdq") && std::arch::is_x86_feature_detected!("sse4.1")
    {
        // SAFETY: the features the function is compiled for were detected.
        let (crc, rest) = unsafe { x86::crc32_fold(crc, data, &table) };
        return table(crc, rest);
    }
    #[cfg(all(target_arch = "aarch64", not(feature = "force-scalar")))]
    if data.len() >= 64 && std::arch::is_aarch64_feature_detected!("aes") {
        // SAFETY: PMULL (the `aes` feature) was detected; NEON is baseline.
        let (crc, rest) = unsafe { arm::crc32_fold(crc, data, &table) };
        return table(crc, rest);
    }
    table(crc, data)
}

/// Adler-32 update; `None` when no vector kernel applies.
#[inline]
pub(crate) fn adler32_update(adler: u32, data: &[u8]) -> Option<u32> {
    #[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
    if data.len() >= 64 && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 was detected.
        return Some(unsafe { x86::adler32_avx2(adler, data) });
    }
    #[cfg(all(target_arch = "aarch64", not(feature = "force-scalar")))]
    if data.len() >= 64 {
        // SAFETY: NEON is part of the AArch64 baseline.
        return Some(unsafe { arm::adler32_neon(adler, data) });
    }
    let _ = (adler, data);
    None
}

/// Runs `f` compiled for AVX2 where the processor has it (`f` and what it
/// inlines are vectorised 32 bytes wide), else as it is.
#[inline(always)]
pub(crate) fn with_wide_vectors<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
    if std::arch::is_x86_feature_detected!("avx2") {
        #[target_feature(enable = "avx2")]
        fn avx2<R>(f: impl FnOnce() -> R) -> R {
            f()
        }
        // SAFETY: AVX2 was detected.
        return unsafe { avx2(f) };
    }
    f()
}

/// The constants for folding: `x^(n-1) mod P` bit-reflected into the top 32
/// bits of a 64-bit lane, so that a reflected carry-less product of a
/// 64-bit half and this lands as `half * x^n mod P`-equivalent in 128 bits.
#[allow(dead_code)]
const fn fold_const(n: u32) -> u64 {
    // x^(n-1) mod P in normal (unreflected) form, P = 0x1_04C1_1DB7.
    let mut r: u32 = 1; // x^0
    let mut i = 0;
    while i < n - 1 {
        let top = r & 0x8000_0000;
        r <<= 1;
        if top != 0 {
            r ^= 0x04C1_1DB7;
        }
        i += 1;
    }
    // Coefficient of x^e goes to bit 63 - e.
    (r.reverse_bits() as u64) << 32
}

/// Fold distances: four lanes of 128 bits (512) and one lane (128). Each
/// fold multiplies the low (higher-degree) half by x^(d+64) and the high
/// half by x^d.
#[allow(dead_code)]
const K512: (u64, u64) = (fold_const(512 + 64), fold_const(512));
#[allow(dead_code)]
const K128: (u64, u64) = (fold_const(128 + 64), fold_const(128));

#[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
mod x86 {
    use super::{K128, K512};
    use std::arch::x86_64::*;

    #[inline(always)]
    fn load(b: &[u8]) -> __m128i {
        let b: &[u8; 16] = b[..16].try_into().unwrap();
        // SAFETY: `b` is 16 readable bytes; the load is unaligned.
        unsafe { _mm_loadu_si128(b.as_ptr().cast()) }
    }

    #[inline]
    #[target_feature(enable = "pclmulqdq,sse2")]
    fn fold(x: __m128i, k: __m128i) -> __m128i {
        _mm_xor_si128(_mm_clmulepi64_si128::<0x00>(x, k), _mm_clmulepi64_si128::<0x11>(x, k))
    }

    /// Folds `data` down to one 128-bit value and returns the register for
    /// it (through `table`) and the bytes left over (fewer than 16).
    #[target_feature(enable = "pclmulqdq,sse4.1")]
    pub(super) fn crc32_fold<'a>(crc: u32, data: &'a [u8], table: &impl Fn(u32, &[u8]) -> u32) -> (u32, &'a [u8]) {
        let k512 = _mm_set_epi64x(K512.1 as i64, K512.0 as i64);
        let k128 = _mm_set_epi64x(K128.1 as i64, K128.0 as i64);
        let mut x = [load(&data[0..]), load(&data[16..]), load(&data[32..]), load(&data[48..])];
        x[0] = _mm_xor_si128(x[0], _mm_cvtsi32_si128(crc as i32));
        let mut rest = &data[64..];
        while rest.len() >= 64 {
            for (i, xi) in x.iter_mut().enumerate() {
                *xi = _mm_xor_si128(fold(*xi, k512), load(&rest[16 * i..]));
            }
            rest = &rest[64..];
        }
        let mut v = _mm_xor_si128(fold(x[0], k128), x[1]);
        v = _mm_xor_si128(fold(v, k128), x[2]);
        v = _mm_xor_si128(fold(v, k128), x[3]);
        while rest.len() >= 16 {
            v = _mm_xor_si128(fold(v, k128), load(rest));
            rest = &rest[16..];
        }
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&(_mm_cvtsi128_si64(v) as u64).to_le_bytes());
        bytes[8..].copy_from_slice(&(_mm_extract_epi64::<1>(v) as u64).to_le_bytes());
        (table(0, &bytes), rest)
    }

    /// The most bytes that can be summed before the 32-bit sums could
    /// overflow, rounded down to whole 32-byte vectors.
    const RUN: usize = 5536;

    #[target_feature(enable = "avx2")]
    pub(super) fn adler32_avx2(adler: u32, data: &[u8]) -> u32 {
        let mut a = adler & 0xFFFF;
        let mut b = adler >> 16;
        // Weights 32, 31, ..., 1 for the bytes of a vector.
        let weights = _mm256_setr_epi8(
            32, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4,
            3, 2, 1,
        );
        let ones = _mm256_set1_epi16(1);
        let zero = _mm256_setzero_si256();
        let (vecs, tail) = data.as_chunks::<32>();
        for run in vecs.chunks(RUN / 32) {
            let n = (run.len() * 32) as u64;
            let mut va = zero; // byte sums, 4 x u64
            let mut vprev = zero; // running sum of va before each vector, 8 x u32
            let mut vb = zero; // weighted sums, 8 x u32
            for v in run {
                // SAFETY: `v` is 32 readable bytes; the load is unaligned.
                let d = unsafe { _mm256_loadu_si256(v.as_ptr().cast()) };
                vprev = _mm256_add_epi32(vprev, va);
                va = _mm256_add_epi64(va, _mm256_sad_epu8(d, zero));
                vb = _mm256_add_epi32(vb, _mm256_madd_epi16(_mm256_maddubs_epi16(d, weights), ones));
            }
            let sum32 = |v: __m256i| -> u64 {
                let mut l = [0u32; 8];
                // SAFETY: `l` is 32 writable bytes.
                unsafe { _mm256_storeu_si256(l.as_mut_ptr().cast(), v) };
                l.iter().map(|&x| x as u64).sum()
            };
            let sum64 = |v: __m256i| -> u64 {
                let mut l = [0u64; 4];
                // SAFETY: `l` is 32 writable bytes.
                unsafe { _mm256_storeu_si256(l.as_mut_ptr().cast(), v) };
                l.iter().sum()
            };
            // va's u64 lanes hold sums in their low halves only, so summing
            // vprev as u32 lanes counts them once.
            let s = sum64(va);
            let bb = b as u64 + n * a as u64 + 32 * sum32(vprev) + sum32(vb);
            a = ((a as u64 + s) % 65521) as u32;
            b = (bb % 65521) as u32;
        }
        super::adler_tail(a, b, tail)
    }
}

#[cfg(all(target_arch = "aarch64", not(feature = "force-scalar")))]
mod arm {
    use super::{K128, K512};
    use std::arch::aarch64::*;

    #[inline(always)]
    fn load(b: &[u8]) -> uint8x16_t {
        let b: &[u8; 16] = b[..16].try_into().unwrap();
        // SAFETY: `b` is 16 readable bytes.
        unsafe { vld1q_u8(b.as_ptr()) }
    }

    #[inline]
    #[target_feature(enable = "neon,aes")]
    fn fold(x: uint8x16_t, k: (u64, u64)) -> uint8x16_t {
        let x = vreinterpretq_u64_u8(x);
        let lo = vmull_p64(vgetq_lane_u64::<0>(x), k.0);
        let hi = vmull_p64(vgetq_lane_u64::<1>(x), k.1);
        veorq_u8(vreinterpretq_u8_p128(lo), vreinterpretq_u8_p128(hi))
    }

    #[target_feature(enable = "neon,aes")]
    pub(super) fn crc32_fold<'a>(crc: u32, data: &'a [u8], table: &impl Fn(u32, &[u8]) -> u32) -> (u32, &'a [u8]) {
        let mut x = [load(&data[0..]), load(&data[16..]), load(&data[32..]), load(&data[48..])];
        x[0] = veorq_u8(x[0], vreinterpretq_u8_u32(vsetq_lane_u32::<0>(crc, vdupq_n_u32(0))));
        let mut rest = &data[64..];
        while rest.len() >= 64 {
            for (i, xi) in x.iter_mut().enumerate() {
                *xi = veorq_u8(fold(*xi, K512), load(&rest[16 * i..]));
            }
            rest = &rest[64..];
        }
        let mut v = veorq_u8(fold(x[0], K128), x[1]);
        v = veorq_u8(fold(v, K128), x[2]);
        v = veorq_u8(fold(v, K128), x[3]);
        while rest.len() >= 16 {
            v = veorq_u8(fold(v, K128), load(rest));
            rest = &rest[16..];
        }
        let mut bytes = [0u8; 16];
        // SAFETY: `bytes` is 16 writable bytes.
        unsafe { vst1q_u8(bytes.as_mut_ptr(), v) };
        (table(0, &bytes), rest)
    }

    /// As for AVX2, in 16-byte vectors.
    const RUN: usize = 5536;

    #[target_feature(enable = "neon")]
    pub(super) fn adler32_neon(adler: u32, data: &[u8]) -> u32 {
        let mut a = adler & 0xFFFF;
        let mut b = adler >> 16;
        const W: [u8; 16] = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
        // SAFETY: W is 16 readable bytes.
        let w = unsafe { vld1q_u8(W.as_ptr()) };
        let (vecs, tail) = data.as_chunks::<16>();
        for run in vecs.chunks(RUN / 16) {
            let n = (run.len() * 16) as u64;
            let mut va = vdupq_n_u32(0);
            let mut vprev = vdupq_n_u32(0);
            let mut vb = vdupq_n_u32(0);
            for v in run {
                // SAFETY: `v` is 16 readable bytes.
                let d = unsafe { vld1q_u8(v.as_ptr()) };
                vprev = vaddq_u32(vprev, va);
                va = vpadalq_u16(va, vpaddlq_u8(d));
                let lo = vmull_u8(vget_low_u8(d), vget_low_u8(w));
                let hi = vmull_u8(vget_high_u8(d), vget_high_u8(w));
                vb = vpadalq_u16(vb, lo);
                vb = vpadalq_u16(vb, hi);
            }
            let bb = b as u64 + n * a as u64 + 16 * vaddvq_u32(vprev) as u64 + vaddvq_u32(vb) as u64;
            a = ((a as u64 + vaddvq_u32(va) as u64) % 65521) as u32;
            b = (bb % 65521) as u32;
        }
        super::adler_tail(a, b, tail)
    }
}

/// The scalar Adler-32 over a short tail (fewer than 5552 bytes).
#[allow(dead_code)]
fn adler_tail(mut a: u32, mut b: u32, tail: &[u8]) -> u32 {
    for &x in tail {
        a += x as u32;
        b += a;
    }
    ((b % 65521) << 16) | (a % 65521)
}

/// Reverses Sub, Average or Paeth on `row` a pixel at a time, for `bpp` of
/// 3, 4, 6 or 8 (8- and 16-bit RGB and RGBA). Returns false (having done nothing) when no kernel applies.
#[inline]
pub(crate) fn unfilter(kind: u8, row: &mut [u8], prev: &[u8], bpp: usize) -> bool {
    #[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
    let usable = row.len() >= 16 && std::arch::is_x86_feature_detected!("sse4.1");
    #[cfg(all(target_arch = "aarch64", not(feature = "force-scalar")))]
    let usable = row.len() >= 16;
    #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(feature = "force-scalar")))]
    if usable {
        // SAFETY: SSE4.1 was detected; NEON is part of the AArch64
        // baseline.
        unsafe {
            match (kind, bpp) {
                (1, 3) => pixels::<1, 3>(row, prev),
                (1, 4) => pixels::<1, 4>(row, prev),
                (1, 6) => pixels::<1, 6>(row, prev),
                (1, 8) => pixels::<1, 8>(row, prev),
                (3, 3) => pixels::<3, 3>(row, prev),
                (3, 4) => pixels::<3, 4>(row, prev),
                (3, 6) => pixels::<3, 6>(row, prev),
                (3, 8) => pixels::<3, 8>(row, prev),
                (4, 3) => pixels::<4, 3>(row, prev),
                (4, 4) => pixels::<4, 4>(row, prev),
                (4, 6) => pixels::<4, 6>(row, prev),
                (4, 8) => pixels::<4, 8>(row, prev),
                _ => return false,
            }
        }
        return true;
    }
    let _ = (kind, row, prev, bpp);
    false
}

/// Eight 16-bit lanes, one per byte of a pixel (of up to 8 bytes).
#[cfg(all(target_arch = "x86_64", not(feature = "force-scalar")))]
mod lanes {
    use std::arch::x86_64::*;
    #[derive(Clone, Copy)]
    pub(super) struct V(__m128i);
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn zero() -> V {
        V(_mm_setzero_si128())
    }
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn load8(b: &[u8]) -> V {
        let v = u64::from_le_bytes(b[..8].try_into().unwrap());
        V(_mm_unpacklo_epi8(_mm_cvtsi64_si128(v as i64), _mm_setzero_si128()))
    }
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn bytes(v: V) -> [u8; 8] {
        (_mm_cvtsi128_si64(_mm_packus_epi16(_mm_and_si128(v.0, _mm_set1_epi16(0xFF)), v.0)) as u64).to_le_bytes()
    }
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn add(a: V, b: V) -> V {
        V(_mm_and_si128(_mm_add_epi16(a.0, b.0), _mm_set1_epi16(0xFF)))
    }
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn avg(a: V, b: V) -> V {
        V(_mm_srli_epi16::<1>(_mm_add_epi16(a.0, b.0)))
    }
    #[inline]
    #[target_feature(enable = "sse4.1")]
    pub(super) fn paeth(a: V, b: V, c: V) -> V {
        let bc = _mm_sub_epi16(b.0, c.0);
        let ac = _mm_sub_epi16(a.0, c.0);
        let pa = _mm_abs_epi16(bc);
        let pb = _mm_abs_epi16(ac);
        let pc = _mm_abs_epi16(_mm_add_epi16(ac, bc));
        let not_a = _mm_or_si128(_mm_cmpgt_epi16(pa, pb), _mm_cmpgt_epi16(pa, pc));
        let c_not_b = _mm_cmpgt_epi16(pb, pc);
        let bc_pick = _mm_blendv_epi8(b.0, c.0, c_not_b);
        V(_mm_blendv_epi8(a.0, bc_pick, not_a))
    }
}

#[cfg(all(target_arch = "aarch64", not(feature = "force-scalar")))]
mod lanes {
    use std::arch::aarch64::*;
    #[derive(Clone, Copy)]
    pub(super) struct V(int16x8_t);
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn zero() -> V {
        V(vdupq_n_s16(0))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn load8(b: &[u8]) -> V {
        let v = u64::from_le_bytes(b[..8].try_into().unwrap());
        V(vreinterpretq_s16_u16(vmovl_u8(vcreate_u8(v))))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn bytes(v: V) -> [u8; 8] {
        vget_lane_u64::<0>(vreinterpret_u64_u8(vmovn_u16(vreinterpretq_u16_s16(v.0)))).to_le_bytes()
    }
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn add(a: V, b: V) -> V {
        V(vandq_s16(vaddq_s16(a.0, b.0), vdupq_n_s16(0xFF)))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn avg(a: V, b: V) -> V {
        V(vshrq_n_s16::<1>(vaddq_s16(a.0, b.0)))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn paeth(a: V, b: V, c: V) -> V {
        let bc = vsubq_s16(b.0, c.0);
        let ac = vsubq_s16(a.0, c.0);
        let pa = vabsq_s16(bc);
        let pb = vabsq_s16(ac);
        let pc = vabsq_s16(vaddq_s16(ac, bc));
        let not_a = vorrq_u16(vcgtq_s16(pa, pb), vcgtq_s16(pa, pc));
        let c_not_b = vcgtq_s16(pb, pc);
        let bc_pick = vbslq_s16(c_not_b, c.0, b.0);
        V(vbslq_s16(not_a, bc_pick, a.0))
    }
}

/// One reverse filter, a pixel at a time: `F` is the filter type (1 Sub,
/// 3 Average, 4 Paeth). Each step loads eight bytes of the row and of the
/// row above, so the loop stops eight bytes short of the end; the rest is
/// finished by the portable code, which reads the reconstructed pixels
/// before it.
#[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(feature = "force-scalar")))]
#[cfg_attr(target_arch = "x86_64", target_feature(enable = "sse4.1"))]
#[cfg_attr(target_arch = "aarch64", target_feature(enable = "neon"))]
fn pixels<const F: u8, const BPP: usize>(row: &mut [u8], prev: &[u8]) {
    let bpp = BPP;
    use lanes::*;
    let n = row.len();
    let mut a = zero();
    let mut c = zero();
    let mut i = 0;
    while i + 8 <= n {
        let x = load8(&row[i..]);
        let r = match F {
            1 => add(x, a),
            3 => add(x, avg(a, load8(&prev[i..]))),
            _ => {
                let b = load8(&prev[i..]);
                let r = add(x, paeth(a, b, c));
                c = b;
                r
            }
        };
        row[i..i + BPP].copy_from_slice(&bytes(r)[..BPP]);
        a = r;
        i += bpp;
    }
    crate::filter::unfilter_scalar_from(F, row, prev, bpp, i);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::{Crc32, adler32_update, crc32};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    fn crc_bitwise(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    fn adler_naive(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u64, 0u64);
        for &x in data {
            a = (a + x as u64) % 65521;
            b = (b + a) % 65521;
        }
        ((b << 16) | a) as u32
    }

    #[test]
    fn crc_matches_bitwise() {
        let mut r = Rng(0x1234_5678_9ABC_DEF1);
        for len in (0..300).chain([1000, 4096, 65535, 65536, 100_003]) {
            let d = r.bytes(len);
            assert_eq!(crc32(&d), crc_bitwise(&d), "len {len}");
            // Split updates at awkward places.
            let cut = len / 3;
            let mut c = Crc32::new();
            c.update(&d[..cut]);
            c.update(&d[cut..]);
            assert_eq!(c.finish(), crc_bitwise(&d), "split {len}");
        }
        for fill in [0u8, 0xFF] {
            let d = vec![fill; 10_000];
            assert_eq!(crc32(&d), crc_bitwise(&d));
        }
    }

    #[test]
    fn adler_matches_naive() {
        let mut r = Rng(0x0F0F_1234_ABCD_0001);
        for len in (0..300).chain([5535, 5536, 5537, 5552, 11_072, 65_536, 200_001]) {
            let d = r.bytes(len);
            assert_eq!(adler32_update(1, &d), adler_naive(&d), "len {len}");
        }
        // All 0xFF is the worst case for the sums' growth.
        for len in [5536, 5552, 100_000, 1 << 20] {
            let d = vec![0xFFu8; len];
            assert_eq!(adler32_update(1, &d), adler_naive(&d), "ff {len}");
        }
        // Continuing from a large running value.
        let d = vec![0xFFu8; 70_000];
        let mid = adler32_update(1, &d[..12_345]);
        assert_eq!(adler32_update(mid, &d[12_345..]), adler_naive(&d));
    }

    #[test]
    fn unfilter_kernels_match_scalar() {
        let mut r = Rng(0xDEAD_BEEF_0000_0007);
        for bpp in 1..=8 {
            for len in [bpp, 15, 16, 17, 24, 31, 33, 64, 300, 1023].map(|l: usize| l.div_ceil(bpp) * bpp) {
                for kind in [1u8, 2, 3, 4] {
                    for trial in 0..20 {
                        let (row, prev) = if trial == 0 {
                            (vec![0xFF; len], vec![0xFF; len])
                        } else if trial == 1 {
                            (vec![0; len], vec![0xFF; len])
                        } else {
                            (r.bytes(len), r.bytes(len))
                        };
                        let mut want = row.clone();
                        crate::filter::unfilter_scalar_from(kind, &mut want, &prev, bpp, 0);
                        let mut got = row.clone();
                        if !unfilter(kind, &mut got, &prev, bpp) {
                            crate::filter::unfilter_scalar_from(kind, &mut got, &prev, bpp, 0);
                        }
                        assert_eq!(got, want, "kind {kind} bpp {bpp} len {len}");
                    }
                }
            }
        }
    }
}
