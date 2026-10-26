//! Récupère les nouvelles photos et vidéos iCloud et les dépose dans « À trier » :
//! Pellicule les renomme, les range et écarte les doublons comme pour tout le reste.

use super::client::{Asset, Downloader, Error, Result, Session};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Téléchargements simultanés.
const PARALLEL: usize = 4;
/// Essais par fichier avant d'abandonner (il sera retenté à la prochaine synchronisation).
const ATTEMPTS: u32 = 3;

pub struct Progress {
    /// Éléments de la photothèque parcourus.
    pub seen: u64,
    pub total: u64,
    /// Nouveaux éléments téléchargés pendant cette synchronisation.
    pub downloaded: u64,
    /// Octets reçus pendant cette synchronisation.
    pub bytes: u64,
    /// Débit moyen sur les dernières secondes (octets/s).
    pub speed: u64,
}

pub struct Summary {
    pub downloaded: u64,
    pub errors: u64,
}

/// `state` : dossier de travail (`.pellicule/icloud`). `inbox` : la boîte « À trier ».
pub fn run(
    session: &mut Session,
    state: &Path,
    inbox: &Path,
    stop: &AtomicBool,
    report: &mut dyn FnMut(&Progress),
) -> Result<Summary> {
    let known_path = state.join("deja-recuperees.txt");
    let mut known: HashSet<String> = fs::File::open(&known_path)
        .map(|f| BufReader::new(f).lines().map_while(|l| l.ok()).collect())
        .unwrap_or_default();
    let mut log = OpenOptions::new().create(true).append(true).open(&known_path).map_err(|e| Error::Other(e.to_string()))?;
    let tmp = state.join("en-cours");
    let _ = fs::remove_dir_all(&tmp); // restes d'une synchronisation interrompue
    fs::create_dir_all(&tmp).map_err(|e| Error::Other(e.to_string()))?;
    fs::create_dir_all(inbox).map_err(|e| Error::Other(e.to_string()))?;

    let total = session.count()?;
    let downloader = session.downloader();
    let received = AtomicU64::new(0);
    let mut speed = Speed::default();
    let mut p = Progress { seen: 0, total, downloaded: 0, bytes: 0, speed: 0 };
    let mut errors = 0;
    report(&p);

    let mut offset = 0;
    while !stop.load(Ordering::Relaxed) {
        let page = session.page(offset)?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;
        let (new, already): (Vec<Asset>, Vec<Asset>) = page.into_iter().partition(|a| !known.contains(&a.id));
        p.seen += already.len() as u64;
        report(&p);

        let queue = Mutex::new(VecDeque::from(new));
        let pending = queue.lock().unwrap().len();
        let (tx, rx) = mpsc::channel::<(Asset, Result<PathBuf>)>();
        std::thread::scope(|scope| {
            for _ in 0..PARALLEL.min(pending) {
                let (tx, queue, downloader, received, tmp) = (tx.clone(), &queue, &downloader, &received, &tmp);
                scope.spawn(move || loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(asset) = queue.lock().unwrap().pop_front() else { return };
                    let result = fetch(downloader, &asset, tmp, received);
                    if tx.send((asset, result)).is_err() {
                        return;
                    }
                });
            }
            drop(tx);

            // Le fil principal range les fichiers terminés un par un (pas de collision de noms).
            loop {
                match rx.recv_timeout(Duration::from_millis(300)) {
                    Ok((asset, Ok(part))) => match finish(&asset, &part, inbox) {
                        Ok(()) => {
                            let _ = writeln!(log, "{}", asset.id);
                            known.insert(asset.id);
                            p.downloaded += 1;
                            p.seen += 1;
                        }
                        Err(_) => {
                            errors += 1;
                            p.seen += 1;
                        }
                    },
                    Ok((_, Err(_))) => {
                        errors += 1;
                        p.seen += 1;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                p.bytes = received.load(Ordering::Relaxed);
                p.speed = speed.update(p.bytes);
                report(&p);
            }
        });
    }
    let _ = fs::remove_dir_all(&tmp);
    Ok(Summary { downloaded: p.downloaded, errors })
}

/// Télécharge dans le dossier temporaire, avec plusieurs essais.
fn fetch(downloader: &Downloader, asset: &Asset, tmp: &Path, received: &AtomicU64) -> Result<PathBuf> {
    let part = tmp.join(format!("{}.part", safe_id(&asset.id)));
    let mut last = Error::Other(String::new());
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2 * attempt as u64));
        }
        match downloader.download(&asset.url, &part, &asset.filename, received) {
            Ok(n) if asset.size == 0 || n == asset.size => return Ok(part),
            Ok(_) => last = Error::Other(format!("{} : fichier incomplet", asset.filename)),
            Err(e) => last = e,
        }
        let _ = fs::remove_file(&part);
    }
    Err(last)
}

