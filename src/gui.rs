//! `brrr gui`: printers on the left, your page in the middle, settings on the right.
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, TextureHandle, Ui};
use crate::ipp::{self, Uri};
use crate::raster::Sides;
use crate::render::{self, Kind};
use crate::{human, mdns, Opts};
use eframe::egui::text::LayoutJob;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const PAPER: Color32 = Color32::from_rgb(0xF4, 0xF1, 0xE8);
const PANEL: Color32 = Color32::from_rgb(0xE9, 0xE5, 0xD8);
const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const MUTED: Color32 = Color32::from_rgb(0x55, 0x51, 0x47);
const EDGE: Color32 = Color32::from_rgb(0xB5, 0xAF, 0x9E);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD4, 0x00);
const BAD: Color32 = Color32::from_rgb(0xB3, 0x12, 0x2B);
const LOGO: &[u8] = include_bytes!("../assets/logo.png");

#[derive(Clone)]
struct Ink {
    name: String,
    level: i32,
    color: Color32,
}

#[derive(Clone)]
struct Row {
    name: String,
    uri: Option<Uri>,
    link: &'static str,
    state: String,
    error: Option<String>,
    ink: Vec<Ink>,
    medias: Vec<String>,
    color: bool,
    duplex: bool,
}

enum Ev {
    Printers(Vec<Row>),
    Picked(PathBuf),
    Notice(String),
    Preview(u64, Result<Option<egui::ColorImage>, String>),
    Log(usize, String),
    Remote(usize, Uri, i32),
    Done(usize, Result<(), String>),
}

struct Job {
    label: String,
    printer: String,
    status: String,
    finished: Option<bool>,
    remote: Option<(Uri, i32)>,
}

#[derive(PartialEq, Clone, Copy)]
enum SidesPick { Default, One, Long, Short }
#[derive(PartialEq, Clone, Copy)]
enum ColorPick { Default, Color, Gray }

struct App {
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    logo: TextureHandle,
    printers: Vec<Row>,
    selected: Option<String>,
    scanning: bool,
    last_scan: Instant,
    file: Option<PathBuf>,
    path_text: String,
    preview: Option<TextureHandle>,
    preview_size: [usize; 2],
    preview_note: String,
    token: u64,
    notice: String,
    copies: u32,
    sides: SidesPick,
    color: ColorPick,
    size: Option<String>,
    pages: String,
    jobs: Vec<Job>,
}

fn hex_color(s: &str) -> Color32 {
    let h = s.trim_start_matches('#');
    match (h.get(0..2), h.get(2..4), h.get(4..6)) {
        (Some(r), Some(g), Some(b)) => match (u8::from_str_radix(r, 16), u8::from_str_radix(g, 16), u8::from_str_radix(b, 16)) {
            (Ok(r), Ok(g), Ok(b)) => Color32::from_rgb(r, g, b),
            _ => MUTED,
        },
        _ => MUTED,
    }
}

fn probe(p: &mdns::Printer) -> Row {
    let mut row = Row { name: p.name.clone(), uri: None, link: p.via, state: String::new(), error: None, ink: vec![], medias: vec![], color: false, duplex: false };
    let uri = match crate::connectable(p, false) {
        Ok((_, u)) => u,
        Err(e) => { row.error = Some(e); return row; }
    };
    row.link = if uri.tls { "encrypted" } else if p.via == "usb" { "usb" } else { "plain" };
    match ipp::get_attrs(&uri, &["all"]) {
        Ok(a) => {
            row.state = human::state(&a);
            let colors = ipp::texts(&a, "marker-colors");
            row.ink = ipp::texts(&a, "marker-names").into_iter().zip(ipp::ints(&a, "marker-levels")).enumerate().map(|(i, (name, level))| {
                let color = colors.get(i).map(|c| hex_color(c)).unwrap_or(MUTED);
                Ink { name, level, color }
            }).collect();
            row.medias = ipp::texts(&a, "media-supported").into_iter().filter(|m| !m.contains("custom_m") && crate::media_size(m).is_some()).collect();
            row.color = ipp::boolean(&a, "color-supported").unwrap_or(false);
            row.duplex = ipp::texts(&a, "sides-supported").iter().any(|s| s != "one-sided");
        }
        Err(e) => row.error = Some(e),
    }
    row.uri = Some(uri);
    row
}

