//! `brrr serve`: answers on localhost in CUPS's dialect so GTK, Qt and anything
//! else built on libcups sends Ctrl+P jobs here. No cupsd required.
use crate::ipp::{self, Group, Msg, Value, ENUM, INTEGER, KEYWORD, MIME, NAME, TEXT, URI};
use crate::raster::Sides;
use crate::Opts;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CUPS_GET_DEFAULT: u16 = 0x4001;
const CUPS_GET_PRINTERS: u16 = 0x4002;
const CUPS_GET_CLASSES: u16 = 0x4005;
const CUPS_GET_PPD: u16 = 0x400F;
const VALIDATE_JOB: u16 = 0x0004;
const CREATE_JOB: u16 = 0x0005;
const SEND_DOCUMENT: u16 = 0x0006;

const DEFAULT_PORT: u16 = 6631;
const HEAD_LIMIT: usize = 1 << 20;

#[derive(Clone)]
struct Queue {
    name: String,
    display: String,
    uri: ipp::Uri,
    seen: Instant,
}

struct Job {
    id: i32,
    queue: String,
    name: String,
    user: String,
    state: i32,
    reason: String,
    created: i64,
    finished: i64,
    opts: Opts,
    docs: Vec<PathBuf>,
    remote: Option<(ipp::Uri, i32)>,
    _dir: Option<tempfile::TempDir>,
}

struct Server {
    port: u16,
    queues: Mutex<Vec<Queue>>,
    jobs: Mutex<HashMap<i32, Job>>,
    next: AtomicI32,
    cache: Mutex<HashMap<String, (Instant, Option<Group>)>>,
    started: Instant,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// CUPS queue names can't have spaces or slashes.
pub fn queue_name(display: &str) -> String {
    let s: String = display.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "printer".into() } else { s }
}

fn refresh(s: &Arc<Server>) {
    let found = crate::mdns::discover(Duration::from_millis(1500));
    let known: Vec<String> = s.queues.lock().unwrap().iter().map(|q| q.display.clone()).collect();
    let mut fresh = vec![];
    for p in &found {
        // Probing can touch the network, so do it once per new printer and outside the lock.
        if known.contains(&p.name) { continue; }
        if let Ok((display, uri)) = crate::usable(p) { fresh.push((display, uri)); }
    }
    let mut q = s.queues.lock().unwrap();
    for p in &found {
        if let Some(e) = q.iter_mut().find(|e| e.display == p.name) {
            e.seen = Instant::now();
            // DHCP moved it: keep following, with the same security level.
            if let Some(u) = p.uri.iter().chain(p.plain.iter()).find(|u| u.tls == e.uri.tls) { e.uri = u.clone(); }
        }
    }
    for (display, uri) in fresh {
        let mut name = queue_name(&display);
        let mut n = 2;
        while q.iter().any(|e| e.name == name) { name = format!("{}_{n}", queue_name(&display)); n += 1; }
        eprintln!("brrr serve: found {display} ({})", uri.to_ipp());
        q.push(Queue { name, display, uri, seen: Instant::now() });
    }
    // Printers nap and wifi blips, so don't drop one the moment it goes quiet.
    q.retain(|e| e.seen.elapsed() < Duration::from_secs(300));
}

fn attrs(s: &Server, q: &Queue) -> Option<Group> {
    if let Some((t, g)) = s.cache.lock().unwrap().get(&q.name) {
        if t.elapsed() < Duration::from_secs(15) { return g.clone(); }
    }
    let g = ipp::get_attrs(&q.uri, &["all"]).ok();
    s.cache.lock().unwrap().insert(q.name.clone(), (Instant::now(), g.clone()));
    g
}

fn find_queue(s: &Server, op: &Group) -> Option<Queue> {
    let u = ipp::text(op, "printer-uri")?;
    let name = u.rsplit('/').next()?;
    let name = percent_decode(name);
    s.queues.lock().unwrap().iter().find(|q| q.name == name).cloned()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 1) && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) { out.push(v); i += 3; continue; }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn default_queue(s: &Server) -> Option<Queue> {
    let q = s.queues.lock().unwrap();
    if let Some(d) = crate::saved_default() {
        let d = d.to_lowercase();
        if let Some(p) = q.iter().find(|p| p.display.to_lowercase().contains(&d)) { return Some(p.clone()); }
    }
    q.first().cloned()
}

