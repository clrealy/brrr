//! Anything → pages of pixels. PDFs go through poppler or ghostscript, the rest is on us.
use crate::raster::{self, Sides, Spec};
use image::{imageops, ImageDecoder, ImageReader, RgbImage};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind { Pdf, Png, Jpeg, Text, PostScript, Office }

pub fn sniff(path: &Path) -> Result<Kind, String> {
    let mut head = vec![0u8; 8192];
    let mut f = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let n = f.read(&mut head).map_err(|e| e.to_string())?;
    let head = &head[..n];
    if n == 0 {
        return Err(format!("{} is empty, nothing to print", path.display()));
    }
    if head.windows(5).take(1024).any(|w| w == b"%PDF-") { return Ok(Kind::Pdf); }
    if head.starts_with(b"\x89PNG") { return Ok(Kind::Png); }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) { return Ok(Kind::Jpeg); }
    if head.starts_with(b"%!") || head.starts_with(b"\x04%!") { return Ok(Kind::PostScript); }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let office_ext = matches!(ext.as_str(), "docx" | "doc" | "odt" | "rtf" | "pptx" | "ppt" | "odp" | "xlsx" | "xls" | "ods");
    let office_magic = head.starts_with(b"PK\x03\x04") || head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) || head.starts_with(b"{\\rtf");
    if office_ext && office_magic { return Ok(Kind::Office); }
    if !head.contains(&0) && !office_magic { return Ok(Kind::Text); }
    Err(format!("brrr doesn't know how to print {} yet. export it to PDF, PNG, JPEG or text", path.display()))
}

/// Word docs, sheets and slides go through LibreOffice.
pub fn office_to_pdf(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let bin = ["soffice", "libreoffice"].into_iter().find(|t| has(t))
        .ok_or("printing office files takes LibreOffice. install it and retry, or export to PDF")?;
    let profile = dir.join("lo-profile");
    let out = dir.join("office");
    let st = Command::new(bin)
        .arg(format!("-env:UserInstallation=file://{}", profile.display()))
        .args(["--headless", "--norestore", "--convert-to", "pdf", "--outdir"]).arg(&out).arg(src)
        .output().map_err(|e| e.to_string())?;
    let pdf = out.join(Path::new(src.file_stem().unwrap_or_default()).with_extension("pdf"));
    if !st.status.success() || !pdf.exists() {
        return Err("LibreOffice couldn't convert that file".into());
    }
    Ok(pdf)
}

pub fn ps_to_pdf(src: &Path, out: &Path) -> Result<(), String> {
    if !has("gs") { return Err("printing PostScript takes ghostscript. install it and retry".into()); }
    let st = Command::new("gs").args(["-q", "-dNOPAUSE", "-dBATCH", "-dSAFER", "-sDEVICE=pdfwrite"])
        .arg(format!("-sOutputFile={}", out.display())).arg(src).output().map_err(|e| e.to_string())?;
    if st.status.success() { Ok(()) } else { Err("ghostscript couldn't read that PostScript".into()) }
}

/// Wraps text by display width (CJK is double wide) into pages of `rows` lines.
fn paginate(text: &str, cols: usize, rows: usize) -> Vec<Vec<String>> {
    use unicode_width::UnicodeWidthChar;
    let mut pages: Vec<Vec<String>> = vec![vec![]];
    let push = |pages: &mut Vec<Vec<String>>, l: String| {
        if pages.last().unwrap().len() >= rows { pages.push(vec![]); }
        pages.last_mut().unwrap().push(l);
    };
    for chunk in text.split_inclusive('\x0c') {
        for line in chunk.trim_end_matches('\x0c').lines() {
            let (mut cur, mut w) = (String::new(), 0usize);
            for c in line.trim_end_matches('\r').chars() {
                if c == '\t' {
                    let pad = 8 - w % 8;
                    cur.extend(std::iter::repeat_n(' ', pad));
                    w += pad;
                    continue;
                }
                if c.is_control() { continue; }
                let cw = c.width().unwrap_or(0);
                if w + cw > cols {
                    push(&mut pages, std::mem::take(&mut cur));
                    w = 0;
                }
                cur.push(c);
                w += cw;
            }
            push(&mut pages, cur);
        }
        if chunk.ends_with('\x0c') { pages.push(vec![]); }
    }
    if pages.len() > 1 && pages.last().is_some_and(|p| p.is_empty()) { pages.pop(); }
    pages
}

