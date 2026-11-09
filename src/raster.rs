//! PWG Raster and Apple Raster (URF) encoders, the two formats driverless printers actually eat.
use image::RgbImage;
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format { Pwg, Urf }

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Srgb8, Gray8, Black1 }

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Sides { One, Long, Short }

#[derive(Clone, Debug)]
pub struct Spec {
    pub format: Format,
    pub kind: Kind,
    pub xdpi: u32,
    pub ydpi: u32,
    pub width: u32,
    pub height: u32,
    pub media: String,
    pub sides: Sides,
    pub back: String,
}

impl Spec {
    /// Which way the back of a duplex sheet must be pre-flipped: (mirror x, mirror y).
    pub fn back_flip(&self) -> (bool, bool) {
        let tumble = self.sides == Sides::Short;
        if self.sides == Sides::One { return (false, false); }
        match self.back.as_str() {
            "flipped" => if tumble { (true, false) } else { (false, true) },
            "manual-tumble" => (tumble, tumble),
            "rotated" => (!tumble, !tumble),
            _ => (false, false),
        }
    }
}

/// Converts RGB rows into the bytes for this raster kind.
pub fn to_pixels(img: &RgbImage, kind: Kind) -> (Vec<u8>, usize, usize) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let luma = |p: &[u8]| ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u8;
    match kind {
        Kind::Srgb8 => (img.as_raw().clone(), w * 3, 3),
        Kind::Gray8 => (img.as_raw().chunks_exact(3).map(luma).collect(), w, 1),
        Kind::Black1 => {
            // Floyd-Steinberg, one error row at a time.
            let stride = w.div_ceil(8);
            let mut out = vec![0u8; stride * h];
            let (mut cur, mut next) = (vec![0i32; w + 2], vec![0i32; w + 2]);
            for (y, row) in img.as_raw().chunks_exact(w * 3).enumerate() {
                for x in 0..w {
                    let v = luma(&row[x * 3..x * 3 + 3]) as i32 + cur[x + 1] / 16;
                    let black = v < 128;
                    if black { out[y * stride + x / 8] |= 0x80 >> (x % 8); }
                    let e = v - if black { 0 } else { 255 };
                    cur[x + 2] += e * 7;
                    next[x] += e * 3;
                    next[x + 1] += e * 5;
                    next[x + 2] += e;
                }
                std::mem::swap(&mut cur, &mut next);
                next.iter_mut().for_each(|v| *v = 0);
            }
            (out, stride, 1)
        }
    }
}

/// The compression shared by PWG and URF: repeated lines + per-line pixel runs.
pub fn compress(data: &[u8], stride: usize, unit: usize, out: &mut Vec<u8>) {
    let rows: Vec<&[u8]> = data.chunks_exact(stride).collect();
    let mut y = 0;
    while y < rows.len() {
        let mut rep = 1;
        while rep < 256 && y + rep < rows.len() && rows[y + rep] == rows[y] {
            rep += 1;
        }
        out.push((rep - 1) as u8);
        let px: Vec<&[u8]> = rows[y].chunks_exact(unit).collect();
        let mut x = 0;
        while x < px.len() {
            let mut run = 1;
            while run < 128 && x + run < px.len() && px[x + run] == px[x] {
                run += 1;
            }
            if run >= 2 || x + 1 == px.len() {
                out.push((run - 1) as u8);
                out.extend_from_slice(px[x]);
                x += run;
            } else {
                let mut lit = 1;
                while lit < 128 && x + lit < px.len()
                    && !(x + lit + 1 < px.len() && px[x + lit] == px[x + lit + 1]) {
                    lit += 1;
                }
                if lit == 1 {
                    out.push(0);
                    out.extend_from_slice(px[x]);
                } else {
                    out.push((257 - lit) as u8);
                    for p in &px[x..x + lit] { out.extend_from_slice(p); }
                }
                x += lit;
            }
        }
        y += rep;
    }
}

pub fn write_start(w: &mut impl Write, format: Format, pages: u32) -> io::Result<()> {
    match format {
        Format::Pwg => w.write_all(b"RaS2"),
        Format::Urf => {
            w.write_all(b"UNIRAST\0")?;
            w.write_all(&pages.to_be_bytes())
        }
    }
}

fn put_str(h: &mut [u8], off: usize, s: &str) { h[off..off + s.len()].copy_from_slice(s.as_bytes()); }
fn put_u32(h: &mut [u8], off: usize, v: u32) { h[off..off + 4].copy_from_slice(&v.to_be_bytes()); }