fn strs(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

fn nice_media(m: &str) -> String {
    let (w, _) = crate::media_size(m).unwrap_or((0, 0));
    match m.split('_').nth(1) {
        Some("letter") => "US Letter".into(),
        Some("legal") => "US Legal".into(),
        Some("a4") => "A4".into(),
        Some("a5") => "A5".into(),
        Some("a3") => "A3".into(),
        Some(o) if w > 0 && m.ends_with("in") && o.contains('x') => format!("{} in", o.trim_end_matches("in")),
        _ => m.split('_').next_back().unwrap_or(m).to_string(),
    }
}

fn medias(a: &Option<Group>) -> Vec<String> {
    let mut m: Vec<String> = a.as_ref().map(|g| ipp::texts(g, "media-supported")).unwrap_or_default()
        .into_iter().filter(|x| !x.contains("custom_m") && crate::media_size(x).is_some()).collect();
    if m.is_empty() { m = strs(&["na_letter_8.5x11in", "iso_a4_210x297mm"]); }
    m
}

fn default_media(a: &Option<Group>) -> String {
    a.as_ref().and_then(|g| ipp::text(g, "media-default")).filter(|m| crate::media_size(m).is_some())
        .unwrap_or_else(|| medias(a)[0].clone())
}

fn color(a: &Option<Group>) -> bool {
    a.as_ref().and_then(|g| ipp::boolean(g, "color-supported")).unwrap_or(false)
}

fn sides(a: &Option<Group>) -> Vec<String> {
    let v = a.as_ref().map(|g| ipp::texts(g, "sides-supported")).unwrap_or_default();
    if v.is_empty() { strs(&["one-sided"]) } else { v }
}

fn queue_uri(s: &Server, name: &str) -> String { format!("ipp://localhost:{}/printers/{name}", s.port) }

fn printer_group(m: &mut Msg, s: &Server, q: &Queue) {
    let a = attrs(s, q);
    let duplex = sides(&a).iter().any(|x| x != "one-sided");
    let col = color(&a);
    let model = a.as_ref().and_then(|g| ipp::text(g, "printer-make-and-model")).unwrap_or_else(|| q.display.clone());
    let state = match &a { Some(g) => ipp::int(g, "printer-state").unwrap_or(3), None => 5 };
    let mut reasons = a.as_ref().map(|g| ipp::texts(g, "printer-state-reasons")).unwrap_or_else(|| strs(&["offline-report"]));
    if reasons.is_empty() { reasons = strs(&["none"]); }
    let accepting = a.as_ref().and_then(|g| ipp::boolean(g, "printer-is-accepting-jobs")).unwrap_or(true);

    m.many(URI, "printer-uri-supported", &[queue_uri(s, &q.name)]);
    m.s(KEYWORD, "uri-authentication-supported", "none");
    m.s(KEYWORD, "uri-security-supported", "none");
    m.s(NAME, "printer-name", &q.name);
    m.s(TEXT, "printer-info", &q.display);
    m.s(TEXT, "printer-location", "");
    m.s(TEXT, "printer-make-and-model", &model);
    m.s(URI, "device-uri", &q.uri.to_ipp());
    m.s(URI, "printer-more-info", &format!("http://localhost:{}/", s.port));
    m.i(ENUM, "printer-state", state);
    m.many(KEYWORD, "printer-state-reasons", &reasons);
    m.s(TEXT, "printer-state-message", "");
    m.i(INTEGER, "printer-state-change-time", now() as i32);
    m.boolean("printer-is-accepting-jobs", accepting);
    m.boolean("printer-is-shared", false);
    m.i(INTEGER, "printer-up-time", s.started.elapsed().as_secs().max(1) as i32);
    let queued = s.jobs.lock().unwrap().values().filter(|j| j.queue == q.name && j.state < 7).count();
    m.i(INTEGER, "queued-job-count", queued as i32);
    // CUPS capability bits: copies + collate, color or b&w, duplex, all paper sizes.
    let ty = 0x40 | 0x80 | if col { 0x8 } else { 0x4 } | if duplex { 0x10 } else { 0 } | 0x1000 | 0x2000;
    m.i(ENUM, "printer-type", ty);
    m.s(ipp::CHARSET, "charset-configured", "utf-8");
    m.s(ipp::CHARSET, "charset-supported", "utf-8");
    m.s(ipp::LANGUAGE, "natural-language-configured", "en");
    m.s(ipp::LANGUAGE, "generated-natural-language-supported", "en");
    m.many(KEYWORD, "ipp-versions-supported", &strs(&["1.1", "2.0"]));
    for (i, op) in [2, 4, 5, 6, 8, 9, 10, 11].iter().enumerate() {
        m.i(ENUM, if i == 0 { "operations-supported" } else { "" }, *op);
    }
    m.s(KEYWORD, "pdl-override-supported", "not-attempted");
    m.s(KEYWORD, "compression-supported", "none");
    m.s(MIME, "document-format-default", "application/octet-stream");
    m.many(MIME, "document-format-supported", &strs(&[
        "application/octet-stream", "application/pdf", "application/postscript", "application/vnd.cups-pdf",
        "application/vnd.cups-postscript", "image/jpeg", "image/png", "text/plain",
    ]));
    m.many(KEYWORD, "media-supported", &medias(&a));
    m.s(KEYWORD, "media-default", &default_media(&a));
    m.many(KEYWORD, "media-ready", &[default_media(&a)]);
    m.many(KEYWORD, "sides-supported", &sides(&a));
    m.s(KEYWORD, "sides-default", "one-sided");
    m.i(INTEGER, "copies-default", 1);
    m.range("copies-supported", 1, 999);
    m.boolean("page-ranges-supported", true);
    m.boolean("color-supported", col);
    m.many(KEYWORD, "print-color-mode-supported", &if col { strs(&["color", "monochrome"]) } else { strs(&["monochrome"]) });
    m.s(KEYWORD, "print-color-mode-default", if col { "color" } else { "monochrome" });
    m.i(INTEGER, "number-up-default", 1);
    m.i(INTEGER, "number-up-supported", 1);
    m.many(NAME, "job-sheets-default", &strs(&["none", "none"]));
    m.s(NAME, "job-sheets-supported", "none");
    m.i(ENUM, "finishings-default", 3);
    m.i(ENUM, "finishings-supported", 3);
    m.res("printer-resolution-default", 300);
    m.res("printer-resolution-supported", 300);
    m.s(KEYWORD, "multiple-document-handling-supported", "separate-documents-collated-copies");
}

fn ppd_escape(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii() && !matches!(c, '"' | ':' | '/' | '\n' | '\r')).collect()
}

