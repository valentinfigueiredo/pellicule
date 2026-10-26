//! Chargement des miniatures en arrière-plan.
//!
//! Pellicule décode elle-même les formats courants (JPEG, PNG, WebP, GIF, BMP, TIFF),
//! les HEIC, les RAW et les vidéos MP4/MOV (voir `preview.rs`).
//! Pour le reste (AVI, WMV, MKV…), on demande la miniature à Windows
//! (IShellItemImageFactory), qui s'appuie sur les codecs installés.

use eframe::egui::{ColorImage, Context};
use image::DynamicImage;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

/// Taille (en pixels) demandée pour les miniatures.
pub const SIZE: u32 = 256;

pub type Loaded = (String, Result<ColorImage, String>);

#[derive(Default)]
struct Queue {
    /// Les plus urgentes à la fin.
    todo: Vec<(String, PathBuf)>,
    busy: HashSet<String>,
}

type Shared = Arc<(Mutex<Queue>, Condvar)>;

/// Deux équipes de travail : l'une décode les formats courants (fiable et indépendante
/// de Windows), l'autre demande à l'Explorateur les miniatures des HEIC, RAW et vidéos.
/// Ainsi un codec Windows lent ou bloqué ne bloque jamais les photos JPEG.
pub struct Loader {
    decode: Shared,
    shell: Shared,
    results: Receiver<Loaded>,
    cache: Cache,
}

/// Dossier des miniatures déjà calculées (`.pellicule/miniatures`), connu une fois l'album ouvert.
type Cache = Arc<Mutex<Option<PathBuf>>>;

impl Loader {
    pub fn new(ctx: Context) -> Loader {
        let (tx, results) = mpsc::channel();
        let cache: Cache = Arc::new(Mutex::new(None));
        let spawn = |n: usize, via_shell: bool| {
            let q: Shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
            for _ in 0..n {
                let (q, tx, ctx, cache) = (q.clone(), tx.clone(), ctx.clone(), cache.clone());
                std::thread::spawn(move || worker(q, tx, ctx, via_shell, cache));
            }
            q
        };
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let decode = spawn(cpus.clamp(2, 6), false);
        let shell = spawn(if cfg!(windows) { 2 } else { 0 }, true);
        Loader { decode, shell, results, cache }
    }

    pub fn set_cache(&self, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        *self.cache.lock().unwrap() = Some(dir);
    }

    /// Remplace les files d'attente par ce qui est visible maintenant (le plus urgent en premier).
    pub fn want(&self, items: Vec<(String, PathBuf)>) {
        let (shell, decode): (Vec<_>, Vec<_>) =
            items.into_iter().partition(|(_, p)| cfg!(windows) && !decodable(p));
        for (q, items) in [(&self.decode, decode), (&self.shell, shell)] {
            let (lock, cv) = &**q;
            let mut q = lock.lock().unwrap();
            q.todo = items.into_iter().rev().filter(|(rel, _)| !q.busy.contains(rel)).collect();
            cv.notify_all();
        }
    }

    pub fn ready(&self) -> impl Iterator<Item = Loaded> + '_ {
        self.results.try_iter()
    }
}