/// A page's height in points, via poppler since cairo compresses its page objects.
fn pdf_height(pdf: &Path) -> Option<f64> {
    let o = Command::new("pdfinfo").arg(pdf).output().ok()?;
    let txt = String::from_utf8_lossy(&o.stdout).into_owned();
    let line = txt.lines().find(|l| l.starts_with("Page size:"))?;
    line.split_whitespace().nth(4)?.parse().ok()
}

/// Any script, any emoji: split into pages ourselves, let pango draw them.
fn unicode_text_to_pdf(text: &str, out: &Path, pts: (f64, f64), cols: usize, mut rows: usize) -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut files = vec![];
    // Fallback fonts make lines taller than we can predict, so measure and repaginate.
    for attempt in 0..3 {
        files.clear();
        let mut tallest = 0f64;
        let pages = paginate(text, cols, rows);
        for (i, page) in pages.iter().enumerate() {
            let txt = tmp.path().join(format!("{i:06}.txt"));
            let pdf = tmp.path().join(format!("{i:06}.pdf"));
            // Pad every page to full height so they all come out the same size.
            let mut body = page.join("\n");
            body.push_str(&"\n".repeat(rows.saturating_sub(page.len())));
            std::fs::write(&txt, body).map_err(|e| e.to_string())?;
            let st = Command::new("pango-view").args(["-q", "--backend=cairo", "--dpi=72", "--font=monospace 10", "--wrap=char", "--margin=36"])
                .arg(format!("--width={}", (pts.0 - 72.0) as i32)).arg(format!("--output={}", pdf.display())).arg(&txt)
                .output().map_err(|e| e.to_string())?;
            if !st.status.success() { return Err("pango couldn't lay out that text".into()); }
            tallest = tallest.max(pdf_height(&pdf).unwrap_or(0.0));
            files.push(pdf);
        }
        if tallest <= pts.1 + 1.0 || attempt == 2 || rows <= 5 { break; }
        rows = (((rows as f64) * (pts.1 - 72.0) / (tallest - 72.0)).floor() as usize).max(5);
    }
    if files.len() == 1 {
        std::fs::copy(&files[0], out).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let st = Command::new("pdfunite").args(&files).arg(out).output().map_err(|e| e.to_string())?;
    if st.status.success() { Ok(()) } else { Err("couldn't stitch the text pages together".into()) }
}

pub fn has(tool: &str) -> bool {
    Command::new(tool).arg("-v").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

/// Plain text → a minimal PDF in Courier. pts is (width, height) of the paper.
pub fn text_to_pdf(src: &Path, out: &Path, pts: (f64, f64)) -> Result<(), String> {
    let raw = std::fs::read(src).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let cols = (((pts.0 - 72.0) / 6.0) as usize).max(10);
    let rows = (((pts.1 - 72.0) / 12.0) as usize).max(5);
    if text.chars().any(|c| c as u32 > 255) {
        if has("pango-view") && has("pdfunite") {
            return unicode_text_to_pdf(&text, out, pts, cols, rows);
        }
        eprintln!("brrr: install pango and poppler to print non-latin text, using ? for now");
    }
    let mut pages: Vec<Vec<Vec<u8>>> = vec![vec![]];
    for chunk in text.split_inclusive('\x0c') {
        let ff = chunk.ends_with('\x0c');
        for line in chunk.trim_end_matches('\x0c').lines() {
            let mut cur: Vec<u8> = vec![];
            for c in line.trim_end_matches('\r').chars() {
                if c == '\t' {
                    let pad = 8 - cur.len() % 8;
                    cur.extend(std::iter::repeat_n(b' ', pad));
                } else {
                    cur.push(if (c as u32) < 256 { c as u32 as u8 } else { b'?' });
                }
                while cur.len() > cols {
                    let rest = cur.split_off(cols);
                    push_line(&mut pages, std::mem::replace(&mut cur, rest), rows);
                }
            }
            push_line(&mut pages, cur, rows);
        }
        if ff { pages.push(vec![]); }
    }
    if pages.len() > 1 && pages.last().is_some_and(|p| p.is_empty()) { pages.pop(); }

    let mut pdf: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offs: Vec<usize> = vec![];
    let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    let mut obj = |pdf: &mut Vec<u8>, body: &[u8]| {
        offs.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", offs.len()).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };
    obj(&mut pdf, b"<< /Type /Catalog /Pages 2 0 R >>");
    obj(&mut pdf, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), pages.len()).as_bytes());
    obj(&mut pdf, b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>");
    for (i, page) in pages.iter().enumerate() {
        let contents = 5 + i * 2;
        obj(&mut pdf, format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.2} {:.2}] /Resources << /Font << /F1 3 0 R >> >> /Contents {contents} 0 R >>",
            pts.0, pts.1).as_bytes());
        let mut s: Vec<u8> = format!("BT /F1 10 Tf 12 TL 36 {:.2} Td\n", pts.1 - 46.0).into_bytes();
        for line in page {
            s.push(b'(');
            for &b in line {
                if matches!(b, b'(' | b')' | b'\\') { s.push(b'\\'); }
                s.push(b);
            }
            s.extend_from_slice(b") Tj T*\n");
        }
        s.extend_from_slice(b"ET");
        let mut body = format!("<< /Length {} >>\nstream\n", s.len()).into_bytes();
        body.extend_from_slice(&s);
        body.extend_from_slice(b"\nendstream");
        obj(&mut pdf, &body);
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offs.len() + 1).as_bytes());
    for o in &offs { pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes()); }
    pdf.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", offs.len() + 1).as_bytes());
    std::fs::write(out, pdf).map_err(|e| e.to_string())
}

