//! La fenêtre « pellicule » : toutes les miniatures, de la plus récente à la plus ancienne,
//! groupées par mois. Les imports et la surveillance de « À trier » tournent en arrière-plan.

use crate::album::{self, Album, InboxWatcher, Mode, Outcome};
use crate::{config, platform, thumbs, update};
use eframe::egui::{
    self, Align, Align2, Color32, Context, CornerRadius, FontId, Id, LayerId, Layout, Order, Rect,
    Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

const MONTHS: [&str; 12] = [
    "janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre",
    "novembre", "décembre",
];
const GAP: f32 = 4.0;
const HEADER: f32 = 46.0;
/// Au-delà, on libère les miniatures loin de l'écran.
const MAX_TEXTURES: usize = 800;

pub fn run(album_flag: Option<PathBuf>, paths: Vec<PathBuf>) -> io::Result<()> {
    let root = config::album(album_flag.as_deref());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pellicule")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([420.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("Pellicule", options, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, root, paths)))))
        .map_err(|e| io::Error::other(e.to_string()))
}

// ------------------------------------------------------------------ tâches de fond

enum Cmd {
    Import(Vec<PathBuf>),
}

enum Ev {
    Loaded(Vec<String>),
    Added(String),
    Progress(usize, usize),
    Done(String),
    Status(String),
}

fn jobs(root: PathBuf, cmds: Receiver<Cmd>, events: Sender<Ev>, ctx: Context) {
    let send = |e: Ev| {
        let _ = events.send(e);
        ctx.request_repaint();
    };
    let mut album = match Album::open(root, false) {
        Ok(a) => a,
        Err(e) => return send(Ev::Status(format!("Impossible d'ouvrir l'album : {e}"))),
    };
    send(Ev::Loaded(album.files()));

    let mut watcher = InboxWatcher::default();
    loop {
        match cmds.recv_timeout(Duration::from_secs(2)) {
            Ok(Cmd::Import(paths)) => {
                send(Ev::Status("Recherche des photos et vidéos…".into()));
                let sources = album::gather(&album.root, &paths);
                if let Some(w) = sources.warnings.into_iter().last() {
                    send(Ev::Status(w));
                }
                if !sources.inbox.is_empty() {
                    batch(&mut album, &sources.inbox, Mode::Move, &send);
                    album::prune_empty_dirs(&album.inbox());
                }
                if sources.outside.is_empty() {
                    send(Ev::Done("Aucune nouvelle photo ni vidéo trouvée.".into()));
                } else {
                    batch(&mut album, &sources.outside, Mode::Copy, &send);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let ready = watcher.poll(&album.inbox());
                if !ready.is_empty() {
                    batch(&mut album, &ready, Mode::Move, &send);
                    album::prune_empty_dirs(&album.inbox());
                    watcher.done(ready);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn batch(album: &mut Album, files: &[PathBuf], mode: Mode, send: &dyn Fn(Ev)) {
    let (mut added, mut dups, mut errors) = (0, 0, 0);
    for (i, f) in files.iter().enumerate() {
        send(Ev::Progress(i, files.len()));
        match album.import(f, mode) {
            Ok(Outcome::Added { rel, .. }) => {
                added += 1;
                send(Ev::Added(rel));
            }
            Ok(Outcome::Duplicate { .. }) => dups += 1,
            Ok(Outcome::Ignored) => {}
            Err(_) => errors += 1,
        }
        if i % 500 == 499 {
            let _ = album.save();
        }
    }
    let save = album.save();
    let mut msg = format!("{added} ajoutée{} à ta pellicule", if added > 1 { "s" } else { "" });
    if dups > 0 {
        msg += &format!(", {dups} déjà présente{}", if dups > 1 { "s" } else { "" });
    }
    if errors > 0 {
        msg += &format!(", {errors} erreur{}", if errors > 1 { "s" } else { "" });
    }
    if let Err(e) = save {
        msg += &format!(" (index non enregistré : {e})");
    }
    send(Ev::Done(msg + "."));
}

// ------------------------------------------------------------------ modèle

struct Item {
    rel: String,
    path: PathBuf,
    video: bool,
    /// `2023-08`, ou vide si le fichier n'est pas rangé par date.
    month: String,
}

impl Item {
    fn new(root: &Path, rel: String) -> Item {
        let mut parts = rel.split('/');
        let month = match (parts.next(), parts.next(), parts.next()) {
            (Some(y), Some(m), Some(_)) if y.len() == 4 && m.len() == 7 && m.starts_with(y) => m.to_string(),
            _ => String::new(),
        };
        let path = rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c));
        let video = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|e| crate::date::kind_of(&e.to_ascii_lowercase()))
            == Some(crate::date::Kind::Video);
        Item { rel, path, video, month }
    }

    /// Ordre d'affichage : les plus récents d'abord, les fichiers non datés à la fin.
    fn key(&self) -> (bool, std::cmp::Reverse<&str>) {
        (self.month.is_empty(), std::cmp::Reverse(self.rel.as_str()))
    }
}

fn month_label(month: &str) -> String {
    let m: usize = month.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(0);
    match MONTHS.get(m.wrapping_sub(1)) {
        Some(name) => {
            let mut c = name.chars();
            let first = c.next().unwrap().to_uppercase().collect::<String>();
            format!("{first}{} {}", c.as_str(), &month[..4])
        }
        None => "Autres".into(),
    }
}

/// `2023-08-14_18-42-05.jpg` → « 14 août 2023 à 18:42 ».
fn date_label(rel: &str) -> Option<String> {
    let name = rel.rsplit('/').next()?;
    let n = |r: std::ops::Range<usize>| name.get(r)?.parse::<usize>().ok();
    let (y, m, d, h, mi) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?);
    Some(format!("{d} {} {y} à {h:02}:{mi:02}", MONTHS.get(m.checked_sub(1)?)?))
}

enum Row {
    Header { label: String, count: usize },
    Thumbs { start: usize, end: usize },
}

struct Grid {
    cols: usize,
    cell: f32,
    rows: Vec<(f32, f32, Row)>,
    height: f32,
}

impl Grid {
    fn build(items: &[Item], cols: usize, cell: f32) -> Grid {
        let mut rows = Vec::new();
        let mut y = 0.0;
        let mut i = 0;
        while i < items.len() {
            let month = &items[i].month;
            let end = i + items[i..].iter().take_while(|it| &it.month == month).count();
            rows.push((y, HEADER, Row::Header { label: month_label(month), count: end - i }));
            y += HEADER;
            for start in (i..end).step_by(cols) {
                rows.push((y, cell, Row::Thumbs { start, end: (start + cols).min(end) }));
                y += cell;
            }
            i = end;
        }
        Grid { cols, cell, rows, height: y + GAP }
    }

    /// Les lignes qui touchent l'intervalle vertical `[top, bottom]`.
    fn rows_in(&self, top: f32, bottom: f32) -> impl Iterator<Item = &(f32, f32, Row)> {
        let first = self.rows.partition_point(|(y, h, _)| y + h < top);
        self.rows[first..].iter().take_while(move |(y, _, _)| *y <= bottom)
    }
}

enum Thumb {
    Ready(TextureHandle),
    Missing,
}

// ------------------------------------------------------------------ application

struct App {
    root: Option<PathBuf>,
    setup_path: String,
    pending: Vec<PathBuf>,
    cmds: Option<Sender<Cmd>>,
    events: Option<Receiver<Ev>>,
    loaded: bool,
    items: Vec<Item>,
    counts: (usize, usize),
    grid: Option<Grid>,
    thumbs: HashMap<String, Thumb>,
    loader: thumbs::Loader,
    size: f32,
    progress: Option<(usize, usize)>,
    status: String,
    capture: Option<PathBuf>,
    quiet_frames: u32,
    thumb_failures: usize,
    update: update::State,
    updates: Option<Receiver<update::State>>,
}

impl App {
    fn new(ctx: &Context, root: Option<PathBuf>, pending: Vec<PathBuf>) -> App {
        let mut app = App {
            root: None,
            setup_path: config::default_album().display().to_string(),
            pending,
            cmds: None,
            events: None,
            loaded: false,
            items: Vec::new(),
            counts: (0, 0),
            grid: None,
            thumbs: HashMap::new(),
            loader: thumbs::Loader::new(ctx.clone()),
            size: 160.0,
            progress: None,
            status: String::new(),
            capture: std::env::var_os("PELLICULE_CAPTURE").map(PathBuf::from),
            quiet_frames: 0,
            thumb_failures: 0,
            update: update::State::Unknown,
            updates: Some(update::check_in_background(ctx.clone())),
        };
        if let Some(root) = root {
            app.start(ctx, root);
        }
        app
    }

    fn start(&mut self, ctx: &Context, root: PathBuf) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let (r, c) = (root.clone(), ctx.clone());
        std::thread::spawn(move || jobs(r, cmd_rx, ev_tx, c));
        if !self.pending.is_empty() {
            let _ = cmd_tx.send(Cmd::Import(std::mem::take(&mut self.pending)));
        }
        self.root = Some(root);
        self.cmds = Some(cmd_tx);
        self.events = Some(ev_rx);
        self.status = "Ouverture de l'album…".into();
    }

    fn import(&mut self, paths: Vec<PathBuf>) {
        if let Some(cmds) = &self.cmds {
            let _ = cmds.send(Cmd::Import(paths));
        }
    }

    fn handle_events(&mut self) {
        let Some(events) = &self.events else { return };
        let root = self.root.clone().unwrap_or_default();
        let mut changed = false;
        for ev in events.try_iter() {
            match ev {
                Ev::Loaded(files) => {
                    self.items = files.into_iter().map(|rel| Item::new(&root, rel)).collect();
                    self.loaded = true;
                    self.status = String::new();
                    changed = true;
                }
                Ev::Added(rel) => {
                    self.items.push(Item::new(&root, rel));
                    changed = true;
                }
                Ev::Progress(done, total) => self.progress = Some((done, total)),
                Ev::Done(msg) => {
                    self.progress = None;
                    self.status = msg;
                }
                Ev::Status(msg) => self.status = msg,
            }
        }
        if changed {
            self.items.sort_by(|a, b| a.key().cmp(&b.key()));
            let videos = self.items.iter().filter(|i| i.video).count();
            self.counts = (self.items.len() - videos, videos);
            self.grid = None;
        }
    }

    fn receive_thumbs(&mut self, ctx: &Context) {
        let mut failures = Vec::new();
        for (rel, image) in self.loader.ready() {
            let thumb = match image {
                Ok(img) => Thumb::Ready(ctx.load_texture(&rel, img, TextureOptions::LINEAR)),
                Err(e) => {
                    failures.push(format!("{rel}\t{e}"));
                    Thumb::Missing
                }
            };
            self.thumbs.insert(rel, thumb);
            self.quiet_frames = 0;
        }
        self.log_failures(failures);
    }

    /// Garde une trace des aperçus impossibles dans `.pellicule/apercus.log` (pour comprendre pourquoi).
    fn log_failures(&mut self, failures: Vec<String>) {
        if failures.is_empty() {
            return;
        }
        self.thumb_failures += failures.len();
        if let Some(root) = &self.root {
            let log = root.join(".pellicule").join("apercus.log");
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                use std::io::Write;
                for line in &failures {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        if self.progress.is_none() {
            self.status = format!(
                "{} aperçu{} impossible{} (détails dans .pellicule{}apercus.log)",
                self.thumb_failures,
                plural(self.thumb_failures),
                plural(self.thumb_failures),
                std::path::MAIN_SEPARATOR
            );
        }
    }

    fn add_folder_dialog(&mut self) {
        if let Some(dirs) = rfd::FileDialog::new().set_title("Ajouter des photos et vidéos").pick_folders() {
            self.import(dirs);
        }
    }

    // -------------------------------------------------------------- écrans

    fn setup_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 140.0).max(20.0));
                ui.heading(egui::RichText::new("Bienvenue dans Pellicule").size(30.0));
                ui.add_space(8.0);
                ui.label("Toutes tes photos et vidéos au même endroit, classées par date.");
                ui.add_space(28.0);
                ui.label("Où veux-tu créer ton album ?");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = 460.0_f32.min(ui.available_width() - 120.0);
                    ui.add_space(((ui.available_width() - w - 110.0) / 2.0).max(0.0));
                    ui.add(egui::TextEdit::singleline(&mut self.setup_path).desired_width(w));
                    if ui.button("Parcourir…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            let dir = if dir.ends_with("Pellicule") { dir } else { dir.join("Pellicule") };
                            self.setup_path = dir.display().to_string();
                        }
                    }
                });
                ui.add_space(18.0);
                let create = ui.add(egui::Button::new(egui::RichText::new("Créer mon album").size(17.0)).min_size(vec2(200.0, 38.0)));
                if create.clicked() && !self.setup_path.trim().is_empty() {
                    let root = config::absolute(Path::new(self.setup_path.trim().trim_matches('"')));
                    if let Err(e) = config::save_album(&root) {
                        self.status = format!("Choix non mémorisé : {e}");
                    }
                    self.start(ui.ctx(), root);
                }
            });
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("barre").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("Pellicule").strong());
                ui.add_space(10.0);
                let (photos, videos) = self.counts;
                ui.weak(format!("{} photo{} · {} vidéo{}", spaced(photos), plural(photos), spaced(videos), plural(videos)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Ouvrir le dossier").clicked() {
                        if let Some(root) = &self.root {
                            platform::open(root);
                        }
                    }
                    if ui.button("+  Ajouter des photos…").clicked() {
                        self.add_folder_dialog();
                    }
                    self.update_button(ui);
                    ui.add_space(12.0);
                    ui.add(egui::Slider::new(&mut self.size, 90.0..=320.0).show_value(false));
                    ui.weak("Taille");
                });
            });
            if let Some((done, total)) = self.progress {
                ui.add_space(4.0);
                ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).text(format!("Import {} / {}", done + 1, total)));
            }
            ui.add_space(6.0);
        });
    }

    fn update_button(&mut self, ui: &mut egui::Ui) {
        match self.update.clone() {
            update::State::Available(version) => {
                let label = egui::RichText::new(format!("Mettre à jour (v{version})")).color(Color32::WHITE);
                let button = egui::Button::new(label).fill(ui.visuals().selection.bg_fill);
                // Pas pendant un import : on ne coupe pas l'appli au milieu d'une copie.
                let resp = ui.add_enabled(self.progress.is_none(), button).on_hover_text("Télécharge la dernière version sur GitHub et relance Pellicule");
                if resp.clicked() {
                    if cfg!(windows) {
                        self.updates = Some(update::install_in_background(ui.ctx().clone()));
                        self.update = update::State::Installing;
                    } else {
                        platform::open(Path::new(update::REPO));
                    }
                }
            }
            update::State::Installing => {
                ui.label("Mise à jour…");
                ui.spinner();
            }
            update::State::Failed(e) => {
                if ui.button("Réessayer la mise à jour").on_hover_text(&e).clicked() {
                    self.updates = Some(update::install_in_background(ui.ctx().clone()));
                    self.update = update::State::Installing;
                }
                ui.colored_label(ui.visuals().error_fg_color, "Mise à jour échouée").on_hover_text(e);
            }
            update::State::Unknown | update::State::UpToDate => {}
        }
        ui.add_space(8.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("etat").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.weak(format!("v{}", update::CURRENT));
                    ui.add_space(12.0);
                    if ui.link(format!("Ouvrir « {} »", album::INBOX)).clicked() {
                        if let Some(root) = &self.root {
                            let inbox = root.join(album::INBOX);
                            let _ = std::fs::create_dir_all(&inbox);
                            platform::open(&inbox);
                        }
                    }
                    ui.weak("Glisse des photos dans cette fenêtre ou dans");
                });
            });
        });
    }

    fn empty_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
            if !self.loaded {
                ui.spinner();
                return;
            }
            ui.heading("Ta pellicule est vide");
            ui.add_space(8.0);
            ui.label("Glisse ici tes dossiers de photos et vidéos (téléphone, clé USB, disque dur…).");
            ui.label("Elles sont copiées, renommées par date, et les doublons sont ignorés.");
            ui.add_space(18.0);
            if ui.add(egui::Button::new("+  Ajouter des photos…").min_size(vec2(200.0, 36.0))).clicked() {
                self.add_folder_dialog();
            }
        });
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let cols = (((width - GAP) / (self.size + GAP)).floor() as usize).max(1);
        // Les vignettes s'étirent pour remplir toute la largeur.
        let cell = (width - GAP) / cols as f32;
        if self.grid.as_ref().is_none_or(|g| g.cols != cols || (g.cell - cell).abs() > 0.5) {
            self.grid = Some(Grid::build(&self.items, cols, cell));
        }
        let Self { grid, items, thumbs, loader, .. } = self;
        let grid = grid.as_ref().unwrap();

        egui::ScrollArea::vertical().auto_shrink(false).show_viewport(ui, |ui, view| {
            ui.set_height(grid.height);
            let origin = ui.min_rect().min;
            let painter = ui.painter().clone();
            let visuals = ui.visuals().clone();
            let side = grid.cell - GAP;

            for (y, _, row) in grid.rows_in(view.min.y, view.max.y) {
                match row {
                    Row::Header { label, count } => {
                        let pos = origin + vec2(GAP + 2.0, y + HEADER - 12.0);
                        let r = painter.text(pos, Align2::LEFT_BOTTOM, label, FontId::proportional(20.0), visuals.strong_text_color());
                        painter.text(r.right_bottom() + vec2(10.0, -2.0), Align2::LEFT_BOTTOM, spaced(*count), FontId::proportional(14.0), visuals.weak_text_color());
                    }
                    Row::Thumbs { start, end } => {
                        for (col, idx) in (*start..*end).enumerate() {
                            let item = &items[idx];
                            let rect = Rect::from_min_size(origin + vec2(GAP + col as f32 * grid.cell, *y), Vec2::splat(side));
                            draw_thumb(ui, &painter, &visuals, rect, item, thumbs.get(&item.rel));
                        }
                    }
                }
            }

            // Ce qu'il faut charger : l'écran d'abord, puis un écran au-dessus et en dessous.
            let margin = view.height();
            let wanted = |top: f32, bottom: f32| {
                grid.rows_in(top, bottom).flat_map(|(_, _, row)| match row {
                    Row::Thumbs { start, end } => *start..*end,
                    Row::Header { .. } => 0..0,
                })
            };
            let near: Vec<usize> = wanted(view.min.y, view.max.y)
                .chain(wanted(view.max.y, view.max.y + margin))
                .chain(wanted(view.min.y - margin, view.min.y))
                .collect();
            loader.want(
                near.iter()
                    .map(|&i| &items[i])
                    .filter(|it| !thumbs.contains_key(&it.rel))
                    .map(|it| (it.rel.clone(), it.path.clone()))
                    .collect(),
            );
            if thumbs.len() > MAX_TEXTURES {
                let keep: HashSet<&str> = near.iter().map(|&i| items[i].rel.as_str()).collect();
                thumbs.retain(|rel, _| keep.contains(rel.as_str()));
            }
        });
    }
}