fn scan(tx: Sender<Ev>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let found = mdns::discover(Duration::from_millis(1500));
        let handles: Vec<_> = found.into_iter().map(|p| std::thread::spawn(move || probe(&p))).collect();
        let rows: Vec<Row> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
        let _ = tx.send(Ev::Printers(rows));
        ctx.request_repaint();
    });
}

fn pdf_thumb(pdf: &Path, dir: &Path) -> Result<image::RgbImage, String> {
    let out = dir.join("thumb");
    let st = Command::new("pdftoppm").args(["-r", "90", "-f", "1", "-l", "1", "-singlefile", "-png"]).arg(pdf).arg(&out)
        .output().map_err(|_| "install poppler to see PDF previews".to_string())?;
    if !st.status.success() { return Err("couldn't read this PDF".into()); }
    Ok(image::open(out.with_extension("png")).map_err(|e| e.to_string())?.into_rgb8())
}

fn make_preview(path: &Path) -> Result<Option<egui::ColorImage>, String> {
    let kind = render::sniff(path)?;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let img = match kind {
        Kind::Pdf => pdf_thumb(path, tmp.path())?,
        Kind::Png | Kind::Jpeg => render::load_photo(path)?,
        Kind::Text => {
            let p = tmp.path().join("t.pdf");
            render::text_to_pdf(path, &p, (612.0, 792.0))?;
            pdf_thumb(&p, tmp.path())?
        }
        Kind::PostScript => {
            let p = tmp.path().join("p.pdf");
            render::ps_to_pdf(path, &p)?;
            pdf_thumb(&p, tmp.path())?
        }
        Kind::Office => return Ok(None),
    };
    let img = if img.height() > 1100 || img.width() > 1100 {
        let s = 1100.0 / img.width().max(img.height()) as f64;
        image::imageops::thumbnail(&img, (img.width() as f64 * s) as u32, (img.height() as f64 * s) as u32)
    } else { img };
    Ok(Some(egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw())))
}

fn pick_file(tx: Sender<Ev>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let tries: [(&str, &[&str]); 2] = [
            ("zenity", &["--file-selection", "--title=Pick something to print"]),
            ("kdialog", &["--getopenfilename", "."]),
        ];
        let mut ran = false;
        for (bin, args) in tries {
            let Ok(o) = Command::new(bin).args(args).output() else { continue };
            ran = true;
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if o.status.success() && !s.is_empty() { let _ = tx.send(Ev::Picked(PathBuf::from(s))); }
            break;
        }
        if !ran { let _ = tx.send(Ev::Notice("no file picker found. install zenity, drag a file in, or paste its path".into())); }
        ctx.request_repaint();
    });
}

fn nice_media(m: &str) -> String {
    match m.split('_').nth(1) {
        Some("letter") => "US Letter".into(),
        Some("legal") => "US Legal".into(),
        Some("a3") => "A3".into(),
        Some("a4") => "A4".into(),
        Some("a5") => "A5".into(),
        _ => m.rsplit('_').next().unwrap_or(m).to_string(),
    }
}