pub fn ppd(q_display: &str, a: &Option<Group>) -> String {
    let name = ppd_escape(q_display);
    let col = color(a);
    let def = default_media(a);
    let m: Vec<(String, f64, f64)> = medias(a).into_iter().filter(|x| x.len() <= 40).map(|x| {
        let (w, h) = crate::media_size(&x).unwrap();
        (x, w as f64 * 72.0 / 2540.0, h as f64 * 72.0 / 2540.0)
    }).collect();
    let mut p = format!("*PPD-Adobe: \"4.3\"\n*FormatVersion: \"4.3\"\n*FileVersion: \"1.0\"\n*LanguageVersion: English\n*LanguageEncoding: ISOLatin1\n*PCFileName: \"brrr.ppd\"\n*Manufacturer: \"brrr\"\n*Product: \"(brrr)\"\n*ModelName: \"{name}\"\n*ShortNickName: \"{short}\"\n*NickName: \"{name}, brrr\"\n*PSVersion: \"(3010.000) 0\"\n*LanguageLevel: \"3\"\n*ColorDevice: {cd}\n*DefaultColorSpace: {cs}\n*FileSystem: False\n*Throughput: \"1\"\n*LandscapeOrientation: Plus90\n*TTRasterizer: Type42\n*cupsVersion: 2.4\n*cupsFilter2: \"application/pdf application/octet-stream 0 -\"\n",
        short = name.chars().take(31).collect::<String>(), cd = if col { "True" } else { "False" }, cs = if col { "RGB" } else { "Gray" });
    let setsize = |p: &mut String, opt: &str, title: &str, order: &str| {
        p.push_str(&format!("*OpenUI *{opt}/{title}: PickOne\n*OrderDependency: 10 {order} *{opt}\n*Default{opt}: {def}\n"));
        for (k, w, h) in &m {
            p.push_str(&format!("*{opt} {k}/{}: \"<</PageSize[{w:.2} {h:.2}]>>setpagedevice\"\n", nice_media(k)));
        }
        p.push_str(&format!("*CloseUI: *{opt}\n"));
    };
    setsize(&mut p, "PageSize", "Paper Size", "AnySetup");
    setsize(&mut p, "PageRegion", "Paper Region", "AnySetup");
    p.push_str(&format!("*DefaultImageableArea: {def}\n"));
    for (k, w, h) in &m { p.push_str(&format!("*ImageableArea {k}: \"14.17 14.17 {:.2} {:.2}\"\n", w - 14.17, h - 14.17)); }
    p.push_str(&format!("*DefaultPaperDimension: {def}\n"));
    for (k, w, h) in &m { p.push_str(&format!("*PaperDimension {k}: \"{w:.2} {h:.2}\"\n")); }
    if sides(a).iter().any(|x| x != "one-sided") {
        p.push_str("*OpenUI *Duplex/2-Sided: PickOne\n*OrderDependency: 20 AnySetup *Duplex\n*DefaultDuplex: None\n*Duplex None/Off: \"<</Duplex false>>setpagedevice\"\n*Duplex DuplexNoTumble/Long Edge: \"<</Duplex true/Tumble false>>setpagedevice\"\n*Duplex DuplexTumble/Short Edge: \"<</Duplex true/Tumble true>>setpagedevice\"\n*CloseUI: *Duplex\n");
    }
    if col {
        p.push_str("*OpenUI *ColorModel/Color: PickOne\n*OrderDependency: 20 AnySetup *ColorModel\n*DefaultColorModel: RGB\n*ColorModel RGB/Color: \"<</cupsColorSpace 1>>setpagedevice\"\n*ColorModel Gray/Black and White: \"<</cupsColorSpace 0>>setpagedevice\"\n*CloseUI: *ColorModel\n");
    }
    p.push_str("*DefaultFont: Courier\n*Font Courier: Standard \"(1.05)\" Standard ROM\n");
    p
}

