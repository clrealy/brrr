//! brrr: printing on linux, minus the suffering.
mod human;
mod ipp;
mod mdns;
mod raster;
mod render;
mod serve;
mod tls;

use ipp::{Group, Msg, Uri, Value};
use mdns::Printer;
use raster::{Format, Kind as RKind, Sides, Spec};
use render::Kind;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const HELP: &str = "brrr - printing without the suffering

  brrr FILE...             print it. pdf, docx, png, jpg, txt, ps, or - for stdin
  brrr list                what printers are around
  brrr status [PRINTER]    state, ink, what's wrong
  brrr jobs                what's in the queue
  brrr cancel JOB          kill a job
  brrr default PRINTER     pick a default
  brrr doctor              figure out why it's not working
  brrr forget PRINTER      trust a printer's new security certificate
  brrr serve --install     make Ctrl+P in apps print through brrr
  brrr serve --uninstall   put apps back on CUPS

options:
  -t, --to NAME|URI        which printer (part of its name, or ipp://...)
  -n, --copies N           how many
  -2, --duplex             both sides
      --short-edge         both sides, flipped like a notepad
      --one-sided          force single sided
      --gray / --color     force mono or color
  -s, --size NAME          letter, legal, a4, a5, 4x6, or a full PWG name
  -p, --pages 1-3,7,10-    only these pages
      --raster             don't trust the printer's PDF engine
      --no-wait            submit and bounce

Also answers as `lp` or `lpr` if you symlink it.";

#[derive(Default, Clone)]
pub(crate) struct Opts {
    to: Option<String>,
    copies: u32,
    sides: Option<Sides>,
    color: Option<bool>,
    size: Option<String>,
    pages: render::Ranges,
    raster: bool,
    no_wait: bool,
    legacy: bool,
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("brrr: {msg}");
    std::process::exit(1);
}

fn parse_pages(s: &str) -> Result<render::Ranges, String> {
    let err = || format!("'{s}' isn't a page list, try something like 1-3,7,10-");
    s.split(',').map(|p| {
        let p = p.trim();
        let (a, b) = match p.split_once('-') {
            Some((a, "")) => (a.parse().map_err(|_| err())?, None),
            Some((a, b)) => (a.parse().map_err(|_| err())?, Some(b.parse().map_err(|_| err())?)),
            None => { let n: u32 = p.parse().map_err(|_| err())?; (n, Some(n)) }
        };
        if a == 0 || b.is_some_and(|b| b < a) { return Err(err()); }
        Ok((a, b))
    }).collect()
}

pub(crate) fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    Some(config_home()?.join("brrr"))
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("default"))
}