fn push_line(pages: &mut Vec<Vec<Vec<u8>>>, line: Vec<u8>, rows: usize) {
    if pages.last().unwrap().len() >= rows { pages.push(vec![]); }
    pages.last_mut().unwrap().push(line);
}

pub type Ranges = Vec<(u32, Option<u32>)>;

fn render_pdf(src: &Path, dir: &Path, s: &Spec, ranges: &Ranges) -> Result<Vec<PathBuf>, String> {
    let whole: Ranges = vec![(1, None)];
    let ranges = if ranges.is_empty() { &whole } else { ranges };
    let poppler = has("pdftoppm");
    if !poppler && !has("gs") {
        return Err("this printer needs raster, and that takes poppler or ghostscript. install poppler (pdftoppm) and retry".into());
    }
    let mut out = vec![];
    for (i, (a, b)) in ranges.iter().enumerate() {
        let sub = dir.join(format!("r{i}"));
        std::fs::create_dir(&sub).map_err(|e| e.to_string())?;
        let st = if poppler {
            let mut c = Command::new("pdftoppm");
            c.args(["-rx", &s.xdpi.to_string(), "-ry", &s.ydpi.to_string(), "-f", &a.to_string()]);
            if let Some(b) = b { c.args(["-l", &b.to_string()]); }
            c.arg(src).arg(sub.join("p"));
            c.output()
        } else {
            let mut c = Command::new("gs");
            c.args(["-q", "-dNOPAUSE", "-dBATCH", "-dSAFER", "-sDEVICE=ppmraw", "-dTextAlphaBits=4", "-dGraphicsAlphaBits=4"]);
            c.arg(format!("-r{}x{}", s.xdpi, s.ydpi)).arg(format!("-dFirstPage={a}"));
            if let Some(b) = b { c.arg(format!("-dLastPage={b}")); }
            c.arg(format!("-sOutputFile={}", sub.join("p-%06d.ppm").display())).arg(src);
            c.output()
        }.map_err(|e| e.to_string())?;
        if !st.status.success() {
            return Err(format!("couldn't render that PDF: {}", String::from_utf8_lossy(&st.stderr).lines().next().unwrap_or("unknown error")));
        }
        let mut pages: Vec<PathBuf> = std::fs::read_dir(&sub).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
        pages.sort();
        out.extend(pages);
    }
    if out.is_empty() { return Err("those pages don't exist in this PDF".into()); }
    Ok(out)
}

