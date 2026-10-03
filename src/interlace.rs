//! Adam7 (PNG §8.2): the seven passes and moving pixels between a pass
//! and the full image.

/// (x start, y start, x step, y step) of the seven passes.
pub(crate) const ADAM7: [(u32, u32, u32, u32); 7] =
    [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

/// The size of pass `p` (0-based) of a `width` x `height` image; either may
/// be zero, in which case the pass is empty (and has no filter bytes).
pub(crate) fn pass_size(p: usize, width: u32, height: u32) -> (u32, u32) {
    let (x0, y0, dx, dy) = ADAM7[p];
    let w = if width > x0 { (width - x0).div_ceil(dx) } else { 0 };
    let h = if height > y0 { (height - y0).div_ceil(dy) } else { 0 };
    (w, h)
}

/// Copies pixel `sx` of packed row `src` to pixel `dx` of packed row `dst`,
/// `bpp` bits per pixel.
#[inline]
pub(crate) fn copy_pixel(src: &[u8], sx: usize, dst: &mut [u8], dx: usize, bpp: usize) {
    if bpp >= 8 {
        let n = bpp / 8;
        dst[dx * n..dx * n + n].copy_from_slice(&src[sx * n..sx * n + n]);
    } else {
        let sbit = sx * bpp;
        let v = (src[sbit / 8] >> (8 - bpp - sbit % 8)) & ((1u8 << bpp) - 1);
        let dbit = dx * bpp;
        let shift = 8 - bpp - dbit % 8;
        let mask = ((1u8 << bpp) - 1) << shift;
        dst[dbit / 8] = (dst[dbit / 8] & !mask) | (v << shift);
    }
}

/// Places the pixels of pass `p` (rows of `pass_row` bytes in `pass`) into
/// the image `out` (rows of `row` bytes).
pub(crate) fn scatter(p: usize, pass: &[u8], pass_row: usize, width: u32, height: u32, bpp: usize, out: &mut [u8], row: usize) {
    let (pw, ph) = pass_size(p, width, height);
    let (x0, y0, dx, dy) = ADAM7[p];
    for j in 0..ph as usize {
        let src = &pass[j * pass_row..(j + 1) * pass_row];
        let y = y0 as usize + j * dy as usize;
        let dst = &mut out[y * row..(y + 1) * row];
        for i in 0..pw as usize {
            copy_pixel(src, i, dst, x0 as usize + i * dx as usize, bpp);
        }
    }
}

/// Extracts the pixels of pass `p` from the image `img` (rows of `row`
/// bytes) as packed rows of the pass's width.
pub(crate) fn gather(p: usize, img: &[u8], row: usize, width: u32, height: u32, bpp: usize) -> (Vec<u8>, usize) {
    let (pw, ph) = pass_size(p, width, height);
    let pass_row = (pw as usize * bpp).div_ceil(8);
    let mut out = vec![0u8; pass_row * ph as usize];
    let (x0, y0, dx, dy) = ADAM7[p];
    for j in 0..ph as usize {
        let y = y0 as usize + j * dy as usize;
        let src = &img[y * row..(y + 1) * row];
        let dst = &mut out[j * pass_row..(j + 1) * pass_row];
        for i in 0..pw as usize {
            copy_pixel(src, x0 as usize + i * dx as usize, dst, i, bpp);
        }
    }
    (out, pass_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_cover_every_pixel_once() {
        for (w, h) in [(1, 1), (2, 3), (8, 8), (9, 17), (33, 5)] {
            let mut seen = vec![0u8; (w * h) as usize];
            for p in 0..7 {
                let (pw, ph) = pass_size(p, w, h);
                let (x0, y0, dx, dy) = ADAM7[p];
                for j in 0..ph {
                    for i in 0..pw {
                        seen[((y0 + j * dy) * w + x0 + i * dx) as usize] += 1;
                    }
                }
            }
            assert!(seen.iter().all(|&s| s == 1), "{w}x{h}");
        }
    }
}
