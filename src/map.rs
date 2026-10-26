//! Le mode carte : les photos regroupées par lieu sur une carte OpenStreetMap.
//!
//! Les tuiles (images de 256 px) sont téléchargées au besoin puis gardées dans
//! `.pellicule/carte`, pour ne pas les redemander (et pouvoir naviguer hors ligne).

use crate::gui::{draw_thumb, Item, Thumb};
use crate::place::LatLon;
use eframe::egui::{
    self, Color32, ColorImage, Context, CornerRadius, FontId, Id, Pos2, Rect, Sense, Stroke,
    TextureHandle, TextureOptions, Vec2, pos2, vec2,
};
use std::collections::{HashMap, HashSet};
use std::f64::consts::PI;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

const TILE: f64 = 256.0;
const MAX_ZOOM: f64 = 19.0;
const MIN_ZOOM: f64 = 1.0;
/// Taille d'une bulle de photos, et distance en dessous de laquelle deux photos sont regroupées.
const BUBBLE: f32 = 62.0;
/// Tuiles gardées en mémoire au maximum.
const MAX_TILES: usize = 220;
const USER_AGENT: &str = concat!("Pellicule/", env!("CARGO_PKG_VERSION"), " (+https://github.com/valentinfigueiredo/pellicule)");

type TileId = (u8, u32, u32);

enum TileState {
    Loading,
    Ready(TextureHandle, u64),
    Failed(Instant),
}

#[derive(Default)]
struct Queue {
    todo: Vec<TileId>,
    busy: HashSet<TileId>,
}

struct Tiles {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    results: Receiver<(TileId, Option<ColorImage>)>,
    state: HashMap<TileId, TileState>,
}

impl Tiles {
    fn new(cache: PathBuf, ctx: Context) -> Tiles {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (tx, results) = mpsc::channel();
        // Deux connexions au plus, comme le demande OpenStreetMap.
        for _ in 0..2 {
            let (queue, tx, ctx, cache) = (queue.clone(), tx.clone(), ctx.clone(), cache.clone());
            std::thread::spawn(move || tile_worker(queue, tx, ctx, cache));
        }
        Tiles { queue, results, state: HashMap::new() }
    }
}

fn tile_worker(queue: Arc<(Mutex<Queue>, Condvar)>, tx: Sender<(TileId, Option<ColorImage>)>, ctx: Context, cache: PathBuf) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let (lock, cv) = &*queue;
    loop {
        let id = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(id) = q.todo.pop() {
                    q.busy.insert(id);
                    break id;
                }
                q = cv.wait(q).unwrap();
            }
        };
        let image = load_tile(&agent, &cache, id);
        lock.lock().unwrap().busy.remove(&id);
        if tx.send((id, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn load_tile(agent: &ureq::Agent, cache: &std::path::Path, (z, x, y): TileId) -> Option<ColorImage> {
    let path = cache.join(z.to_string()).join(x.to_string()).join(format!("{y}.png"));
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < Duration::from_secs(60 * 86400));
    let bytes = if fresh {
        std::fs::read(&path).ok()
    } else {
        let url = format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png");
        let downloaded = agent
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .call()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| r.into_body().with_config().limit(4 << 20).read_to_vec().ok());
        match downloaded {
            Some(b) => {
                if std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
                    let _ = std::fs::write(&path, &b);
                }
                Some(b)
            }
            // Hors ligne : une vieille tuile vaut mieux que rien.
            None => std::fs::read(&path).ok(),
        }
    }?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// Web Mercator : latitude/longitude → coordonnées du monde entre 0 et 1.
fn project((lat, lon): LatLon) -> (f64, f64) {
    let lat = lat.clamp(-85.0511, 85.0511).to_radians();
    ((lon + 180.0) / 360.0, (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / PI) / 2.0)
}