/// Turns the settings an app sent into brrr's own print options.
fn job_opts(job: &Group, op: &Group) -> Opts {
    let mut o = Opts { copies: 1, ..Default::default() };
    let get = |k: &str| ipp::text(job, k).or_else(|| ipp::text(op, k));
    if let Some(n) = ipp::int(job, "copies").or_else(|| ipp::int(op, "copies")) { o.copies = n.clamp(1, 999) as u32; }
    match get("sides").or_else(|| get("Duplex")).as_deref() {
        Some("two-sided-long-edge" | "DuplexNoTumble") => o.sides = Some(Sides::Long),
        Some("two-sided-short-edge" | "DuplexTumble") => o.sides = Some(Sides::Short),
        Some("one-sided" | "None") => o.sides = Some(Sides::One),
        _ => {}
    }
    o.size = get("media").or_else(|| get("PageSize"));
    match get("print-color-mode").or_else(|| get("ColorModel")).as_deref() {
        Some("color" | "RGB") => o.color = Some(true),
        Some("monochrome" | "Gray" | "bi-level") => o.color = Some(false),
        _ => {}
    }
    if let Some(v) = job.get("page-ranges").or_else(|| op.get("page-ranges")) {
        o.pages = v.iter().filter_map(|x| match x {
            Value::Range(a, b) if *a >= 1 => Some((*a as u32, if *b == i32::MAX { None } else { Some(*b as u32) })),
            _ => None,
        }).collect();
    }
    o
}

fn job_group(m: &mut Msg, s: &Server, j: &Job) {
    m.i(INTEGER, "job-id", j.id);
    m.s(URI, "job-uri", &format!("ipp://localhost:{}/jobs/{}", s.port, j.id));
    m.s(URI, "job-printer-uri", &queue_uri(s, &j.queue));
    m.s(NAME, "job-name", &j.name);
    m.s(NAME, "job-originating-user-name", &j.user);
    m.i(ENUM, "job-state", j.state);
    m.s(KEYWORD, "job-state-reasons", &j.reason);
    m.i(INTEGER, "time-at-creation", j.created as i32);
    m.i(INTEGER, "time-at-processing", j.created as i32);
    m.i(INTEGER, "time-at-completed", if j.finished > 0 { j.finished } else { j.created } as i32);
    m.i(INTEGER, "job-k-octets", 0);
}

fn set_job(s: &Server, id: i32, state: i32, reason: &str) {
    if let Some(j) = s.jobs.lock().unwrap().get_mut(&id) {
        // A cancel the user asked for shouldn't get overwritten by the worker finishing.
        if j.state != 7 { j.state = state; j.reason = reason.to_string(); }
        if j.state >= 7 { j.finished = now(); }
    }
}