pub(crate) fn saved_default() -> Option<String> {
    if let Ok(p) = std::env::var("BRRR_PRINTER") { if !p.is_empty() { return Some(p); } }
    let s = std::fs::read_to_string(config_path()?).ok()?;
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

fn find() -> Vec<Printer> { mdns::discover(Duration::from_millis(1500)) }

fn matching<'a>(all: &'a [Printer], want: &str) -> Result<&'a Printer, String> {
    let w = want.to_lowercase();
    let hits: Vec<&Printer> = match all.iter().find(|p| p.name.to_lowercase() == w) {
        Some(p) => vec![p],
        None => all.iter().filter(|p| p.name.to_lowercase().contains(&w)).collect(),
    };
    match hits.as_slice() {
        [] => Err(format!("no printer matching '{want}'. run `brrr list` to see what's around")),
        [p] => Ok(p),
        many => Err(format!("'{want}' matches {}: {}", many.len(), many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

fn by_name(all: &[Printer], want: &str) -> Result<(String, Uri), String> {
    usable(matching(all, want)?)
}

pub(crate) fn usable(p: &Printer) -> Result<(String, Uri), String> {
    let uri = p.uri.clone().ok_or_else(|| format!("{} showed up but didn't finish answering, try again", p.name))?;
    // Fall back only for printers whose TLS never worked. Once a cert is pinned, no downgrades.
    if let (true, Some(plain)) = (uri.tls, &p.plain) {
        if !tls::pinned(&uri) {
            if let Err(e) = ipp::get_attrs(&uri, &["printer-state"]) {
                eprintln!("brrr: couldn't encrypt the connection to {} ({e}), using plain IPP", p.name);
                return Ok((p.name.clone(), plain.clone()));
            }
        }
    }
    Ok((p.name.clone(), uri))
}

fn pick(to: Option<&str>) -> Result<(String, Uri), String> {
    if let Some(t) = to {
        if t.contains("://") { let u = Uri::parse(t)?; return Ok((u.host.clone(), u)); }
    }
    let saved = saved_default();
    if to.is_none() {
        if let Some(s) = saved.as_deref().filter(|s| s.contains("://")) {
            let u = Uri::parse(s)?;
            return Ok((u.host.clone(), u));
        }
    }
    let all = find();
    if let Some(t) = to { return by_name(&all, t); }
    if let Some(s) = saved {
        match by_name(&all, &s) {
            Ok(x) => return Ok(x),
            Err(_) => eprintln!("brrr: your default '{s}' isn't around, looking for others"),
        }
    }
    match all.as_slice() {
        [] => Err("no printers found. is it on, and on this network? try `brrr doctor`".into()),
        [p] => usable(p),
        many => {
            if !std::io::stdin().is_terminal() {
                return Err(format!("more than one printer around, pick with --to: {}", many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")));
            }
            for (i, p) in many.iter().enumerate() { eprintln!("  {}) {}", i + 1, p.name); }
            eprint!("which one? ");
            std::io::stderr().flush().ok();
            let mut l = String::new();
            std::io::stdin().read_line(&mut l).ok();
            let n: usize = l.trim().parse().map_err(|_| "that's not a number".to_string())?;
            usable(many.get(n.wrapping_sub(1)).ok_or("that's not on the list")?)
        }
    }
}

/// Paper size in hundredths of mm from a self-describing PWG name.
pub(crate) fn media_size(name: &str) -> Option<(u32, u32)> {
    let dims = name.rsplit('_').next()?;
    let (nums, unit) = if let Some(n) = dims.strip_suffix("mm") { (n, 100.0) }
        else if let Some(n) = dims.strip_suffix("in") { (n, 2540.0) } else { return None };
    let (a, b) = nums.split_once('x')?;
    let a: f64 = a.parse().ok()?;
    let b: f64 = b.parse().ok()?;
    if a <= 0.0 || b <= 0.0 { return None; }
    Some(((a * unit).round() as u32, (b * unit).round() as u32))
}

fn choose_media(a: &Group, want: Option<&str>) -> Result<(String, (u32, u32)), String> {
    let mut supported = ipp::texts(a, "media-supported");
    supported.retain(|m| !m.contains("custom_m"));
    let name = match want {
        Some(w) => {
            let alias = match w.to_lowercase().as_str() {
                "letter" => "na_letter_".to_string(),
                "legal" => "na_legal_".to_string(),
                "a4" => "iso_a4_".to_string(),
                "a5" => "iso_a5_".to_string(),
                "4x6" => "4x6in".to_string(),
                "5x7" => "5x7in".to_string(),
                o => o.to_string(),
            };
            supported.iter().find(|m| m.contains(&alias)).cloned().or_else(|| {
                media_size(w).map(|_| w.to_string())
            }).ok_or_else(|| format!("this printer doesn't do '{w}' paper. it does: {}", supported.join(", ")))?
        }
        None => ipp::text(a, "media-default")
            .filter(|m| media_size(m).is_some())
            .or_else(|| ipp::texts(a, "media-ready").into_iter().find(|m| media_size(m).is_some()))
            .or_else(|| supported.iter().find(|m| media_size(m).is_some()).cloned())
            .ok_or("printer won't say what paper it has, pass --size")?,
    };
    let size = media_size(&name).ok_or_else(|| format!("can't work out how big '{name}' is"))?;
    Ok((name, size))
}

fn resolutions(a: &Group, key: &str) -> Vec<(u32, u32)> {
    a.get(key).map(|v| v.iter().filter_map(|x| match x {
        Value::Res(x, y, 3) if *x > 0 && *y > 0 => Some((*x as u32, *y as u32)),
        Value::Res(x, y, 4) if *x > 0 && *y > 0 => Some(((*x as f64 * 2.54).round() as u32, (*y as f64 * 2.54).round() as u32)),
        _ => None,
    }).collect()).unwrap_or_default()
}

fn nearest_300(list: &[(u32, u32)]) -> Option<(u32, u32)> {
    list.iter().copied().min_by_key(|(x, y)| ((*x as i64 - 300).abs() + (*y as i64 - 300).abs(), -(*x as i64)))
}

fn urf_dpis(tags: &[String]) -> Vec<(u32, u32)> {
    tags.iter().filter_map(|t| t.strip_prefix("RS"))
        .flat_map(|s| s.split('-').filter_map(|n| n.parse::<u32>().ok()).map(|d| (d, d)).collect::<Vec<_>>()).collect()
}

fn urf_back(tags: &[String]) -> &'static str {
    for t in tags {
        match t.as_str() { "DM2" => return "flipped", "DM3" => return "rotated", "DM4" => return "manual-tumble", _ => {} }
    }
    "normal"
}

/// Which raster dialect, depth and DPI to speak to this printer.
fn raster_spec(a: &Group, color: bool, media: (&str, (u32, u32)), sides: Sides) -> Result<Spec, String> {
    let formats = ipp::texts(a, "document-format-supported");
    let has = |f: &str| formats.iter().any(|x| x.eq_ignore_ascii_case(f));
    let (format, kind, dpi, back);
    let pwg = ipp::texts(a, "pwg-raster-document-type-supported");
    let urf = ipp::texts(a, "urf-supported");
    let pick_kind = |c: bool, s: bool, g: bool, b: bool| -> Option<RKind> {
        let order: &[(bool, RKind)] = if color { &[(c, RKind::Srgb8), (g, RKind::Gray8), (b, RKind::Black1)] }
            else { &[(g, RKind::Gray8), (b, RKind::Black1), (s, RKind::Srgb8)] };
        order.iter().find(|x| x.0).map(|x| x.1)
    };
    if has("image/pwg-raster") && !pwg.is_empty() {
        let t = |n: &str| pwg.iter().any(|x| x == n);
        format = Format::Pwg;
        kind = pick_kind(t("srgb_8"), t("srgb_8"), t("sgray_8"), t("black_1")).ok_or("printer's raster types are all weird ones brrr can't make")?;
        dpi = nearest_300(&resolutions(a, "pwg-raster-document-resolution-supported")).ok_or("printer didn't say what resolution it wants")?;
        back = ipp::text(a, "pwg-raster-document-sheet-back").unwrap_or_else(|| "normal".into());
    } else if has("image/urf") && !urf.is_empty() {
        let t = |n: &str| urf.iter().any(|x| x.eq_ignore_ascii_case(n));
        format = Format::Urf;
        kind = pick_kind(t("SRGB24"), t("SRGB24"), t("W8"), false).ok_or("printer's URF modes are all weird ones brrr can't make")?;
        dpi = nearest_300(&urf_dpis(&urf)).ok_or("printer didn't say what resolution it wants")?;
        back = urf_back(&urf).to_string();
    } else {
        return Err(format!("this printer doesn't take any format brrr can make. it wants: {}", formats.join(", ")));
    }
    let width = (media.1.0 as u64 * dpi.0 as u64 / 2540) as u32;
    let height = (media.1.1 as u64 * dpi.1 as u64 / 2540) as u32;
    Ok(Spec { format, kind, xdpi: dpi.0, ydpi: dpi.1, width, height, media: media.0.to_string(), sides, back })
}

fn margin(a: &Group, k: &str, dpi: u32) -> u32 {
    let hmm = ipp::ints(a, k).into_iter().max().unwrap_or(0).max(0) as u64;
    (hmm * dpi as u64 / 2540) as u32
}

fn sides_word(s: Sides) -> &'static str {
    match s { Sides::One => "one-sided", Sides::Long => "two-sided-long-edge", Sides::Short => "two-sided-short-edge" }
}

fn print_one(path: &Path, label: &str, o: &Opts) -> Result<(), String> {
    render::sniff(path)?;
    let (pname, uri) = pick(o.to.as_deref())?;
    print_to(path, label, o, &pname, &uri, &mut |_| {})
}

pub(crate) fn print_to(path: &Path, label: &str, o: &Opts, pname: &str, uri: &Uri, on_job: &mut dyn FnMut(i32)) -> Result<(), String> {
    let kind = render::sniff(path)?;
    let a = ipp::get_attrs(uri, &["all", "media-col-database"])?;

    if ipp::boolean(&a, "printer-is-accepting-jobs") == Some(false) {
        return Err(format!("{pname} isn't accepting jobs right now"));
    }
    if ipp::int(&a, "printer-state") == Some(5) {
        let why: Vec<_> = ipp::texts(&a, "printer-state-reasons").iter().filter_map(|r| human::reason(r)).collect();
        if !why.is_empty() { eprintln!("brrr: heads up, {pname} says: {}", why.join(", ")); }
    }

    let (media, size) = choose_media(&a, o.size.as_deref())?;
    let sides_ok = ipp::texts(&a, "sides-supported");
    let mut sides = match o.sides {
        Some(s) => s,
        None => match ipp::text(&a, "sides-default").as_deref() {
            Some("two-sided-long-edge") => Sides::Long,
            Some("two-sided-short-edge") => Sides::Short,
            _ => Sides::One,
        },
    };
    if sides != Sides::One && !sides_ok.iter().any(|s| s == sides_word(sides)) {
        eprintln!("brrr: this printer can't do both sides, going one-sided");
        sides = Sides::One;
    }
    let color_capable = ipp::boolean(&a, "color-supported").unwrap_or(false);
    let default_mode = ipp::text(&a, "print-color-mode-default").unwrap_or_default();
    if o.color == Some(true) && !color_capable { eprintln!("brrr: this printer is black and white only"); }
    let color = o.color.unwrap_or(default_mode != "monochrome") && color_capable;
    let color_modes = ipp::texts(&a, "print-color-mode-supported");

    // Text gets turned into a PDF first, then it's just another PDF.
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let (src, kind) = match kind {
        Kind::Text => {
            let p = tmp.path().join("text.pdf");
            render::text_to_pdf(path, &p, (size.0 as f64 * 72.0 / 2540.0, size.1 as f64 * 72.0 / 2540.0))?;
            (p, Kind::Pdf)
        }
        Kind::PostScript => {
            let p = tmp.path().join("ps.pdf");
            render::ps_to_pdf(path, &p)?;
            (p, Kind::Pdf)
        }
        Kind::Office => {
            eprintln!("brrr: converting {label} with LibreOffice...");
            (render::office_to_pdf(path, tmp.path())?, Kind::Pdf)
        }
        _ => (path.to_path_buf(), kind),
    };

    let formats = ipp::texts(&a, "document-format-supported");
    let accepts = |f: &str| formats.iter().any(|x| x.eq_ignore_ascii_case(f));
    let ranges_ok = ipp::boolean(&a, "page-ranges-supported").unwrap_or(false);
    let direct = !o.raster && match kind {
        Kind::Pdf => accepts("application/pdf") && (o.pages.is_empty() || ranges_ok),
        Kind::Png => accepts("image/png"),
        Kind::Jpeg => accepts("image/jpeg"),
        _ => false,
    };
    let copies_ok = o.copies == 1 || matches!(a.get("copies-supported").and_then(|v| v.first()), Some(Value::Range(_, hi)) if *hi >= o.copies as i32);

    let submit = |doc: &Path, mime: &str, send_ranges: bool, send_copies: bool| -> Result<ipp::Resp, String> {
        let build = |v: (u8, u8)| {
            let mut m = Msg::standard(ipp::PRINT_JOB, v, uri);
            m.s(ipp::NAME, "job-name", label);
            m.s(ipp::MIME, "document-format", mime);
            m.group(2);
            if send_copies && o.copies > 1 { m.i(ipp::INTEGER, "copies", o.copies as i32); }
            if sides_ok.iter().any(|s| s == sides_word(sides)) { m.s(ipp::KEYWORD, "sides", sides_word(sides)); }
            let cm = if color { "color" } else { "monochrome" };
            if color_modes.iter().any(|c| c == cm) { m.s(ipp::KEYWORD, "print-color-mode", cm); }
            if ipp::texts(&a, "media-supported").iter().any(|x| *x == media) { m.s(ipp::KEYWORD, "media", &media); }
            if send_ranges {
                for (i, (lo, hi)) in o.pages.iter().enumerate() {
                    m.range(if i == 0 { "page-ranges" } else { "" }, *lo as i32, hi.map(|h| h as i32).unwrap_or(i32::MAX));
                }
            }
            m.finish()
        };
        // Printers love saying "busy". Give them a minute before giving up.
        for attempt in 0..12 {
            let r = ipp::request(uri, &build, Some(doc))?;
            if !matches!(r.status, 0x0502 | 0x0507) { return Ok(r); }
            if attempt == 0 { eprintln!("brrr: {pname} is busy, waiting..."); }
            std::thread::sleep(Duration::from_secs(5));
        }
        Err(format!("{pname} stayed busy for a whole minute, try again later"))
    };

    let send_raster = || -> Result<ipp::Resp, String> {
        let spec = raster_spec(&a, color, (&media, size), sides)?;
        let m = (
            margin(&a, "media-left-margin-supported", spec.xdpi),
            margin(&a, "media-top-margin-supported", spec.ydpi),
            margin(&a, "media-right-margin-supported", spec.xdpi),
            margin(&a, "media-bottom-margin-supported", spec.ydpi),
        );
        let repeat = if copies_ok { 1 } else { o.copies };
        let out = tmp.path().join("job.raster");
        eprintln!("brrr: converting for {pname} ({:?} {:?}, {}x{} dpi)...", spec.format, spec.kind, spec.xdpi, spec.ydpi);
        let n = render::rasterize(&src, kind, &spec, &o.pages, repeat, m, &out)?;
        eprintln!("brrr: sending {n} page{} to {pname}", if n == 1 { "" } else { "s" });
        let mime = if spec.format == Format::Pwg { "image/pwg-raster" } else { "image/urf" };
        submit(&out, mime, false, copies_ok)
    };

    let mut jobs = vec![];
    if direct {
        let mime = match kind { Kind::Png => "image/png", Kind::Jpeg => "image/jpeg", _ => "application/pdf" };
        eprintln!("brrr: sending {label} to {pname}");
        let rounds = if copies_ok { 1 } else { o.copies };
        for _ in 0..rounds {
            let mut r = submit(&src, mime, !o.pages.is_empty(), copies_ok)?;
            if matches!(r.status, 0x040A | 0x040B) {
                eprintln!("brrr: {pname} choked on that, converting it instead");
                r = send_raster()?;
                jobs.push(check(r)?);
                break;
            }
            jobs.push(check(r)?);
        }
    } else {
        jobs.push(check(send_raster()?)?);
    }

    for id in jobs {
        on_job(id);
        if o.no_wait || o.legacy { eprintln!("brrr: job {id} is in"); continue; }
        watch(uri, pname, id)?;
    }
    Ok(())
}

fn check(r: ipp::Resp) -> Result<i32, String> {
    if !r.ok() {
        let mut msg = human::status(r.status);
        if let Some(d) = ipp::text(&r.op, "status-message") { msg = format!("{msg} ({d})"); }
        return Err(msg);
    }
    if !r.unsupported.is_empty() {
        let k: Vec<_> = r.unsupported.keys().cloned().collect();
        eprintln!("brrr: printer ignored: {}", k.join(", "));
    }
    r.job.first().and_then(|j| ipp::int(j, "job-id")).ok_or_else(|| "printer took it but never gave a job number".into())
}

/// Follow a job until the paper's out or something goes wrong.
fn watch(uri: &Uri, pname: &str, id: i32) -> Result<(), String> {
    let mut last = String::new();
    let mut failures = 0;
    loop {
        let r = ipp::request(uri, &|v| {
            let mut m = Msg::standard(ipp::GET_JOB_ATTRS, v, uri);
            m.i(ipp::INTEGER, "job-id", id);
            m.s(ipp::KEYWORD, "requested-attributes", "job-state");
            m.s(ipp::KEYWORD, "", "job-state-reasons");
            m.finish()
        }, None);
        let r = match r {
            Ok(r) if r.ok() => r,
            Ok(r) if r.status == 0x0406 || r.status == 0x0407 => { eprintln!("brrr: job {id} finished"); return Ok(()); }
            Ok(_) | Err(_) => {
                failures += 1;
                if failures > 4 { eprintln!("brrr: lost track of job {id}, but it was sent. check the printer"); return Ok(()); }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        failures = 0;
        let j = r.job.first().cloned().unwrap_or_default();
        let st = ipp::int(&j, "job-state").unwrap_or(0);
        match st {
            9 => { eprintln!("brrr: done, go grab it"); return Ok(()); }
            7 => return Err(format!("job {id} got canceled")),
            8 => {
                let why: Vec<_> = ipp::texts(&j, "job-state-reasons").iter().filter(|r| *r != "job-printing").filter_map(|r| human::reason(r)).collect();
                return Err(format!("job {id} failed{}", if why.is_empty() { String::new() } else { format!(": {}", why.join(", ")) }));
            }
            _ => {}
        }
        let p = ipp::get_attrs(uri, &["printer-state-reasons"]).unwrap_or_default();
        let problems: Vec<String> = ipp::texts(&p, "printer-state-reasons").iter().filter(|r| human::is_blocking(r)).filter_map(|r| human::reason(r)).collect();
        let line = if problems.is_empty() { human::job_state(st).to_string() } else { format!("{pname}: {}", problems.join(", ")) };
        if line != last { eprintln!("brrr: {line}"); last = line; }
        std::thread::sleep(Duration::from_secs(2));
    }
}

const STATUS_ATTRS: &[&str] = &["printer-state", "printer-state-reasons", "marker-names", "marker-levels", "printer-make-and-model"];

fn cmd_list() -> Result<(), String> {
    let all = find();
    if all.is_empty() { return Err("no printers found. try `brrr doctor`".into()); }
    let default = saved_default().unwrap_or_default().to_lowercase();
    for p in &all {
        let star = if !default.is_empty() && p.name.to_lowercase().contains(&default) { "*" } else { " " };
        let info = match &p.uri {
            Some(u) => ipp::get_attrs(u, STATUS_ATTRS).map(|a| human::state(&a)).unwrap_or_else(|e| e),
            None => "found it, but it didn't finish answering".to_string(),
        };
        println!("{star} {}  [{}]  {info}", p.name, p.via);
    }
    Ok(())
}

fn cmd_status(to: Option<&str>) -> Result<(), String> {
    let (name, uri) = pick(to)?;
    let a = ipp::get_attrs(&uri, STATUS_ATTRS)?;
    println!("{name}");
    if let Some(m) = ipp::text(&a, "printer-make-and-model") { println!("  model   {m}"); }
    println!("  address {}", uri.to_ipp());
    println!("  state   {}", human::state(&a));
    let ink = human::ink(&a);
    if !ink.is_empty() { println!("  ink     {}", ink.join("  ")); }
    Ok(())
}

fn cmd_jobs(to: Option<&str>) -> Result<(), String> {
    let (name, uri) = pick(to)?;
    let r = ipp::request(&uri, &|v| {
        let mut m = Msg::standard(ipp::GET_JOBS, v, &uri);
        m.s(ipp::KEYWORD, "which-jobs", "not-completed");
        m.s(ipp::KEYWORD, "requested-attributes", "job-id");
        m.s(ipp::KEYWORD, "", "job-state");
        m.s(ipp::KEYWORD, "", "job-name");
        m.finish()
    }, None)?;
    if !r.ok() { return Err(human::status(r.status)); }
    if r.job.is_empty() { println!("{name}: queue's empty"); }
    for j in &r.job {
        println!("{}  {}  {}", ipp::int(j, "job-id").unwrap_or(0), human::job_state(ipp::int(j, "job-state").unwrap_or(0)), ipp::text(j, "job-name").unwrap_or_default());
    }
    Ok(())
}

fn cmd_cancel(id: i32, to: Option<&str>) -> Result<(), String> {
    let (_, uri) = pick(to)?;
    let r = ipp::request(&uri, &|v| {
        let mut m = Msg::standard(ipp::CANCEL_JOB, v, &uri);
        m.i(ipp::INTEGER, "job-id", id);
        m.finish()
    }, None)?;
    if !r.ok() { return Err(human::status(r.status)); }
    println!("job {id} is gone");
    Ok(())
}

fn cmd_default(name: &str) -> Result<(), String> {
    let (real, uri) = pick(Some(name))?;
    let p = config_path().ok_or("can't find your config folder")?;
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    // A typed-in URI is saved as-is, discovered printers by name so a new DHCP lease can't break it.
    let value = if name.contains("://") { uri.to_ipp() } else { real.clone() };
    std::fs::write(&p, value).map_err(|e| e.to_string())?;
    println!("default is now {real}");
    Ok(())
}

fn cmd_forget(name: &str) -> Result<(), String> {
    let id = if name.contains("://") { Uri::parse(name)?.id } else {
        let all = find();
        matching(&all, name)?.uri.as_ref().ok_or("that printer didn't finish answering")?.id.clone()
    };
    if tls::forget(&id) { println!("forgot it. brrr will trust whatever certificate it has next time"); }
    else { println!("brrr hadn't saved a certificate for that printer"); }
    Ok(())
}

fn cmd_doctor() -> Result<(), String> {
    let mark = |ok: bool| if ok { "ok " } else { "!! " };
    let pdf = render::has("pdftoppm");
    let gs = render::has("gs");
    println!("{}office files: {}", mark(render::has("soffice") || render::has("libreoffice")), if render::has("soffice") || render::has("libreoffice") { "LibreOffice" } else { "install LibreOffice to print .docx and friends" });
    println!("{}non-latin text: {}", mark(render::has("pango-view") && render::has("pdfunite")), if render::has("pango-view") && render::has("pdfunite") { "pango" } else { "install pango and poppler" });
    println!("{}PDF renderer: {}", mark(pdf || gs), if pdf { "poppler" } else if gs { "ghostscript" } else { "none. printers without native PDF won't work, install poppler" });
    let ifs = if_addrs::get_if_addrs().unwrap_or_default();
    let up: Vec<_> = ifs.iter().filter(|i| !i.is_loopback() && i.ip().is_ipv4()).map(|i| format!("{} {}", i.name, i.ip())).collect();
    println!("{}network: {}", mark(!up.is_empty()), if up.is_empty() { "no IPv4 network up".to_string() } else { up.join(", ") });
    let all = mdns::discover(Duration::from_millis(3000));
    println!("{}printers found: {}", mark(!all.is_empty()), all.len());
    if all.is_empty() {
        println!("   checklist:");
        println!("   - printer on the same wifi as you, not its guest network");
        println!("   - firewall allows UDP 5353 replies (mDNS) and TCP 631");
        println!("   - USB printer? install and start ipp-usb");
        println!("   - know its IP? try: brrr --to ipp://IP/ipp/print FILE");
    }
    for p in &all {
        println!("\n{}:", p.name);
        let Some(u) = &p.uri else { println!("!! it showed up but didn't finish answering"); continue; };
        println!("   address {}", u.to_ipp());
        match ipp::get_attrs(u, &["all"]) {
            Err(e) => println!("!! {e}"),
            Ok(a) => {
                println!("ok talks IPP, state: {}", human::state(&a));
                let f = ipp::texts(&a, "document-format-supported");
                let can = f.iter().any(|x| matches!(x.to_lowercase().as_str(), "application/pdf" | "image/pwg-raster" | "image/urf"));
                println!("{}formats: {}", mark(can), f.join(", "));
                match choose_media(&a, None) { Ok((m, _)) => println!("ok paper: {m}"), Err(e) => println!("!! {e}") }
            }
        }
    }
    Ok(())
}

fn spool_stdin() -> Result<tempfile::NamedTempFile, String> {
    let mut t = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let mut buf = vec![];
    std::io::stdin().read_to_end(&mut buf).map_err(|e| e.to_string())?;
    t.write_all(&buf).map_err(|e| e.to_string())?;
    Ok(t)
}

fn lp_option(o: &mut Opts, kv: &str) {
    match kv.split_once('=') {
        Some(("sides", "two-sided-long-edge")) => o.sides = Some(Sides::Long),
        Some(("sides", "two-sided-short-edge")) => o.sides = Some(Sides::Short),
        Some(("sides", "one-sided")) => o.sides = Some(Sides::One),
        Some(("media", m)) => o.size = Some(m.to_string()),
        Some(("page-ranges", p)) => { o.pages = parse_pages(p).unwrap_or_else(|e| die(e)); }
        _ => eprintln!("brrr: ignoring option {kv}"),
    }
}

fn parse_args(args: Vec<String>, legacy: bool) -> (Opts, Vec<String>) {
    let mut o = Opts { copies: 1, legacy, ..Default::default() };
    let mut files = vec![];
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let need = |it: &mut std::vec::IntoIter<String>, f: &str| it.next().unwrap_or_else(|| die(format!("{f} needs a value")));
        let copies = |s: &str| s.parse::<u32>().ok().filter(|n| *n >= 1).unwrap_or_else(|| die(format!("'{s}' isn't a copy count")));
        match a.as_str() {
            "-h" | "--help" => { println!("{HELP}"); std::process::exit(0); }
            "-V" | "--version" => { println!("brrr 0.2.0"); std::process::exit(0); }
            "-t" | "--to" => o.to = Some(need(&mut it, &a)),
            "-d" | "-P" if legacy => o.to = Some(need(&mut it, &a)),
            "-n" | "--copies" => { let v = need(&mut it, &a); o.copies = copies(&v); }
            "-o" if legacy => { let v = need(&mut it, &a); lp_option(&mut o, &v); }
            "-2" | "--duplex" => o.sides = Some(Sides::Long),
            "--short-edge" => o.sides = Some(Sides::Short),
            "--one-sided" => o.sides = Some(Sides::One),
            "--gray" | "--grey" | "--mono" => o.color = Some(false),
            "--color" | "--colour" => o.color = Some(true),
            "-s" | "--size" => o.size = Some(need(&mut it, &a)),
            "-p" | "--pages" => { let v = need(&mut it, &a); o.pages = parse_pages(&v).unwrap_or_else(|e| die(e)); }
            "--raster" => o.raster = true,
            "--no-wait" => o.no_wait = true,
            s if legacy && s.starts_with("-d") && s.len() > 2 => o.to = Some(s[2..].into()),
            s if legacy && s.starts_with("-P") && s.len() > 2 => o.to = Some(s[2..].into()),
            s if legacy && s.starts_with("-#") => o.copies = copies(&s[2..]),
            s if legacy && s.starts_with("-n") && s.len() > 2 => o.copies = copies(&s[2..]),
            "-" => files.push(a),
            s if s.starts_with('-') => die(format!("no idea what {s} means. try --help")),
            _ => files.push(a),
        }
    }
    (o, files)
}

fn main() {
    // `brrr list | head` closing the pipe isn't a crash.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.to_string().contains("Broken pipe") { std::process::exit(0); }
        default_hook(info);
    }));
    let mut args: Vec<String> = std::env::args().collect();
    let argv0 = Path::new(&args.remove(0)).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let legacy = argv0 == "lp" || argv0 == "lpr";

    if args.is_empty() && !legacy { println!("{HELP}"); return; }
    if !legacy && args[0] == "serve" {
        if let Err(e) = serve::main(&args[1..]) { die(e); }
        return;
    }

    let (o, files) = parse_args(args, legacy);
    let to = o.to.clone();
    let cmd = files.first().filter(|f| !legacy && !Path::new(f.as_str()).exists()).map(|s| s.as_str());
    let result = match cmd {
        Some("list" | "ls") => cmd_list(),
        Some("status") => cmd_status(files.get(1).map(|s| s.as_str()).or(to.as_deref())),
        Some("jobs") => cmd_jobs(to.as_deref()),
        Some("doctor") => cmd_doctor(),
        Some("cancel") => match files.get(1).and_then(|s| s.parse().ok()) {
            Some(id) => cmd_cancel(id, to.as_deref()),
            None => Err("cancel which job? try `brrr jobs`".into()),
        },
        Some("default") => match files.get(1) {
            Some(n) => cmd_default(n),
            None => Err("default to what? try `brrr list`".into()),
        },
        Some("forget") => match files.get(1) {
            Some(n) => cmd_forget(n),
            None => Err("forget which printer?".into()),
        },
        Some("help") => { println!("{HELP}"); Ok(()) }
        _ => {
            let mut list = files.clone();
            if list.is_empty() { if legacy { list.push("-".into()); } else { die("print what?"); } }
            let mut res = Ok(());
            for f in list {
                let run = if f == "-" {
                    spool_stdin().and_then(|t| print_one(t.path(), "stdin", &o))
                } else {
                    let name = Path::new(&f).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| f.clone());
                    print_one(Path::new(&f), &name, &o)
                };
                if let Err(e) = run { eprintln!("brrr: {f}: {e}"); res = Err(String::new()); }
            }
            res
        }
    };
    if let Err(e) = result {
        if !e.is_empty() { eprintln!("brrr: {e}"); }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_names() {
        assert_eq!(media_size("na_letter_8.5x11in"), Some((21590, 27940)));
        assert_eq!(media_size("iso_a4_210x297mm"), Some((21000, 29700)));
        assert_eq!(media_size("custom_min_1x1in"), Some((2540, 2540)));
        assert_eq!(media_size("garbage"), None);
    }

    #[test]
    fn page_lists() {
        assert_eq!(parse_pages("1-3,7,10-").unwrap(), vec![(1, Some(3)), (7, Some(7)), (10, None)]);
        assert!(parse_pages("0").is_err());
        assert!(parse_pages("5-2").is_err());
        assert!(parse_pages("abc").is_err());
    }

    #[test]
    fn picks_closest_to_300() {
        assert_eq!(nearest_300(&[(150, 150), (300, 300), (600, 600)]), Some((300, 300)));
        assert_eq!(nearest_300(&[(203, 203), (600, 600)]), Some((203, 203)));
        let urf: Vec<String> = ["V1.4", "RS300-600", "W8", "DM3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(urf_dpis(&urf), vec![(300, 300), (600, 600)]);
        assert_eq!(urf_back(&urf), "rotated");
    }
}
