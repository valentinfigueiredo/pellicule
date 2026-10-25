//! Récupère les nouvelles photos et vidéos iCloud et les dépose dans « À trier » :
//! Pellicule les renomme, les range et écarte les doublons comme pour tout le reste.

use super::client::{Asset, Error, Result, Session};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Progress {
    /// Éléments de la photothèque parcourus.
    pub seen: u64,
    pub total: u64,
    /// Nouveaux éléments téléchargés pendant cette synchronisation.
    pub downloaded: u64,
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
    fs::create_dir_all(&tmp).map_err(|e| Error::Other(e.to_string()))?;
    fs::create_dir_all(inbox).map_err(|e| Error::Other(e.to_string()))?;

    let total = session.count()?;
    let mut p = Progress { seen: 0, total, downloaded: 0 };
    let mut errors = 0;
    report(&p);

    let mut offset = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let page = session.page(offset)?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;
        for asset in page {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            p.seen += 1;
            if !known.contains(&asset.id) {
                match fetch(session, &asset, &tmp, inbox) {
                    Ok(()) => {
                        let _ = writeln!(log, "{}", asset.id);
                        known.insert(asset.id);
                        p.downloaded += 1;
                    }
                    Err(Error::NeedLogin(m)) => return Err(Error::NeedLogin(m)),
                    Err(_) => errors += 1, // on réessaiera à la prochaine synchronisation
                }
            }
            report(&p);
        }
    }
    Ok(Summary { downloaded: p.downloaded, errors })
}

fn fetch(session: &Session, asset: &Asset, tmp: &Path, inbox: &Path) -> Result<()> {
    let part = tmp.join(format!("{}.part", safe_id(&asset.id)));
    let got = session.download(&asset.url, &part);
    let got = match got {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    if asset.size > 0 && got != asset.size {
        let _ = fs::remove_file(&part);
        return Err(Error::Other(format!("{} : fichier incomplet", asset.filename)));
    }
    // La date iCloud sert de dernier recours quand le fichier n'a pas de date (captures d'écran…).
    if asset.date_ms > 0 {
        let when = UNIX_EPOCH + Duration::from_millis(asset.date_ms as u64);
        if let Ok(f) = fs::File::options().write(true).open(&part) {
            let _ = f.set_modified(when.min(SystemTime::now()));
        }
    }
    let dest = free_path(inbox, &asset.filename);
    fs::rename(&part, &dest).map_err(|e| Error::Other(e.to_string()))
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