fn draw_thumb(ui: &egui::Ui, painter: &egui::Painter, visuals: &egui::Visuals, rect: Rect, item: &Item, thumb: Option<&Thumb>) {
    let radius = CornerRadius::same(4);
    let resp = ui.interact(rect, Id::unique(("vignette", &item.rel)), Sense::click());
    match thumb {
        Some(Thumb::Ready(tex)) => {
            egui::Image::new((tex.id(), rect.size())).uv(cover_uv(tex.size_vec2())).corner_radius(radius).paint_at(ui, rect);
        }
        Some(Thumb::Missing) => {
            painter.rect_filled(rect, radius, visuals.faint_bg_color);
            let ext = item.path.extension().unwrap_or_default().to_string_lossy().to_uppercase();
            painter.text(rect.center(), Align2::CENTER_CENTER, ext, FontId::proportional(15.0), visuals.weak_text_color());
        }
        None => {
            painter.rect_filled(rect, radius, visuals.faint_bg_color);
        }
    }
    if item.video {
        let c = rect.left_bottom() + vec2(18.0, -18.0);
        painter.circle_filled(c, 12.0, Color32::from_black_alpha(150));
        painter.text(c + vec2(1.0, 0.0), Align2::CENTER_CENTER, "▶", FontId::proportional(12.0), Color32::WHITE);
    }
    if resp.hovered() {
        painter.rect_stroke(rect, radius, Stroke::new(2.5, visuals.selection.bg_fill), StrokeKind::Inside);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        platform::open(&item.path);
    }
    resp.context_menu(|ui| {
        if ui.button("Ouvrir").clicked() {
            platform::open(&item.path);
            ui.close();
        }
        if ui.button("Afficher dans le dossier").clicked() {
            platform::reveal(&item.path);
            ui.close();
        }
    });
    resp.on_hover_text(date_label(&item.rel).unwrap_or_else(|| item.rel.clone()));
}

