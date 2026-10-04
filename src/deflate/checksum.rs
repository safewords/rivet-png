//! The two checksums the formats need: CRC-32 (PNG chunks, ISO 3309 / ITU-T
//! V.42, the reflected polynomial 0xEDB88320) and Adler-32 (the zlib trailer,
//! RFC 1950 §8.2).

const fn crc_table() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[0][n] = c;
        n += 1;
    }
    // Slicing by eight: t[j][n] is the CRC of byte n followed by j zero bytes.
    let mut n = 0;
    while n < 256 {
        let mut j = 1;
        while j < 8 {
            let prev = t[j - 1][n];
            t[j][n] = t[0][(prev & 0xFF) as usize] ^ (prev >> 8);
            j += 1;
        }
        n += 1;
    }
    t
}

static CRC: [[u32; 256]; 8] = crc_table();

/// A running CRC-32. Start from [`Crc32::new`], feed bytes with
/// [`update`](Crc32::update), read the value with [`finish`](Crc32::finish).
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// A CRC over no bytes yet.
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    /// Feeds `data`.
    pub fn update(&mut self, data: &[u8]) {
        self.0 = crate::simd::crc32_update(self.0, data, crc_table_update);
    }

    /// The CRC of everything fed so far.
    pub fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// The table-driven register update (slicing by four).
fn crc_table_update(mut c: u32, data: &[u8]) -> u32 {
    let (octets, rest) = data.as_chunks::<8>();
    for o in octets {
        let lo = c ^ u32::from_le_bytes([o[0], o[1], o[2], o[3]]);
        c = CRC[7][(lo & 0xFF) as usize]
            ^ CRC[6][((lo >> 8) & 0xFF) as usize]
            ^ CRC[5][((lo >> 16) & 0xFF) as usize]
            ^ CRC[4][(lo >> 24) as usize]
            ^ CRC[3][o[4] as usize]
            ^ CRC[2][o[5] as usize]
            ^ CRC[1][o[6] as usize]
            ^ CRC[0][o[7] as usize];
    }
    for &b in rest {
        c = CRC[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// The CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

const ADLER_MOD: u32 = 65521;
/// The most bytes that can be summed before `b` could overflow a `u32`
/// (RFC 1950 leaves the reduction schedule to the implementation).
const ADLER_RUN: usize = 5552;

/// The Adler-32 of `data`, continuing from `adler` (1 for a fresh sum).
pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    if let Some(v) = crate::simd::adler32_update(adler, data) {
        return v;
    }
    let mut a = adler & 0xFFFF;
    let mut b = adler >> 16;
    for run in data.chunks(ADLER_RUN) {
        for &x in run {
            a += x as u32;
            b += a;
        }
        a %= ADLER_MOD;
        b %= ADLER_MOD;
    }
    (b << 16) | a
}

/// The Adler-32 of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    adler32_update(1, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // The check values of the CRC-32 and Adler-32 catalogues.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        let big = vec![0xFFu8; 100_000];
        let mut slow = 0xFFFF_FFFFu32;
        for &b in &big {
            slow = CRC[0][((slow ^ b as u32) & 0xFF) as usize] ^ (slow >> 8);
        }
        assert_eq!(crc32(&big), slow ^ 0xFFFF_FFFF);
        let (mut a, mut b) = (1u64, 0u64);
        for &x in &big {
            a = (a + x as u64) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(adler32(&big) as u64, (b << 16) | a);
    }
}
