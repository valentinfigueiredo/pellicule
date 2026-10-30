//! Récupère les nouvelles photos et vidéos iCloud et les dépose dans « À trier » :
//! Pellicule les renomme, les range et écarte les doublons comme pour tout le reste.
//! Les vidéos des Live Photos vont à part (`Live Photos`) : sinon chaque photo aurait
//! son double de quelques secondes dans la pellicule.

use super::client::{Asset, Downloader, Error, Kind, Resource, Result, Session};
use chrono::{DateTime, Local};
use std::collections::{HashMap, HashSet, VecDeque};
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
/// Recherche rapide : on s'arrête après autant d'éléments d'affilée déjà récupérés.
/// iCloud liste du plus récent au plus ancien, donc les nouveautés sont au début.
const QUICK_STOP: u64 = 300;

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
    /// Recherche rapide des nouveautés (sinon : toute la photothèque est parcourue).
    pub quick: bool,
}

pub struct Summary {
    pub downloaded: u64,
    pub errors: u64,
    /// Toute la photothèque a été parcourue jusqu'au bout.
    pub complete: bool,
}

/// `state` : dossier de travail (`.pellicule/icloud`). `inbox` : la boîte « À trier ».
/// `live` : le dossier des vidéos de Live Photos.
#[allow(clippy::too_many_arguments)]
pub fn run(
    session: &mut Session,
    state: &Path,
    inbox: &Path,
    live: &Path,
    full: bool,
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

    // Première fois : il faut tout parcourir.
    let quick = !full && !known.is_empty();
    let total = session.count()?;
    let downloader = session.downloader();
    let received = AtomicU64::new(0);
    let mut speed = Speed::default();
    let mut p = Progress { seen: 0, total, downloaded: 0, bytes: 0, speed: 0, quick };
    let mut errors = 0;
    let mut known_streak = 0u64;
    let mut reached_end = false;
    report(&p);

    // Les nouveautés doivent être vues en premier. On vérifie le sens de la liste sur la
    // première page : si elle commence par les plus anciennes, la recherche rapide part de la fin.
    let first = session.page(0)?;
    let backward = quick && !newest_first(&first);
    let mut first_page = (!backward).then_some(first);
    let mut offset: i64 = if backward { total as i64 - PAGE as i64 } else { 0 };
    while !stop.load(Ordering::Relaxed) {
        if quick && known_streak >= QUICK_STOP {
            break;
        }
        let page = if backward {
            if offset <= -(PAGE as i64) {
                reached_end = true;
                break;
            }
            let start = offset.max(0) as u64;
            let mut page = session.page(start)?;
            page.truncate((offset + PAGE as i64 - start as i64) as usize);
            page.reverse();
            offset -= PAGE as i64;
            page
        } else {
            let page = match first_page.take() {
                Some(p) => p,
                None => session.page(offset as u64)?,
            };
            offset += page.len() as i64;
            page
        };
        if page.is_empty() {
            reached_end = true;
            break;
        }
        let is_done = |a: &Asset| a.resources.iter().all(|r| known.contains(&r.key));
        for a in &page {
            known_streak = if is_done(a) { known_streak + 1 } else { 0 };
        }
        // Une photo compte comme parcourue quand tous ses fichiers sont traités.
        let mut remaining: HashMap<String, usize> = HashMap::new();
        let mut jobs = VecDeque::new();
        for a in page {
            let todo: Vec<Resource> = a.resources.into_iter().filter(|r| !known.contains(&r.key)).collect();
            if todo.is_empty() {
                p.seen += 1;
            } else {
                remaining.insert(a.id.clone(), todo.len());
                jobs.extend(todo.into_iter().map(|r| (a.id.clone(), a.date_ms, r)));
            }
        }
        report(&p);

        let pending = jobs.len();
        let queue = Mutex::new(jobs);
        let (tx, rx) = mpsc::channel::<(String, i64, Resource, Result<PathBuf>)>();
        std::thread::scope(|scope| {
            for _ in 0..PARALLEL.min(pending) {
                let (tx, queue, downloader, received, tmp) = (tx.clone(), &queue, &downloader, &received, &tmp);
                scope.spawn(move || loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some((id, date_ms, res)) = queue.lock().unwrap().pop_front() else { return };
                    let result = fetch(downloader, &res, tmp, received);
                    if tx.send((id, date_ms, res, result)).is_err() {
                        return;
                    }
                });
            }
            drop(tx);

            // Le fil principal range les fichiers terminés un par un (pas de collision de noms).
            loop {
                match rx.recv_timeout(Duration::from_millis(300)) {
                    Ok((id, date_ms, res, result)) => {
                        match result.and_then(|part| finish(&res, date_ms, &part, inbox, live)) {
                            Ok(()) => {
                                let _ = writeln!(log, "{}", res.key);
                                known.insert(res.key);
                                p.downloaded += 1;
                            }
                            Err(_) => errors += 1,
                        }
                        if let Some(n) = remaining.get_mut(&id) {
                            *n -= 1;
                            if *n == 0 {
                                p.seen += 1;
                            }
                        }
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
    let complete = reached_end && !stop.load(Ordering::Relaxed);
    Ok(Summary { downloaded: p.downloaded, errors, complete })
}

/// Éléments par page renvoyés par iCloud (200 enregistrements = 100 photos + leurs fiches).
const PAGE: u64 = 100;

/// La page va-t-elle du plus récent au plus ancien ?
fn newest_first(page: &[Asset]) -> bool {
    let dates: Vec<i64> = page.iter().map(|a| a.date_ms).filter(|&d| d > 0).collect();
    match (dates.first(), dates.last()) {
        (Some(a), Some(b)) => a >= b,
        _ => true,
    }
}

/// Télécharge dans le dossier temporaire, avec plusieurs essais.
fn fetch(downloader: &Downloader, res: &Resource, tmp: &Path, received: &AtomicU64) -> Result<PathBuf> {
    let part = tmp.join(format!("{}.part", safe_id(&res.key)));
    let mut last = Error::Other(String::new());
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2 * attempt as u64));
        }
        match downloader.download(&res.url, &part, &res.filename, received) {
            Ok(n) if res.size == 0 || n == res.size => return Ok(part),
            Ok(_) => last = Error::Other(format!("{} : fichier incomplet", res.filename)),
            Err(e) => last = e,
        }
        let _ = fs::remove_file(&part);
    }
    Err(last)
}

/// Date le fichier et le dépose à sa place.
fn finish(res: &Resource, date_ms: i64, part: &Path, inbox: &Path, live: &Path) -> Result<()> {
    // La date iCloud sert de dernier recours quand le fichier n'a pas de date (captures d'écran…).
    if date_ms > 0 {
        let when = UNIX_EPOCH + Duration::from_millis(date_ms as u64);
        if let Ok(f) = fs::File::options().write(true).open(part) {
            let _ = f.set_modified(when.min(SystemTime::now()));
        }
    }
    let dest = if res.kind == Kind::Live {
        let (dir, name) = live_place(date_ms, &res.filename);
        let dir = live.join(dir);
        fs::create_dir_all(&dir).map_err(|e| Error::Other(e.to_string()))?;
        free_path(&dir, &name)
    } else {
        free_path(inbox, &res.filename)
    };
    fs::rename(part, &dest).map_err(|e| Error::Other(e.to_string()))
}

/// `Live Photos/2024/2024-07/2024-07-15_14-32-08_IMG_1234_live.mov`, à l'heure locale du PC.
fn live_place(date_ms: i64, filename: &str) -> (PathBuf, String) {
    match DateTime::from_timestamp_millis(date_ms).filter(|_| date_ms > 0) {
        Some(d) => {
            let d = d.with_timezone(&Local);
            let dir = Path::new(&d.format("%Y").to_string()).join(d.format("%Y-%m").to_string());
            (dir, format!("{}_{filename}", d.format("%Y-%m-%d_%H-%M-%S")))
        }
        None => (PathBuf::from("Sans date"), filename.to_string()),
    }
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

    /// Les vidéos Live vont dans leur dossier, datées ; le reste passe par « À trier ».
    #[test]
    fn destinations() {
        let dir = std::env::temp_dir().join("pellicule-test-destinations");
        let _ = fs::remove_dir_all(&dir);
        let (inbox, live) = (dir.join("inbox"), dir.join("live"));
        fs::create_dir_all(&inbox).unwrap();
        let date_ms = 1_721_000_000_000;
        for (kind, name) in [(Kind::Live, "IMG_2_live.mov"), (Kind::Edited, "IMG_2_retouche.jpg")] {
            let part = dir.join(name);
            fs::write(&part, b"x").unwrap();
            let res = Resource { kind, key: name.into(), filename: name.into(), size: 1, url: String::new() };
            finish(&res, date_ms, &part, &inbox, &live).unwrap();
        }
        assert!(inbox.join("IMG_2_retouche.jpg").exists());
        let (sub, name) = live_place(date_ms, "IMG_2_live.mov");
        assert!(name.ends_with("_IMG_2_live.mov") && name.starts_with("2024-07-"), "{name}");
        assert!(live.join(sub).join(name).exists());
        assert_eq!(live_place(0, "a.mov").0, PathBuf::from("Sans date"));
        let _ = fs::remove_dir_all(dir);
    }

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
                    let res = Resource { kind: Kind::Original, key: format!("id{i}"), filename: "pellicule.exe".into(), size: 0, url: url.into() };
                    let (d, t, r) = (&downloader, &tmp, &received);
                    s.spawn(move || (fetch(d, &res, t, r).unwrap(), res))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (part, res) in &parts {
            finish(res, 1_600_000_000_000, part, &inbox, &dir.join("live")).unwrap();
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