/// Date le fichier et le dépose dans « À trier » sous son nom d'origine.
fn finish(asset: &Asset, part: &Path, inbox: &Path) -> Result<()> {
    // La date iCloud sert de dernier recours quand le fichier n'a pas de date (captures d'écran…).
    if asset.date_ms > 0 {
        let when = UNIX_EPOCH + Duration::from_millis(asset.date_ms as u64);
        if let Ok(f) = fs::File::options().write(true).open(part) {
            let _ = f.set_modified(when.min(SystemTime::now()));
        }
    }
    let dest = free_path(inbox, &asset.filename);
    fs::rename(part, &dest).map_err(|e| Error::Other(e.to_string()))
}

/// Débit sur une fenêtre glissante d'une dizaine de secondes.
#[derive(Default)]
struct Speed {
    samples: VecDeque<(Instant, u64)>,
}

impl Speed {
    fn update(&mut self, bytes: u64) -> u64 {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > Duration::from_secs(10) {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let secs = now.duration_since(t0).as_secs_f64();
        if secs < 0.5 { 0 } else { ((bytes - b0) as f64 / secs) as u64 }
    }
}

fn safe_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

/// `IMG_1234.HEIC`, ou `IMG_1234_1.HEIC` si le nom est déjà pris dans « À trier ».
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "photo".into());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (0..)
        .map(|n| if n == 0 { dir.join(name) } else { dir.join(format!("{stem}_{n}{ext}")) })
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Télécharge 4 fois l'exe de Pellicule en parallèle et le range : `cargo test -- --ignored paralleles`.
    #[test]
    #[ignore]
    fn telechargements_paralleles() {
        let dir = std::env::temp_dir().join("pellicule-test-sync");
        let _ = fs::remove_dir_all(&dir);
        let (tmp, inbox) = (dir.join("tmp"), dir.join("inbox"));
        fs::create_dir_all(&tmp).unwrap();
        fs::create_dir_all(&inbox).unwrap();
        let session = Session::load(&dir);
        let downloader = session.downloader();
        let received = AtomicU64::new(0);
        let url = "https://raw.githubusercontent.com/valentinfigueiredo/pellicule/master/dist/pellicule.exe";
        let started = Instant::now();
        let parts: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|i| {
                    let asset = Asset { id: format!("id{i}"), filename: "pellicule.exe".into(), size: 0, url: url.into(), date_ms: 1_600_000_000_000 };
                    let (d, t, r) = (&downloader, &tmp, &received);
                    s.spawn(move || (fetch(d, &asset, t, r).unwrap(), asset))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (part, asset) in &parts {
            finish(asset, part, &inbox).unwrap();
        }
        let mut names: Vec<_> = fs::read_dir(&inbox).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        println!("{} octets en {:.1} s → {names:?}", received.load(Ordering::Relaxed), started.elapsed().as_secs_f32());
        assert_eq!(names, ["pellicule.exe", "pellicule_1.exe", "pellicule_2.exe", "pellicule_3.exe"]);
        let modified = fs::metadata(inbox.join("pellicule.exe")).unwrap().modified().unwrap();
        assert_eq!(modified, UNIX_EPOCH + Duration::from_millis(1_600_000_000_000));
        print!("{}", fs::read_to_string(dir.join("journal.log")).unwrap_or_default());
        let _ = fs::remove_dir_all(dir);
    }
}