pub struct MapView {
    tiles: Option<Tiles>,
    /// Centre de la vue, en coordonnées du monde (0..1).
    center: (f64, f64),
    zoom: f64,
    fitted: bool,
    frame: u64,
    /// Photos du lieu sélectionné (chemins relatifs, du plus récent au plus ancien).
    pub selection: Vec<String>,
    points: Vec<(f64, f64, usize)>,
    points_key: (usize, usize),
    /// Pour les captures de développement (`PELLICULE_CARTE=selection`).
    auto_select: bool,
}

impl Default for MapView {
    fn default() -> Self {
        MapView {
            tiles: None,
            center: project((46.5, 2.5)),
            zoom: 5.0,
            fitted: false,
            frame: 0,
            selection: Vec::new(),
            points: Vec::new(),
            points_key: (usize::MAX, 0),
            auto_select: std::env::var("PELLICULE_CARTE").is_ok_and(|v| v == "selection"),
        }
    }
}

impl MapView {
    fn world_px(&self) -> f64 {
        TILE * 2f64.powf(self.zoom)
    }

    fn to_screen(&self, rect: Rect, (wx, wy): (f64, f64)) -> Pos2 {
        let w = self.world_px();
        rect.center() + vec2(((wx - self.center.0) * w) as f32, ((wy - self.center.1) * w) as f32)
    }

    fn to_world(&self, rect: Rect, p: Pos2) -> (f64, f64) {
        let w = self.world_px();
        let d = p - rect.center();
        (self.center.0 + d.x as f64 / w, self.center.1 + d.y as f64 / w)
    }