pub fn write_page(w: &mut impl Write, s: &Spec, img: &RgbImage, back: bool, total: u32) -> io::Result<()> {
    let (data, stride, unit) = to_pixels(img, s.kind);
    let bpp: u32 = match s.kind { Kind::Srgb8 => 24, Kind::Gray8 => 8, Kind::Black1 => 1 };
    match s.format {
        Format::Pwg => {
            let mut h = vec![0u8; 1796];
            let (cs, colors, bpc) = match s.kind {
                Kind::Srgb8 => (19, 3, 8),
                Kind::Gray8 => (18, 1, 8),
                Kind::Black1 => (3, 1, 1),
            };
            put_str(&mut h, 0, "PwgRaster");
            let duplex = s.sides != Sides::One;
            put_u32(&mut h, 272, duplex as u32);
            put_u32(&mut h, 276, s.xdpi);
            put_u32(&mut h, 280, s.ydpi);
            put_u32(&mut h, 340, 1);
            put_u32(&mut h, 352, (s.width as u64 * 72 / s.xdpi as u64) as u32);
            put_u32(&mut h, 356, (s.height as u64 * 72 / s.ydpi as u64) as u32);
            put_u32(&mut h, 368, (s.sides == Sides::Short) as u32);
            put_u32(&mut h, 372, s.width);
            put_u32(&mut h, 376, s.height);
            put_u32(&mut h, 384, bpc);
            put_u32(&mut h, 388, bpp);
            put_u32(&mut h, 392, stride as u32);
            put_u32(&mut h, 400, cs);
            put_u32(&mut h, 420, colors);
            let (mx, my) = if back { s.back_flip() } else { (false, false) };
            put_u32(&mut h, 452, total);
            put_u32(&mut h, 456, if mx { u32::MAX } else { 1 });
            put_u32(&mut h, 460, if my { u32::MAX } else { 1 });
            put_u32(&mut h, 472, s.width);  // ImageBoxRight
            put_u32(&mut h, 476, s.height); // ImageBoxBottom
            put_u32(&mut h, 480, 0xFF_FFFF); // AlternatePrimary (white)
            put_str(&mut h, 1732, &s.media[..s.media.len().min(63)]);
            w.write_all(&h)?;
        }
        Format::Urf => {
            let mut h = [0u8; 32];
            h[0] = bpp as u8;
            h[1] = if s.kind == Kind::Srgb8 { 1 } else { 0 };
            h[2] = match s.sides { Sides::One => 1, Sides::Short => 2, Sides::Long => 3 };
            put_u32(&mut h, 12, s.width);
            put_u32(&mut h, 16, s.height);
            put_u32(&mut h, 20, s.xdpi);
            w.write_all(&h)?;
        }
    }
    let mut buf = Vec::with_capacity(data.len() / 8);
    compress(&data, stride, unit, &mut buf);
    w.write_all(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decoder written straight from the spec, to round-trip the compressor.
    fn decompress(mut c: &[u8], stride: usize, unit: usize, rows: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < rows * stride {
            let rep = c[0] as usize + 1;
            c = &c[1..];
            let mut line = Vec::new();
            while line.len() < stride {
                let b = c[0] as usize;
                c = &c[1..];
                assert_ne!(b, 128);
                if b < 128 {
                    for _ in 0..=b { line.extend_from_slice(&c[..unit]); }
                    c = &c[unit..];
                } else {
                    let n = (257 - b) * unit;
                    line.extend_from_slice(&c[..n]);
                    c = &c[n..];
                }
            }
            assert_eq!(line.len(), stride);
            for _ in 0..rep { out.extend_from_slice(&line); }
        }
        assert!(c.is_empty());
        out
    }

    fn rng(seed: &mut u32) -> u32 { *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); *seed >> 16 }

    #[test]
    fn compression_round_trips() {
        let mut seed = 7;
        for (unit, w, h, spread) in [(1, 1, 1, 2), (1, 300, 5, 2), (3, 1, 3, 2), (3, 299, 4, 3), (3, 129, 300, 2), (1, 700, 3, 256), (3, 500, 2, 256)] {
            for mode in 0..3 {
                let stride = w * unit;
                let mut d = vec![0u8; stride * h];
                for (i, b) in d.iter_mut().enumerate() {
                    *b = match mode {
                        0 => (rng(&mut seed) % spread) as u8,
                        1 => 200,
                        _ => ((i / stride) % 2 * 255) as u8,
                    };
                }
                let mut c = vec![];
                compress(&d, stride, unit, &mut c);
                assert_eq!(decompress(&c, stride, unit, h), d, "unit={unit} w={w} h={h} mode={mode}");
            }
        }
    }

    #[test]
    fn back_side_transforms_match_cups() {
        let t = |back: &str, sides| Spec { format: Format::Pwg, kind: Kind::Gray8, xdpi: 300, ydpi: 300, width: 1, height: 1, media: "x".into(), sides, back: back.into() }.back_flip();
        assert_eq!(t("normal", Sides::Long), (false, false));
        assert_eq!(t("flipped", Sides::Long), (false, true));
        assert_eq!(t("flipped", Sides::Short), (true, false));
        assert_eq!(t("rotated", Sides::Long), (true, true));
        assert_eq!(t("rotated", Sides::Short), (false, false));
        assert_eq!(t("manual-tumble", Sides::Long), (false, false));
        assert_eq!(t("manual-tumble", Sides::Short), (true, true));
    }

    #[test]
    fn black1_is_black_where_dark() {
        let img = RgbImage::from_fn(16, 2, |x, _| if x < 8 { [0, 0, 0].into() } else { [255, 255, 255].into() });
        let (d, stride, _) = to_pixels(&img, Kind::Black1);
        assert_eq!(stride, 2);
        assert_eq!(&d[..2], &[0xFF, 0x00]);
    }
}