fn start_worker(s: &Arc<Server>, id: i32, q: Queue) {
    let s = s.clone();
    std::thread::spawn(move || {
        let (docs, opts, name) = {
            let mut jobs = s.jobs.lock().unwrap();
            let Some(j) = jobs.get_mut(&id) else { return };
            j.state = 5;
            j.reason = "job-printing".into();
            (j.docs.clone(), j.opts.clone(), j.name.clone())
        };
        let mut failed = None;
        for d in &docs {
            let sref = s.clone();
            let uri = q.uri.clone();
            let mut on_job = move |rid: i32| {
                if let Some(j) = sref.jobs.lock().unwrap().get_mut(&id) { j.remote = Some((uri.clone(), rid)); }
            };
            if let Err(e) = crate::print_to(d, &name, &opts, &q.display, &q.uri, &mut on_job) {
                eprintln!("brrr serve: job {id} failed: {e}");
                failed = Some(e);
                break;
            }
        }
        match failed {
            Some(_) => set_job(&s, id, 8, "job-aborted-by-system"),
            None => set_job(&s, id, 9, "job-completed-successfully"),
        }
        let mut jobs = s.jobs.lock().unwrap();
        if let Some(j) = jobs.get_mut(&id) { j._dir = None; j.docs.clear(); }
        // Keep a little history for apps polling status, not forever.
        if jobs.len() > 200 {
            let mut done: Vec<(i64, i32)> = jobs.values().filter(|j| j.state >= 7).map(|j| (j.created, j.id)).collect();
            done.sort();
            for (_, old) in done.into_iter().take(100) { jobs.remove(&old); }
        }
    });
}

/// Drop the IPP attributes off the front, leaving just the document on disk.
fn spool(dir: &Path, body: &Body, off: usize, n: usize) -> std::io::Result<PathBuf> {
    let p = dir.join(format!("doc{n}"));
    let mut f = std::fs::File::create(&p)?;
    f.write_all(&body.head[off.min(body.head.len())..])?;
    if let Some(t) = &body.tail {
        std::io::copy(&mut std::fs::File::open(t.path())?, &mut f)?;
    }
    Ok(p)
}

fn new_job(s: &Server, q: &Queue, op: &Group, job: &Group) -> std::io::Result<i32> {
    let id = s.next.fetch_add(1, Ordering::SeqCst);
    if std::env::var_os("BRRR_DEBUG").is_some() {
        eprintln!("brrr serve: job {id} options: {job:?}");
    }
    let j = Job {
        id, queue: q.name.clone(),
        name: ipp::text(op, "job-name").filter(|n| !n.is_empty()).unwrap_or_else(|| "untitled".into()),
        user: ipp::text(op, "requesting-user-name").unwrap_or_else(ipp::user),
        state: 3, reason: "none".into(), created: now(), finished: 0,
        opts: job_opts(job, op), docs: vec![], remote: None,
        _dir: Some(tempfile::tempdir()?),
    };
    s.jobs.lock().unwrap().insert(id, j);
    Ok(id)
}

fn job_id(op: &Group) -> Option<i32> {
    ipp::int(op, "job-id").or_else(|| ipp::text(op, "job-uri")?.rsplit('/').next()?.parse().ok())
}