    /// Les photos localisées, projetées une fois pour toutes (recalculé quand la liste change).
    fn update_points(&mut self, items: &[Item], places: &HashMap<String, LatLon>) {
        let key = (items.len(), places.len());
        if key == self.points_key {
            return;
        }
        self.points_key = key;
        self.points = items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| places.get(&it.rel).map(|p| {
                let (x, y) = project(*p);
                (x, y, i)
            }))
            .collect();
    }

    fn fit(&mut self, rect: Rect) {
        if self.points.is_empty() {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (1.0f64, 1.0f64, 0.0f64, 0.0f64);
        for &(x, y, _) in &self.points {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        self.center = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let span = ((x1 - x0) / (rect.width() as f64 * 0.8)).max((y1 - y0) / (rect.height() as f64 * 0.8)).max(1e-9);
        self.zoom = (1.0 / span / TILE).log2().clamp(MIN_ZOOM, 15.0);
        self.fitted = true;
    }

    /// Le panneau de droite : les photos du lieu choisi.
    pub fn side_panel(&mut self, ui: &mut egui::Ui, items: &[Item], thumbs_map: &HashMap<String, Thumb>, wanted: &mut Vec<(String, PathBuf)>) {
        if self.selection.is_empty() {
            return;
        }
        let by_rel: HashMap<&str, &Item> = items.iter().map(|i| (i.rel.as_str(), i)).collect();
        let chosen: Vec<&Item> = self.selection.iter().filter_map(|r| by_rel.get(r.as_str()).copied()).collect();
        let mut close = false;
        egui::Panel::right("lieu").resizable(false).exact_size(3.0 * 112.0 + 24.0).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading(format!("{} photo{}", chosen.len(), if chosen.len() > 1 { "s" } else { "" }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Fermer").clicked() {
                        close = true;
                    }
                });
            });
            if let (Some(last), Some(first)) = (chosen.first(), chosen.last()) {
                let (a, b) = (crate::gui::month_label(&first.month), crate::gui::month_label(&last.month));
                ui.weak(if a == b { a } else { format!("{a} – {b}") });
            }
            ui.add_space(6.0);
            let side = 108.0;
            let rows = chosen.len().div_ceil(3);
            egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, side + 4.0, rows, |ui, range| {
                for row in range {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), side + 4.0), Sense::hover());
                    let painter = ui.painter().clone();
                    let visuals = ui.visuals().clone();
                    for col in 0..3 {
                        let Some(item) = chosen.get(row * 3 + col) else { break };
                        let r = Rect::from_min_size(rect.min + vec2(col as f32 * (side + 4.0), 0.0), Vec2::splat(side));
                        if !thumbs_map.contains_key(&item.rel) {
                            wanted.push((item.rel.clone(), item.path.clone()));
                        }
                        draw_thumb(ui, &painter, &visuals, r, item, thumbs_map.get(&item.rel));
                    }
                }
            });
        });
        if close {
            self.selection.clear();
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        album_root: &std::path::Path,
        items: &[Item],
        places: &HashMap<String, LatLon>,
        thumbs_map: &HashMap<String, Thumb>,
        wanted: &mut Vec<(String, PathBuf)>,
        scanning: bool,
    ) {
        self.frame += 1;
        let ctx = ui.ctx().clone();
        if self.tiles.is_none() {
            self.tiles = Some(Tiles::new(album_root.join(".pellicule").join("carte"), ctx.clone()));
        }
        self.update_points(items, places);
        let rect = ui.available_rect_before_wrap();
        if !self.fitted && !self.points.is_empty() {
            self.fit(rect);
        }

        // Déplacement et zoom.
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        if resp.dragged() {
            let d = resp.drag_delta();
            let w = self.world_px();
            self.center.0 -= d.x as f64 / w;
            self.center.1 -= d.y as f64 / w;
        }
        if let Some(cursor) = resp.hover_pos() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let mut dz = scroll as f64 / 250.0 + (pinch as f64).log2();
            if resp.double_clicked() {
                dz += 1.0;
            }
            if dz != 0.0 {
                self.zoom_at(rect, cursor, dz);
            }
        }
        self.center.1 = self.center.1.clamp(0.0, 1.0);
        self.center.0 = self.center.0.rem_euclid(1.0);

        let painter = ui.painter_at(rect);
        let visuals = ui.visuals().clone();
        // Couleur de fond d'OpenStreetMap, en attendant les tuiles.
        painter.rect_filled(rect, 0.0, if visuals.dark_mode { Color32::from_gray(45) } else { Color32::from_rgb(232, 229, 223) });
        self.draw_tiles(&painter, rect, &ctx);
        self.draw_bubbles(ui, &painter, rect, items, thumbs_map, wanted);
        if self.points.is_empty() {
            let text = if scanning { "Recherche des photos avec une position GPS…" } else { "Aucune photo avec une position GPS pour l'instant." };
            let galley = painter.layout_no_wrap(text.into(), FontId::proportional(18.0), Color32::from_gray(30));
            let r = Rect::from_center_size(rect.center(), galley.size() + vec2(32.0, 20.0));
            painter.rect_filled(r, 10.0, Color32::from_white_alpha(230));
            painter.galley(r.center() - galley.size() / 2.0, galley, Color32::from_gray(30));
        }
        self.controls(ui, rect);
    }

    fn zoom_at(&mut self, rect: Rect, cursor: Pos2, dz: f64) {
        let before = self.to_world(rect, cursor);
        self.zoom = (self.zoom + dz).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.to_world(rect, cursor);
        self.center.0 += before.0 - after.0;
        self.center.1 += before.1 - after.1;
    }

    fn draw_tiles(&mut self, painter: &egui::Painter, rect: Rect, ctx: &Context) {
        let mut tiles = self.tiles.take().unwrap();
        self.draw_tiles_with(&mut tiles, painter, rect, ctx);
        self.tiles = Some(tiles);
    }

    fn draw_tiles_with(&self, tiles: &mut Tiles, painter: &egui::Painter, rect: Rect, ctx: &Context) {
        for (id, image) in tiles.results.try_iter() {
            let state = match image {
                Some(img) => TileState::Ready(ctx.load_texture(format!("tuile-{id:?}"), img, TextureOptions::LINEAR), self.frame),
                None => TileState::Failed(Instant::now()),
            };
            tiles.state.insert(id, state);
        }

        let z = self.zoom.round().clamp(0.0, MAX_ZOOM) as u8;
        let n = 1u32 << z;
        let w = self.world_px();
        let size = (w / n as f64) as f32;
        let tl = self.to_world(rect, rect.min);
        let br = self.to_world(rect, rect.max);
        let (x0, x1) = ((tl.0 * n as f64).floor() as i64, (br.0 * n as f64).floor() as i64);
        let (y0, y1) = ((tl.1 * n as f64).floor().max(0.0) as i64, ((br.1 * n as f64).floor() as i64).min(n as i64 - 1));

        let mut wanted = Vec::new();
        for ty in y0..=y1 {
            for tx in x0..=x1 {
                let wrapped = tx.rem_euclid(n as i64) as u32;
                let pos = self.to_screen(rect, (tx as f64 / n as f64, ty as f64 / n as f64));
                let r = Rect::from_min_size(pos, Vec2::splat(size + 0.5));
                let id = (z, wrapped, ty as u32);
                match tiles.state.get_mut(&id) {
                    Some(TileState::Ready(tex, used)) => {
                        *used = self.frame;
                        painter.image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        continue;
                    }
                    Some(TileState::Failed(when)) if when.elapsed() < Duration::from_secs(30) => {}
                    Some(TileState::Loading) => {}
                    _ => wanted.push(id),
                }
                // En attendant : une tuile plus large, agrandie.
                for dz in 1..=4u8.min(z) {
                    let parent = (z - dz, wrapped >> dz, ty as u32 >> dz);
                    if let Some(TileState::Ready(tex, used)) = tiles.state.get_mut(&parent) {
                        *used = self.frame;
                        let k = 1u32 << dz;
                        let (fx, fy) = ((wrapped % k) as f32 / k as f32, (ty as u32 % k) as f32 / k as f32);
                        let uv = Rect::from_min_size(pos2(fx, fy), Vec2::splat(1.0 / k as f32));
                        painter.image(tex.id(), r, uv, Color32::WHITE);
                        break;
                    }
                }
            }
        }
        for id in &wanted {
            tiles.state.insert(*id, TileState::Loading);
        }
        {
            let (lock, cv) = &*tiles.queue;
            let mut q = lock.lock().unwrap();
            // Les tuiles qui ne sont plus à l'écran ne sont plus demandées.
            for id in q.todo.drain(..) {
                if matches!(tiles.state.get(&id), Some(TileState::Loading)) {
                    tiles.state.remove(&id);
                }
            }
            q.todo = wanted.into_iter().rev().filter(|id| !q.busy.contains(id)).collect();
            for id in &q.todo {
                tiles.state.insert(*id, TileState::Loading);
            }
            cv.notify_all();
        }

        let ready = tiles.state.values().filter(|s| matches!(s, TileState::Ready(..))).count();
        if ready > MAX_TILES {
            let mut used: Vec<u64> = tiles.state.values().filter_map(|s| match s { TileState::Ready(_, u) => Some(*u), _ => None }).collect();
            used.sort_unstable();
            let cutoff = used[ready - MAX_TILES * 3 / 4];
            tiles.state.retain(|_, s| !matches!(s, TileState::Ready(_, u) if *u < cutoff));
        }
    }

    fn draw_bubbles(
        &mut self,
        ui: &egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        items: &[Item],
        thumbs_map: &HashMap<String, Thumb>,
        wanted: &mut Vec<(String, PathBuf)>,
    ) {
        // Regroupement sur une grille fixe dans le monde : les bulles ne sautent pas quand on déplace la carte.
        let w = self.world_px();
        let cell = BUBBLE as f64 * 1.1;
        let mut groups: HashMap<(i64, i64), (f64, f64, Vec<usize>)> = HashMap::new();
        for &(x, y, i) in &self.points {
            let key = ((x * w / cell).floor() as i64, (y * w / cell).floor() as i64);
            let g = groups.entry(key).or_insert((0.0, 0.0, Vec::new()));
            g.0 += x;
            g.1 += y;
            g.2.push(i);
        }
        let mut bubbles: Vec<(Pos2, Vec<usize>)> = groups
            .into_values()
            .map(|(sx, sy, members)| {
                let n = members.len() as f64;
                (self.to_screen(rect, (sx / n, sy / n)), members)
            })
            .filter(|(p, _)| rect.expand(BUBBLE).contains(*p))
            .collect();
        // Les plus petits groupes d'abord : les gros restent au-dessus.
        bubbles.sort_by_key(|(_, m)| m.len());
        if self.auto_select && self.selection.is_empty() {
            if let Some((_, m)) = bubbles.last() {
                self.selection = m.iter().map(|&i| items[i].rel.clone()).collect();
            }
        }

        let radius = BUBBLE / 2.0;
        for (center, mut members) in bubbles {
            members.sort_unstable();
            let cover = &items[members[0]]; // le plus récent
            let r = Rect::from_center_size(center, Vec2::splat(BUBBLE));
            painter.circle_filled(center + vec2(0.0, 2.0), radius + 3.0, Color32::from_black_alpha(60));
            painter.circle_filled(center, radius + 3.0, Color32::WHITE);
            match thumbs_map.get(&cover.rel) {
                Some(Thumb::Ready(tex)) => {
                    egui::Image::new((tex.id(), r.size()))
                        .uv(crate::gui::cover_uv(tex.size_vec2()))
                        .corner_radius(CornerRadius::same(radius as u8))
                        .paint_at(ui, r);
                }
                Some(Thumb::Missing) => {
                    painter.circle_filled(center, radius, Color32::from_gray(200));
                }
                None => {
                    painter.circle_filled(center, radius, Color32::from_gray(220));
                    wanted.push((cover.rel.clone(), cover.path.clone()));
                }
            }
            if members.len() > 1 {
                let text = crate::gui::spaced(members.len());
                let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), Color32::WHITE);
                let pill = Rect::from_center_size(
                    center + vec2(radius * 0.75, -radius * 0.75),
                    vec2(galley.size().x + 12.0, 20.0),
                );
                painter.rect_filled(pill, 10.0, ui.visuals().selection.bg_fill);
                painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
            }
            let id = Id::unique(("bulle", members[0]));
            let resp = ui.interact(r, id, Sense::click());
            if resp.hovered() {
                painter.circle_stroke(center, radius + 3.0, Stroke::new(3.0, ui.visuals().selection.bg_fill));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                self.selection = members.iter().map(|&i| items[i].rel.clone()).collect();
            }
            if resp.double_clicked() {
                self.zoom_at(rect, center, 2.0);
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, rect: Rect) {
        // Boutons de zoom en haut à droite.
        let size = vec2(34.0, 34.0);
        let plus = Rect::from_min_size(rect.right_top() + vec2(-46.0, 12.0), size);
        let minus = plus.translate(vec2(0.0, 38.0));
        let fit = Rect::from_min_size(minus.right_bottom() + vec2(-54.0, 12.0), vec2(54.0, 30.0));
        for (r, label, action) in [(plus, "+", 1.0), (minus, "−", -1.0), (fit, "Tout", 0.0)] {
            let size = if action == 0.0 { 14.0 } else { 18.0 };
            let resp = ui.put(r, egui::Button::new(egui::RichText::new(label).size(size)));
            if resp.clicked() {
                if action == 0.0 {
                    self.fit(rect);
                } else {
                    self.zoom_at(rect, rect.center(), action);
                }
            }
        }
        let fit_resp = ui.interact(fit, Id::unique("carte-tout"), Sense::hover());
        fit_resp.on_hover_text("Voir toutes les photos");

        // Mention obligatoire des contributeurs OpenStreetMap.
        let text = "© les contributeurs d'OpenStreetMap";
        let galley = ui.painter().layout_no_wrap(text.into(), FontId::proportional(11.0), Color32::from_gray(40));
        let r = Rect::from_min_size(rect.right_bottom() - galley.size() - vec2(12.0, 6.0), galley.size() + vec2(8.0, 4.0));
        ui.painter().rect_filled(r, 3.0, Color32::from_white_alpha(200));
        ui.painter().galley(r.min + vec2(4.0, 2.0), galley, Color32::from_gray(40));
        if ui.interact(r, Id::unique("osm"), Sense::click()).clicked() {
            crate::platform::open(std::path::Path::new("https://www.openstreetmap.org/copyright"));
        }
    }
}