/// Recadre au centre pour remplir un carré (comme la pellicule d'un téléphone).
fn cover_uv(size: Vec2) -> Rect {
    if size.x > size.y {
        let w = size.y / size.x;
        Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
    } else {
        let h = size.x / size.y.max(1.0);
        Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
    }
}

fn plural(n: usize) -> &'static str {
    if n > 1 { "s" } else { "" }
}

/// 12345 → « 12 345 ».
fn spaced(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_events();
        self.receive_thumbs(&ctx);
        if let Some(state) = self.updates.as_ref().and_then(|rx| rx.try_iter().last()) {
            self.update = state;
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            if self.root.is_some() {
                self.import(dropped);
            } else {
                self.pending.extend(dropped);
            }
        }

        if self.root.is_none() {
            self.setup_screen(ui);
            self.maybe_capture(&ctx);
            return;
        }

        self.top_bar(ui);
        self.status_bar(ui);
        egui::CentralPanel::default_margins().frame(egui::Frame::central_panel(ui.style()).inner_margin(0)).show(ui, |ui| {
            if self.items.is_empty() {
                self.empty_screen(ui);
            } else {
                self.gallery(ui);
            }
        });

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("depot")));
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(190));
            painter.text(screen.center(), Align2::CENTER_CENTER, "Lâche pour ajouter à ta pellicule", FontId::proportional(28.0), Color32::WHITE);
        }

        self.maybe_capture(&ctx);
    }
}

impl App {
    /// Pour le développement : `PELLICULE_CAPTURE=ecran.png` enregistre une capture de la
    /// fenêtre une fois les miniatures chargées, puis quitte.
    fn maybe_capture(&mut self, ctx: &Context) {
        let Some(path) = self.capture.clone() else { return };
        self.quiet_frames += 1;
        ctx.request_repaint_after(Duration::from_millis(50));
        if (self.loaded || self.root.is_none()) && self.progress.is_none() && self.quiet_frames == 40 {
            ctx.request_screenshot(move |img| {
                save_capture(&path, &img);
                std::process::exit(0);
            });
        }
    }
}

#[cfg(not(windows))]
fn save_capture(path: &Path, img: &egui::ColorImage) {
    let raw: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    if let Some(buf) = image::RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, raw) {
        let _ = buf.save(path);
    }
}

#[cfg(windows)]
fn save_capture(_path: &Path, _img: &egui::ColorImage) {}
