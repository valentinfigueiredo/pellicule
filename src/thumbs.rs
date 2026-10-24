//! Chargement des miniatures en arrière-plan.
//!
//! Sous Windows on demande les miniatures à l'Explorateur (IShellItemImageFactory) :
//! elles sont déjà en cache, gèrent HEIC, RAW et vidéos, et respectent l'orientation.
//! Ailleurs (pour développer), on décode les images nous-mêmes.

use eframe::egui::{ColorImage, Context};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

/// Taille (en pixels) demandée pour les miniatures.
pub const SIZE: u32 = 256;

#[derive(Default)]
struct Queue {
    /// Les plus urgentes à la fin.
    todo: Vec<(String, PathBuf)>,
    busy: HashSet<String>,
}

pub struct Loader {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    results: Receiver<(String, Option<ColorImage>)>,
}

impl Loader {
    pub fn new(ctx: Context) -> Loader {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (tx, results) = mpsc::channel();
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 6);
        for _ in 0..workers {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || worker(queue, tx, ctx));
        }
        Loader { queue, results }
    }

    /// Remplace la file d'attente par ce qui est visible maintenant (le plus urgent en premier).
    pub fn want(&self, items: impl DoubleEndedIterator<Item = (String, PathBuf)>) {
        let (lock, cv) = &*self.queue;
        let mut q = lock.lock().unwrap();
        let todo: Vec<_> = items.rev().filter(|(rel, _)| !q.busy.contains(rel)).collect();
        q.todo = todo;
        cv.notify_all();
    }

    pub fn ready(&self) -> impl Iterator<Item = (String, Option<ColorImage>)> + '_ {
        self.results.try_iter()
    }
}

fn worker(queue: Arc<(Mutex<Queue>, Condvar)>, tx: Sender<(String, Option<ColorImage>)>, ctx: Context) {
    imp::init_thread();
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
        let image = imp::load(&path, SIZE);
        lock.lock().unwrap().busy.remove(&rel);
        if tx.send((rel, image)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

#[cfg(windows)]
mod imp {
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
    use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};

    pub fn init_thread() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        }
    }

    pub fn load(path: &Path, size: u32) -> Option<ColorImage> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>).ok()?;
            let bitmap = factory.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_RESIZETOFIT).ok()?;
            let image = to_image(bitmap);
            let _ = DeleteObject(bitmap.into());
            image
        }
    }

    unsafe fn to_image(bitmap: HBITMAP) -> Option<ColorImage> {
        let mut bm = BITMAP::default();
        if GetObjectW(bitmap.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return None;
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
            return None;
        }
        // BGRA → RGBA. Les photos arrivent souvent avec une transparence à 0 : on les rend opaques.
        let no_alpha = px.chunks_exact(4).all(|p| p[3] == 0);
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
            if no_alpha {
                p[3] = 255;
            }
        }
        Some(ColorImage::from_rgba_premultiplied([w as usize, h as usize], &px))
    }
}

#[cfg(not(windows))]
mod imp {
    use eframe::egui::ColorImage;
    use image::{DynamicImage, ImageDecoder, ImageReader};
    use std::path::Path;

    pub fn init_thread() {}

    pub fn load(path: &Path, size: u32) -> Option<ColorImage> {
        let mut decoder = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
        let orientation = decoder.orientation().ok();
        let mut img = DynamicImage::from_decoder(decoder).ok()?;
        if let Some(o) = orientation {
            img.apply_orientation(o);
        }
        let t = img.thumbnail(size, size).to_rgba8();
        Some(ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw()))
    }
}