impl App {
    fn new(cc: &eframe::CreationContext, file: Option<PathBuf>) -> App {
        let ctx = &cc.egui_ctx;
        ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::Light);
        ctx.set_style_of(egui::Theme::Light, style(&ctx.global_style()));
        let img = image::load_from_memory(LOGO).expect("bundled logo").to_rgba8();
        let (w, h) = img.dimensions();
        let logo = ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw()), egui::TextureOptions::LINEAR);
        let (tx, rx) = channel();
        scan(tx.clone(), ctx.clone());
        let mut app = App {
            tx, rx, logo, printers: vec![], selected: None, scanning: true, last_scan: Instant::now(),
            file: None, path_text: String::new(), preview: None, preview_size: [1, 1], preview_note: String::new(), token: 0, notice: String::new(),
            copies: 1, sides: SidesPick::Default, color: ColorPick::Default, size: None, pages: String::new(), jobs: vec![],
        };
        if let Some(f) = file { app.load(f, ctx); }
        app
    }

    fn load(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.path_text = path.display().to_string();
        self.token += 1;
        self.preview = None;
        self.notice.clear();
        if let Err(e) = render::sniff(&path) {
            self.file = None;
            self.preview_note = e;
            return;
        }
        self.preview_note = "loading preview...".into();
        self.file = Some(path.clone());
        let (tx, tok, c) = (self.tx.clone(), self.token, ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Ev::Preview(tok, make_preview(&path)));
            c.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Printers(rows) => {
                    self.printers = rows;
                    self.scanning = false;
                    self.last_scan = Instant::now();
                    if !self.printers.iter().any(|p| Some(&p.name) == self.selected.as_ref()) {
                        let saved = crate::saved_default().unwrap_or_default().to_lowercase();
                        let pick = self.printers.iter().find(|p| !saved.is_empty() && p.name.to_lowercase().contains(&saved))
                            .or_else(|| self.printers.iter().find(|p| p.error.is_none()))
                            .or(self.printers.first());
                        self.selected = pick.map(|p| p.name.clone());
                        self.size = None;
                    }
                }
                Ev::Picked(p) => self.load(p, ctx),
                Ev::Notice(n) => self.notice = n,
                Ev::Preview(t, r) if t == self.token => match r {
                    Ok(Some(img)) => {
                        self.preview_size = img.size;
                        // mipmaps keep small text readable when the page is shrunk to fit
                        let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
                        self.preview = Some(ctx.load_texture("preview", img, opts));
                        self.preview_note.clear();
                    }
                    Ok(None) => self.preview_note = "no preview for office files, but it'll print".into(),
                    Err(e) => self.preview_note = e,
                },
                Ev::Preview(..) => {}
                Ev::Log(i, s) => if let Some(j) = self.jobs.get_mut(i) { j.status = s; },
                Ev::Remote(i, u, id) => if let Some(j) = self.jobs.get_mut(i) { j.remote = Some((u, id)); },
                Ev::Done(i, r) => if let Some(j) = self.jobs.get_mut(i) {
                    match r {
                        Ok(()) => { j.finished = Some(true); j.status = "done, go grab it".into(); }
                        Err(e) => { j.finished = Some(false); j.status = e; }
                    }
                    j.remote = None;
                },
            }
        }
        if !self.scanning && self.last_scan.elapsed() > Duration::from_secs(30) { self.rescan(ctx); }
        ctx.request_repaint_after(Duration::from_secs(5));
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        self.scanning = true;
        scan(self.tx.clone(), ctx.clone());
    }

    fn current(&self) -> Option<&Row> {
        self.printers.iter().find(|p| Some(&p.name) == self.selected.as_ref())
    }

    /// Why Print is greyed out, or None if it's good to go.
    fn blocked(&self) -> Option<String> {
        if self.file.is_none() { return Some("pick a file first".into()); }
        let Some(p) = self.current() else { return Some("pick a printer".into()) };
        if let Some(e) = &p.error { return Some(e.clone()); }
        if !self.pages.trim().is_empty() {
            if let Err(e) = crate::parse_pages(self.pages.trim()) { return Some(e); }
        }
        None
    }

    fn start_print(&mut self, ctx: &egui::Context) {
        let (Some(file), Some(p)) = (self.file.clone(), self.current().cloned()) else { return };
        let Some(uri) = p.uri.clone() else { return };
        let mut o = Opts { copies: self.copies.max(1), ..Default::default() };
        o.size = self.size.clone();
        o.sides = match self.sides { SidesPick::Default => None, SidesPick::One => Some(Sides::One), SidesPick::Long => Some(Sides::Long), SidesPick::Short => Some(Sides::Short) };
        o.color = match self.color { ColorPick::Default => None, ColorPick::Color => Some(true), ColorPick::Gray => Some(false) };
        if !self.pages.trim().is_empty() { o.pages = crate::parse_pages(self.pages.trim()).unwrap_or_default(); }
        let label = file.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
        self.jobs.push(Job { label: label.clone(), printer: p.name.clone(), status: "starting...".into(), finished: None, remote: None });
        let i = self.jobs.len() - 1;
        let (tx, c) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (ltx, lc) = (tx.clone(), c.clone());
            crate::set_sink(Box::new(move |s: &str| { let _ = ltx.send(Ev::Log(i, s.to_string())); lc.request_repaint(); }));
            let (rtx, ru, rc) = (tx.clone(), uri.clone(), c.clone());
            let mut on_job = move |id: i32| { let _ = rtx.send(Ev::Remote(i, ru.clone(), id)); rc.request_repaint(); };
            let r = crate::print_to(&file, &label, &o, &p.name, &uri, &mut on_job);
            let _ = tx.send(Ev::Done(i, r));
            c.request_repaint();
        });
    }

    fn left(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Printers").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!self.scanning, egui::Button::new(if self.scanning { "looking..." } else { "rescan" })).clicked() { self.rescan(&ctx); }
            });
        });
        ui.add_space(6.0);
        if self.printers.is_empty() && !self.scanning {
            ui.label(RichText::new("no printers found. make sure it's on and on your network, then rescan").color(MUTED));
        }
        let mut chosen = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for p in &self.printers {
                let on = Some(&p.name) == self.selected.as_ref();
                let (fg, sub) = if on { (PAPER, Color32::from_rgb(0xD8, 0xD4, 0xC8)) } else { (INK, MUTED) };
                let frame = egui::Frame::new().fill(if on { INK } else { PAPER }).stroke(Stroke::new(1.0, if on { INK } else { EDGE })).corner_radius(CornerRadius::same(2)).inner_margin(Margin::same(8));
                let r = frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&p.name).color(fg).strong());
                    match &p.error {
                        Some(e) => { ui.label(RichText::new(e).color(if on { PAPER } else { BAD }).small()); }
                        None => { ui.label(RichText::new(format!("{}  ·  {}", p.state, p.link)).color(sub).small()); }
                    }
                    for k in &p.ink { ink_bar(ui, k, fg, on); }
                }).response.interact(Sense::click());
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() { chosen = Some(p.name.clone()); }
                ui.add_space(6.0);
            }
        });
        if let Some(c) = chosen {
            if self.selected.as_ref() != Some(&c) { self.selected = Some(c); self.size = None; }
        }
    }

    fn right(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let cur = self.current().cloned();
        ui.label(RichText::new("Settings").strong());
        ui.add_space(8.0);
        let label = |ui: &mut Ui, s: &str| { ui.add_sized([70.0, 22.0], egui::Label::new(RichText::new(s).color(MUTED))); };

        ui.horizontal(|ui| { label(ui, "Copies"); ui.add(egui::DragValue::new(&mut self.copies).range(1..=999)); });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            label(ui, "Paper");
            let medias = cur.as_ref().map(|c| c.medias.clone()).unwrap_or_default();
            let text = self.size.as_deref().map(nice_media).unwrap_or_else(|| "printer default".into());
            egui::ComboBox::from_id_salt("paper").selected_text(text).width(ui.available_width() - 4.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.size, None, "printer default");
                for m in medias { let t = nice_media(&m); ui.selectable_value(&mut self.size, Some(m), t); }
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            label(ui, "Sides");
            let duplex = cur.as_ref().is_some_and(|c| c.duplex);
            let name = |s: SidesPick| match s { SidesPick::Default => "printer default", SidesPick::One => "one side", SidesPick::Long => "both, long edge", SidesPick::Short => "both, short edge" };
            ui.add_enabled_ui(duplex, |ui| {
                egui::ComboBox::from_id_salt("sides").selected_text(name(self.sides)).width(ui.available_width() - 4.0).show_ui(ui, |ui| {
                    for s in [SidesPick::Default, SidesPick::One, SidesPick::Long, SidesPick::Short] { ui.selectable_value(&mut self.sides, s, name(s)); }
                });
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            label(ui, "Color");
            let can = cur.as_ref().is_some_and(|c| c.color);
            let name = |s: ColorPick| match s { ColorPick::Default => "printer default", ColorPick::Color => "color", ColorPick::Gray => "black and white" };
            ui.add_enabled_ui(can, |ui| {
                egui::ComboBox::from_id_salt("color").selected_text(name(self.color)).width(ui.available_width() - 4.0).show_ui(ui, |ui| {
                    for s in [ColorPick::Default, ColorPick::Color, ColorPick::Gray] { ui.selectable_value(&mut self.color, s, name(s)); }
                });
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            label(ui, "Pages");
            ui.add(egui::TextEdit::singleline(&mut self.pages).hint_text("all, or 1-3,7").desired_width(ui.available_width() - 4.0));
        });
        if cur.as_ref().is_some_and(|c| c.ink.iter().any(|k| (0..=15).contains(&k.level))) {
            ui.add_space(8.0);
            ui.label(RichText::new("ink is low, colors might print faint or not at all").color(BAD).small());
        }
        ui.add_space(14.0);
        let blocked = self.blocked();
        let btn = egui::Button::new(RichText::new("Print").color(INK).strong().size(16.0)).fill(if blocked.is_none() { YELLOW } else { PANEL }).stroke(Stroke::new(1.0, INK));
        let size = [ui.available_width(), 40.0];
        let can_print = blocked.is_none();
        if ui.add_enabled_ui(can_print, |ui| ui.add_sized(size, btn)).inner.clicked() { self.start_print(&ctx); }
        if let Some(b) = blocked { ui.add_space(6.0); ui.label(RichText::new(b).color(MUTED).small()); }
    }

    fn center(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            if ui.button("Browse...").clicked() { pick_file(self.tx.clone(), ctx.clone()); }
            let r = ui.add(egui::TextEdit::singleline(&mut self.path_text).hint_text("or paste a file path").desired_width(ui.available_width()));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.path_text.trim().is_empty() {
                let p = PathBuf::from(self.path_text.trim());
                self.load(p, &ctx);
            }
        });
        if !self.notice.is_empty() { ui.add_space(4.0); ui.label(RichText::new(&self.notice).color(BAD).small()); }
        ui.add_space(8.0);
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        let paint = ui.painter_at(rect);
        paint.rect_filled(rect, CornerRadius::same(2), PANEL);
        paint.rect_stroke(rect, CornerRadius::same(2), Stroke::new(if hovering { 2.0 } else { 1.0 }, if hovering { INK } else { EDGE }), egui::StrokeKind::Inside);
        if let Some(t) = &self.preview {
            let [w, h] = self.preview_size;
            let s = ((rect.width() - 32.0) / w as f32).min((rect.height() - 32.0) / h as f32).max(0.01);
            let r = egui::Rect::from_center_size(rect.center(), egui::vec2(w as f32 * s, h as f32 * s));
            paint.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            paint.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, EDGE), egui::StrokeKind::Outside);
        } else {
            let msg = if !self.preview_note.is_empty() { self.preview_note.clone() } else { "drop a file here".to_string() };
            let mut job = LayoutJob::simple(msg, FontId::proportional(16.0), MUTED, (rect.width() - 48.0).max(80.0));
            job.halign = egui::Align::Center;
            let g = ui.fonts_mut(|f| f.layout_job(job));
            paint.galley(egui::pos2(rect.center().x, rect.center().y - g.size().y / 2.0), g, MUTED);
        }
    }

    fn bottom(&mut self, ui: &mut Ui) {
        let mut cancel = None;
        ui.label(RichText::new("Jobs").strong());
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if self.jobs.is_empty() { ui.label(RichText::new("nothing printed yet").color(MUTED)); }
            for (i, j) in self.jobs.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&j.label).strong());
                    ui.label(RichText::new(&j.printer).color(MUTED));
                    ui.label(RichText::new(&j.status).color(if j.finished == Some(false) { BAD } else { INK }));
                    if j.remote.is_some() && j.finished.is_none() && ui.small_button("cancel").clicked() { cancel = Some(i); }
                });
            }
        });
        if let Some(i) = cancel {
            let Some((uri, id)) = self.jobs[i].remote.take() else { return };
            self.jobs[i].status = "canceling...".into();
            std::thread::spawn(move || {
                let _ = ipp::request(&uri, &|v| {
                    let mut m = ipp::Msg::standard(ipp::CANCEL_JOB, v, &uri);
                    m.i(ipp::INTEGER, "job-id", id);
                    m.finish()
                }, None);
            });
        }
    }
}