fn load_photo(p: &Path) -> Result<RgbImage, String> {
    let e = |e: image::ImageError| format!("couldn't read {}: {e}", p.display());
    let mut dec = ImageReader::open(p).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?.into_decoder().map_err(e)?;
    let o = dec.orientation().map_err(e)?;
    let mut img = image::DynamicImage::from_decoder(dec).map_err(e)?;
    img.apply_orientation(o);
    // Transparent PNGs get composited on white paper, not black.
    let rgba = img.into_rgba8();
    Ok(RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let p = rgba.get_pixel(x, y).0;
        let a = p[3] as u32;
        let m = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        [m(p[0]), m(p[1]), m(p[2])].into()
    }))
}

/// Center img on a white sheet, rotated and scaled to fit inside the margins.
pub fn on_sheet(mut img: RgbImage, w: u32, h: u32, m: (u32, u32, u32, u32)) -> RgbImage {
    let (l, t, r, b) = m;
    let (bw, bh) = (w.saturating_sub(l + r).max(1), h.saturating_sub(t + b).max(1));
    if (img.width() > img.height()) != (bw > bh) && img.width() != img.height() {
        img = imageops::rotate90(&img);
    }
    let s = (bw as f64 / img.width() as f64).min(bh as f64 / img.height() as f64);
    let (nw, nh) = (((img.width() as f64 * s) as u32).clamp(1, bw), ((img.height() as f64 * s) as u32).clamp(1, bh));
    if (nw, nh) != img.dimensions() {
        img = imageops::resize(&img, nw, nh, imageops::FilterType::CatmullRom);
    }
    let mut sheet = RgbImage::from_pixel(w, h, [255, 255, 255].into());
    imageops::replace(&mut sheet, &img, (l + (bw - nw) / 2) as i64, (t + (bh - nh) / 2) as i64);
    sheet
}

/// Writes the finished raster stream and returns how many sheets-sides it holds.
pub fn rasterize(src: &Path, kind: Kind, s: &Spec, ranges: &Ranges, repeat: u32, margins: (u32, u32, u32, u32), out: &Path) -> Result<u32, String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let is_doc = kind == Kind::Pdf;
    let pages = if is_doc { render_pdf(src, tmp.path(), s, ranges)? } else { vec![src.to_path_buf()] };
    // Each copy of an odd-length duplex job needs a blank back so copies don't share a sheet.
    let pad = s.sides != Sides::One && pages.len() % 2 == 1 && repeat > 1;
    let per_copy = pages.len() as u32 + pad as u32;
    let total = per_copy * repeat;
    let io = |e: std::io::Error| e.to_string();
    let mut w = BufWriter::new(File::create(out).map_err(io)?);
    raster::write_start(&mut w, s.format, total).map_err(io)?;
    let mut n = 0u32;
    for _ in 0..repeat {
        for i in 0..per_copy as usize {
            let mut sheet = match pages.get(i) {
                Some(p) if is_doc => {
                    let img = image::open(p).map_err(|e| format!("bad render: {e}"))?.into_rgb8();
                    on_sheet(img, s.width, s.height, (0, 0, 0, 0))
                }
                Some(p) => on_sheet(load_photo(p)?, s.width, s.height, margins),
                None => RgbImage::from_pixel(s.width, s.height, [255, 255, 255].into()),
            };
            let back = n % 2 == 1 && s.sides != Sides::One;
            if back {
                let (mx, my) = s.back_flip();
                if mx { imageops::flip_horizontal_in_place(&mut sheet); }
                if my { imageops::flip_vertical_in_place(&mut sheet); }
            }
            raster::write_page(&mut w, s, &sheet, back, total).map_err(io)?;
            n += 1;
        }
    }
    w.flush().map_err(io)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_image_rotates_onto_portrait_sheet() {
        let img = RgbImage::from_pixel(400, 200, [0, 0, 0].into());
        let s = on_sheet(img, 100, 200, (0, 0, 0, 0));
        assert_eq!(s.dimensions(), (100, 200));
        assert_eq!(s.get_pixel(50, 100).0, [0, 0, 0]);
    }

    #[test]
    fn margins_stay_white() {
        let img = RgbImage::from_pixel(10, 10, [0, 0, 0].into());
        let s = on_sheet(img, 100, 100, (20, 20, 20, 20));
        assert_eq!(s.get_pixel(5, 50).0, [255, 255, 255]);
        assert_eq!(s.get_pixel(50, 5).0, [255, 255, 255]);
        assert_eq!(s.get_pixel(50, 50).0, [0, 0, 0]);
    }
}