fn handle_ipp(s: &Arc<Server>, body: &Body) -> Vec<u8> {
    let b = &body.head;
    let version = (*b.first().unwrap_or(&2), *b.get(1).unwrap_or(&0));
    let id = if b.len() >= 8 { u32::from_be_bytes([b[4], b[5], b[6], b[7]]) } else { 0 };
    let op_code = if b.len() >= 4 { u16::from_be_bytes([b[2], b[3]]) } else { 0 };
    let head = |status: u16| {
        let mut m = Msg::new(status, version, id);
        m.group(1);
        m.s(ipp::CHARSET, "attributes-charset", "utf-8");
        m.s(ipp::LANGUAGE, "attributes-natural-language", "en");
        m
    };
    let Ok((req, off)) = ipp::parse_full(b) else { return head(0x0400).finish(); };
    let op = &req.op;
    let job_attrs = req.job.first().cloned().unwrap_or_default();
    let not_found = || head(0x0406).finish();

    match op_code {
        CUPS_GET_DEFAULT => match default_queue(s) {
            Some(q) => { let mut m = head(0); m.group(4); printer_group(&mut m, s, &q); m.finish() }
            None => not_found(),
        },
        CUPS_GET_PRINTERS => {
            let qs = s.queues.lock().unwrap().clone();
            let mut m = head(0);
            for q in qs { m.group(4); printer_group(&mut m, s, &q); }
            m.finish()
        }
        CUPS_GET_CLASSES => head(0).finish(),
        CUPS_GET_PPD => head(0x0406).finish(),
        ipp::GET_PRINTER_ATTRS => match find_queue(s, op).or_else(|| default_queue(s)) {
            Some(q) => { let mut m = head(0); m.group(4); printer_group(&mut m, s, &q); m.finish() }
            None => not_found(),
        },
        VALIDATE_JOB => match find_queue(s, op) { Some(_) => head(0).finish(), None => not_found() },
        ipp::PRINT_JOB | CREATE_JOB => {
            let Some(q) = find_queue(s, op) else { return not_found() };
            let Ok(jid) = new_job(s, &q, op, &job_attrs) else { return head(0x0500).finish() };
            if op_code == ipp::PRINT_JOB {
                let dir = s.jobs.lock().unwrap().get(&jid).and_then(|j| j._dir.as_ref().map(|d| d.path().to_path_buf()));
                let Some(Ok(p)) = dir.map(|d| spool(&d, body, off, 0)) else { return head(0x0500).finish() };
                if let Some(j) = s.jobs.lock().unwrap().get_mut(&jid) { j.docs.push(p); }
                start_worker(s, jid, q);
            }
            let mut m = head(0);
            m.group(2);
            if let Some(j) = s.jobs.lock().unwrap().get(&jid) { job_group(&mut m, s, j); }
            m.finish()
        }
        SEND_DOCUMENT => {
            let Some(jid) = job_id(op) else { return head(0x0400).finish() };
            let info = s.jobs.lock().unwrap().get(&jid).map(|j| (j.queue.clone(), j._dir.as_ref().map(|d| d.path().to_path_buf()), j.docs.len()));
            let Some((qname, Some(dir), n)) = info else { return not_found() };
            let doc_len = body.head.len() - off.min(body.head.len()) + body.tail_len;
            if doc_len > 0 {
                let Ok(p) = spool(&dir, body, off, n) else { return head(0x0500).finish() };
                if let Some(j) = s.jobs.lock().unwrap().get_mut(&jid) { j.docs.push(p); }
            }
            if ipp::boolean(op, "last-document").unwrap_or(false) {
                let q = s.queues.lock().unwrap().iter().find(|q| q.name == qname).cloned();
                match q {
                    Some(q) => start_worker(s, jid, q),
                    None => set_job(s, jid, 8, "job-aborted-by-system"),
                }
            }
            let mut m = head(0);
            m.group(2);
            if let Some(j) = s.jobs.lock().unwrap().get(&jid) { job_group(&mut m, s, j); }
            m.finish()
        }
        ipp::GET_JOB_ATTRS => {
            let Some(jid) = job_id(op) else { return head(0x0400).finish() };
            let jobs = s.jobs.lock().unwrap();
            let Some(j) = jobs.get(&jid) else { return not_found() };
            let mut m = head(0);
            m.group(2);
            job_group(&mut m, s, j);
            m.finish()
        }
        ipp::GET_JOBS => {
            let which = ipp::text(op, "which-jobs").unwrap_or_else(|| "not-completed".into());
            let only = find_queue(s, op).map(|q| q.name);
            let jobs = s.jobs.lock().unwrap();
            let mut list: Vec<&Job> = jobs.values()
                .filter(|j| only.as_ref().is_none_or(|q| *q == j.queue))
                .filter(|j| if which == "completed" { j.state >= 7 } else { j.state < 7 }).collect();
            list.sort_by_key(|j| j.id);
            let mut m = head(0);
            for j in list { m.group(2); job_group(&mut m, s, j); }
            m.finish()
        }
        ipp::CANCEL_JOB => {
            let Some(jid) = job_id(op) else { return head(0x0400).finish() };
            let remote = {
                let mut jobs = s.jobs.lock().unwrap();
                let Some(j) = jobs.get_mut(&jid) else { return not_found() };
                if j.state >= 7 { return head(0x0407).finish(); }
                j.state = 7;
                j.reason = "job-canceled-by-user".into();
                j.finished = now();
                j.remote.clone()
            };
            if let Some((uri, rid)) = remote {
                std::thread::spawn(move || {
                    let _ = ipp::request(&uri, &|v| {
                        let mut m = Msg::standard(ipp::CANCEL_JOB, v, &uri);
                        m.i(INTEGER, "job-id", rid);
                        m.finish()
                    }, None);
                });
            }
            head(0).finish()
        }
        _ => head(0x0501).finish(),
    }
}

struct Body {
    head: Vec<u8>,
    tail: Option<tempfile::NamedTempFile>,
    tail_len: usize,
}

impl Body {
    fn new() -> Body { Body { head: vec![], tail: None, tail_len: 0 } }
    fn push(&mut self, mut b: &[u8]) -> std::io::Result<()> {
        if self.head.len() < HEAD_LIMIT {
            let n = (HEAD_LIMIT - self.head.len()).min(b.len());
            self.head.extend_from_slice(&b[..n]);
            b = &b[n..];
        }
        if !b.is_empty() {
            if self.tail.is_none() { self.tail = Some(tempfile::NamedTempFile::new()?); }
            self.tail.as_mut().unwrap().write_all(b)?;
            self.tail_len += b.len();
        }
        Ok(())
    }
}