fn ink_bar(ui: &mut Ui, k: &Ink, fg: Color32, on: bool) {
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        let text = if k.level < 0 { "??".to_string() } else { format!("{}%", k.level) };
        let (r, _) = ui.allocate_exact_size(egui::vec2(86.0, 8.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, CornerRadius::ZERO, if on { Color32::from_rgb(0x3A, 0x3A, 0x3A) } else { PANEL });
        if k.level > 0 {
            let w = r.width() * k.level.min(100) as f32 / 100.0;
            // black ink on a selected (black) row would disappear
            let dark = k.color.r() < 60 && k.color.g() < 60 && k.color.b() < 60;
            p.rect_filled(egui::Rect::from_min_size(r.min, egui::vec2(w, r.height())), CornerRadius::ZERO, if on && dark { PAPER } else { k.color });
        }
        ui.label(RichText::new(format!("{text} {}", k.name)).color(fg).small());
    });
}

fn style(base: &egui::Style) -> egui::Style {
    let mut s = base.clone();
    let r = CornerRadius::same(2);
    s.interaction.selectable_labels = false;
    s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
    s.spacing.item_spacing = egui::vec2(6.0, 4.0);
    s.spacing.button_padding = egui::vec2(8.0, 4.0);
    let v = &mut s.visuals;
    *v = egui::Visuals::light();
    v.panel_fill = PAPER;
    v.window_fill = PAPER;
    v.extreme_bg_color = Color32::WHITE;
    v.faint_bg_color = PANEL;
    v.override_text_color = Some(INK);
    v.window_stroke = Stroke::new(1.0, EDGE);
    v.window_corner_radius = r;
    v.menu_corner_radius = r;
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.selection.bg_fill = YELLOW;
    v.selection.stroke = Stroke::new(1.0, INK);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
        w.expansion = 0.0;
        w.fg_stroke = Stroke::new(1.0, INK);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, EDGE);
    v.widgets.inactive.bg_fill = PANEL;
    v.widgets.inactive.weak_bg_fill = PANEL;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, EDGE);
    v.widgets.hovered.bg_fill = PAPER;
    v.widgets.hovered.weak_bg_fill = PAPER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, INK);
    v.widgets.active.bg_fill = YELLOW;
    v.widgets.active.weak_bg_fill = YELLOW;
    v.widgets.active.bg_stroke = Stroke::new(1.0, INK);
    v.widgets.open.bg_fill = PAPER;
    v.widgets.open.weak_bg_fill = PAPER;
    v.widgets.open.bg_stroke = Stroke::new(1.0, INK);
    s
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        let dropped: Option<PathBuf> = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()));
        if let Some(f) = dropped { self.load(f, &ctx); }

        let edge = |side_margin: i8| egui::Frame::new().fill(PAPER).stroke(Stroke::new(1.0, EDGE)).inner_margin(Margin::same(side_margin));
        egui::Panel::top("head").exact_size(44.0).frame(edge(8)).show_separator_line(false).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(26.0, 26.0)));
                ui.label(RichText::new("brrr").strong().size(18.0).font(FontId::monospace(18.0)));
                ui.label(RichText::new("printing without the suffering").color(MUTED));
            });
        });
        egui::Panel::bottom("jobs").exact_size(120.0).frame(edge(10)).show_separator_line(false).show(ui, |ui| self.bottom(ui));
        egui::Panel::left("printers").exact_size(260.0).frame(edge(10)).show_separator_line(false).show(ui, |ui| self.left(ui));
        egui::Panel::right("settings").exact_size(270.0).frame(edge(10)).show_separator_line(false).show(ui, |ui| self.right(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(PAPER).inner_margin(Margin::same(10))).show(ui, |ui| self.center(ui));
    }
}

pub fn main(file: Option<PathBuf>) -> Result<(), String> {
    let mut vp = egui::ViewportBuilder::default().with_title("brrr").with_app_id("brrr").with_inner_size([1040.0, 680.0]).with_min_inner_size([820.0, 520.0]);
    if let Ok(img) = image::load_from_memory(LOGO) {
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        vp = vp.with_icon(egui::IconData { rgba: rgba.into_raw(), width, height });
    }
    let opts = eframe::NativeOptions { viewport: vp, ..Default::default() };
    eframe::run_native("brrr", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, file)))))
        .map_err(|e| format!("couldn't open a window: {e}"))
}