fn worker(queue: Shared, tx: Sender<Loaded>, ctx: Context, via_shell: bool, cache: Cache) {
    // Aussi pour l'équipe « décodage » : elle se rabat sur Windows si elle n'y arrive pas.
    init_thread();
    let (lock, cv) = &*queue;
    loop {
        let (rel, path) = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(item) = q.todo.pop() {
                    q.busy.insert(item.0.clone());
                    break item;
                }
                q = cv.wait(q).unwrap();
            }
        };
        let image = if via_shell {
            load_shell(&path)
        } else {
            let dir = cache.lock().unwrap().clone();
            load_own(&path, dir.as_deref())
        };
        lock.lock().unwrap().busy.remove(&rel);
        if tx.send((rel, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Formats qu'on sait décoder nous-mêmes.
fn decodable(path: &Path) -> bool {
    let ext = ext_of(path);
    matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff") || crate::preview::handles(&ext)
}

/// Nos propres décodeurs. Les HEIC, RAW et vidéos sont longs à décoder : leur miniature
/// est gardée sur le disque. Si on n'y arrive pas, on demande à Windows.
fn load_own(path: &Path, cache: Option<&Path>) -> Result<ColorImage, String> {
    let ext = ext_of(path);
    if !crate::preview::handles(&ext) {
        return decode(path, SIZE);
    }
    let cached = cache.and_then(|dir| {
        let meta = std::fs::metadata(path).ok()?;
        let stamp = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        let key = xxhash_rust::xxh3::xxh3_64(format!("{}|{}|{stamp}", path.display(), meta.len()).as_bytes());
        Some(dir.join(format!("{key:016x}.jpg")))
    });
    if let Some(img) = cached.as_ref().and_then(|c| image::open(c).ok()) {
        return Ok(to_color_image(img));
    }
    match crate::preview::decode(path, &ext) {
        Ok(img) => {
            let thumb = img.thumbnail(SIZE, SIZE);
            if let Some(c) = &cached {
                let _ = DynamicImage::ImageRgb8(thumb.to_rgb8()).save_with_format(c, image::ImageFormat::Jpeg);
            }
            Ok(to_color_image(thumb))
        }
        Err(own) => load_shell(path).map_err(|shell| if cfg!(windows) { format!("{own} ; Windows : {shell}") } else { own }),
    }
}

fn to_color_image(img: DynamicImage) -> ColorImage {
    let t = img.to_rgba8();
    ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw())
}

pub fn init_thread() {
    #[cfg(windows)]
    shell::init_thread();
}

#[cfg(windows)]
fn load_shell(path: &Path) -> Result<ColorImage, String> {
    shell::load(path, SIZE)
}

#[cfg(not(windows))]
fn load_shell(_path: &Path) -> Result<ColorImage, String> {
    Err("format non pris en charge".into())
}

/// Pour `pellicule apercu <fichier>` : essaie chaque méthode et dit ce qui se passe.
pub fn diagnose(path: &Path) -> Vec<(&'static str, Result<[usize; 2], String>)> {
    init_thread();
    let mut out = vec![("décodage Pellicule", load_own(path, None).map(|i| i.size))];
    if cfg!(windows) {
        out.push(("miniature Windows", load_shell(path).map(|i| i.size)));
    }
    out
}

fn decode(path: &Path, size: u32) -> Result<ColorImage, String> {
    use image::{ImageDecoder, ImageReader};
    let reader = ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let t = img.thumbnail(size, size).to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw()))
}

#[cfg(windows)]
mod shell {
    use eframe::egui::ColorImage;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    };
    use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_MULTITHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};

    pub fn init_thread() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
        }
    }

    pub fn load(path: &Path, size: u32) -> Result<ColorImage, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let factory: IShellItemImageFactory =
                SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>).map_err(|e| e.message())?;
            let bitmap = factory
                .GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_RESIZETOFIT)
                .map_err(|e| e.message())?;
            let image = to_image(bitmap);
            let _ = DeleteObject(bitmap.into());
            image
        }
    }

    unsafe fn to_image(bitmap: HBITMAP) -> Result<ColorImage, String> {
        let mut bm = BITMAP::default();
        if GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
            return Err("GetObject a échoué".into());
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return Err(format!("image vide ({w}×{h})"));
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // lignes de haut en bas
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        if lines == 0 {
            return Err("GetDIBits a échoué".into());
        }
        // BGRA → RGBA. Une photo sans transparence arrive souvent avec alpha à 0 partout :
        // on la rend opaque, sinon elle serait invisible.
        let no_alpha = px.chunks_exact(4).all(|p| p[3] == 0);
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
            if no_alpha {
                p[3] = 255;
            }
        }
        if no_alpha {
            Ok(ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px))
        } else {
            Ok(ColorImage::from_rgba_premultiplied([w as usize, h as usize], &px))
        }
    }
}