fn read_exact_into(r: &mut impl Read, mut n: usize, body: &mut Body) -> std::io::Result<()> {
    let mut buf = [0u8; 65536];
    while n > 0 {
        let want = n.min(buf.len());
        let got = r.read(&mut buf[..want])?;
        if got == 0 { return Err(std::io::ErrorKind::UnexpectedEof.into()); }
        body.push(&buf[..got])?;
        n -= got;
    }
    Ok(())
}

fn respond(w: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(w, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nServer: brrr/0.2\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

/// Browsers can be tricked into POSTing to localhost, so only local Host names get in.
pub fn host_allowed(h: &str) -> bool {
    let h = h.trim().to_ascii_lowercase();
    let host = if h.starts_with('[') { h.split(']').next().unwrap_or("").to_string() + "]" } else { h.split(':').next().unwrap_or("").to_string() };
    matches!(host.as_str(), "localhost" | "localhost." | "127.0.0.1" | "[::1]" | "")
}

fn serve_conn(s: Arc<Server>, sock: TcpStream) -> std::io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut w = sock.try_clone()?;
    let mut r = BufReader::new(sock);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 { return Ok(()); }
        if line.trim().is_empty() { continue; }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
        let (mut len, mut chunked, mut expect, mut host, mut close) = (0usize, false, false, String::new(), false);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h)? == 0 { return Ok(()); }
            if h.trim().is_empty() { break; }
            let Some((k, v)) = h.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
                "expect" => expect = v.to_ascii_lowercase().contains("100-continue"),
                "connection" => close = v.eq_ignore_ascii_case("close"),
                "host" => host = v.to_string(),
                _ => {}
            }
        }
        if !host_allowed(&host) {
            respond(&mut w, "403 Forbidden", "text/plain", b"nope\n")?;
            return Ok(());
        }
        if expect { w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?; w.flush()?; }
        let mut body = Body::new();
        if chunked {
            loop {
                let mut sz = String::new();
                if r.read_line(&mut sz)? == 0 { return Ok(()); }
                let n = usize::from_str_radix(sz.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                if n == 0 {
                    // Eat any trailers up to the final blank line.
                    loop {
                        let mut t = String::new();
                        if r.read_line(&mut t)? == 0 || t.trim().is_empty() { break; }
                    }
                    break;
                }
                read_exact_into(&mut r, n, &mut body)?;
                let mut crlf = String::new();
                r.read_line(&mut crlf)?;
            }
        } else {
            read_exact_into(&mut r, len, &mut body)?;
        }
        match method.as_str() {
            "POST" => {
                let out = handle_ipp(&s, &body);
                respond(&mut w, "200 OK", "application/ipp", &out)?;
            }
            "GET" | "HEAD" => {
                let path = target.split('?').next().unwrap_or("");
                let name = path.strip_prefix("/printers/").and_then(|p| p.strip_suffix(".ppd")).map(percent_decode);
                let q = name.and_then(|n| s.queues.lock().unwrap().iter().find(|q| q.name == n).cloned());
                match q {
                    Some(q) => {
                        let a = attrs(&s, &q);
                        let doc = ppd(&q.display, &a);
                        if method == "HEAD" { write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", doc.len())?; w.flush()?; }
                        else { respond(&mut w, "200 OK", "application/vnd.cups-ppd", doc.as_bytes())?; }
                    }
                    None if path == "/" => respond(&mut w, "200 OK", "text/plain", b"brrr serve is running\n")?,
                    None => respond(&mut w, "404 Not Found", "text/plain", b"")?,
                }
            }
            _ => respond(&mut w, "501 Not Implemented", "text/plain", b"")?,
        }
        if close { return Ok(()); }
    }
}

fn client_conf() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("can't find your home folder")?;
    Ok(Path::new(&home).join(".cups").join("client.conf"))
}

fn unit_path() -> Result<PathBuf, String> {
    Ok(crate::config_home().ok_or("can't find your config folder")?.join("systemd/user/brrr.service"))
}

fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl").arg("--user").args(args).status().map(|s| s.success()).unwrap_or(false)
}

const MARK: &str = "# managed by brrr serve --install\n";

fn install(port: u16) -> Result<(), String> {
    let e = |e: std::io::Error| e.to_string();
    let exe = std::env::current_exe().map_err(e)?;
    let unit = unit_path()?;
    std::fs::create_dir_all(unit.parent().unwrap()).map_err(e)?;
    std::fs::write(&unit, format!("[Unit]\nDescription=brrr print server\n\n[Service]\nExecStart={} serve --port {port}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n", exe.display())).map_err(e)?;
    let conf = client_conf()?;
    std::fs::create_dir_all(conf.parent().unwrap()).map_err(e)?;
    if let Ok(old) = std::fs::read_to_string(&conf) {
        if !old.starts_with(MARK) { std::fs::write(conf.with_extension("conf.pre-brrr"), old).map_err(e)?; }
    }
    std::fs::write(&conf, format!("{MARK}ServerName 127.0.0.1:{port}\n")).map_err(e)?;
    if systemctl(&["daemon-reload"]) && systemctl(&["enable", "--now", "brrr.service"]) {
        println!("done. restart your apps and Ctrl+P will print through brrr");
        Ok(())
    } else {
        Err(format!("wrote the config, but systemd wouldn't start it. run `{} serve --port {port}` yourself", exe.display()))
    }
}

fn uninstall() -> Result<(), String> {
    let e = |e: std::io::Error| e.to_string();
    systemctl(&["disable", "--now", "brrr.service"]);
    let unit = unit_path()?;
    if unit.exists() { std::fs::remove_file(unit).map_err(e)?; }
    systemctl(&["daemon-reload"]);
    let conf = client_conf()?;
    if std::fs::read_to_string(&conf).is_ok_and(|c| c.starts_with(MARK)) {
        std::fs::remove_file(&conf).map_err(e)?;
        let bak = conf.with_extension("conf.pre-brrr");
        if bak.exists() { std::fs::rename(bak, &conf).map_err(e)?; }
    }
    println!("undone. apps are back on CUPS");
    Ok(())
}

pub fn main(args: &[String]) -> Result<(), String> {
    let mut port = DEFAULT_PORT;
    let mut action = "run";
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next().and_then(|p| p.parse().ok()).ok_or("--port needs a number")?,
            "--install" => action = "install",
            "--uninstall" => action = "uninstall",
            o => return Err(format!("no idea what {o} means")),
        }
    }
    match action {
        "install" => return install(port),
        "uninstall" => return uninstall(),
        _ => {}
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("can't listen on port {port}: {e}"))?;
    let s = Arc::new(Server {
        port, queues: Mutex::new(vec![]), jobs: Mutex::new(HashMap::new()), next: AtomicI32::new(1),
        cache: Mutex::new(HashMap::new()), started: Instant::now(),
    });
    let bg = s.clone();
    std::thread::spawn(move || loop {
        refresh(&bg);
        std::thread::sleep(Duration::from_secs(20));
    });
    eprintln!("brrr serve: listening on 127.0.0.1:{port}");
    for c in listener.incoming().flatten() {
        let s = s.clone();
        std::thread::spawn(move || { let _ = serve_conn(s, c); });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_names_are_cups_safe() {
        assert_eq!(queue_name("HP LaserJet/Pro @ home"), "HP_LaserJet_Pro___home");
        assert_eq!(queue_name("///"), "printer");
    }

    #[test]
    fn only_local_hosts_allowed() {
        for h in ["localhost", "localhost:6631", "127.0.0.1:6631", "[::1]:6631"] { assert!(host_allowed(h), "{h}"); }
        for h in ["evil.com", "evil.com:6631", "localhost.evil.com", "127.0.0.1.nip.io"] { assert!(!host_allowed(h), "{h}"); }
    }

    #[test]
    fn app_options_map_to_brrr_options() {
        let mut j = Group::new();
        j.insert("copies".into(), vec![Value::Int(3)]);
        j.insert("Duplex".into(), vec![Value::Text("DuplexTumble".into())]);
        j.insert("PageSize".into(), vec![Value::Text("iso_a4_210x297mm".into())]);
        j.insert("ColorModel".into(), vec![Value::Text("Gray".into())]);
        j.insert("page-ranges".into(), vec![Value::Range(2, 4)]);
        let o = job_opts(&j, &Group::new());
        assert_eq!(o.copies, 3);
        assert_eq!(o.sides, Some(Sides::Short));
        assert_eq!(o.size.as_deref(), Some("iso_a4_210x297mm"));
        assert_eq!(o.color, Some(false));
        assert_eq!(o.pages, vec![(2, Some(4))]);
    }

    #[test]
    fn percent_decoding_handles_bad_input() {
        assert_eq!(percent_decode("A%20B"), "A B");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
