//! L'emplacement de l'album est mémorisé dans `pellicule.ini`, à côté de l'exécutable.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn path() -> Option<PathBuf> {
    env::current_exe().ok()?.parent().map(|d| d.join("pellicule.ini"))
}

pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// L'album choisi : `--album`, puis la variable `PELLICULE_ALBUM`, puis `pellicule.ini`.
/// `None` au tout premier lancement.
pub fn album(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(absolute(p));
    }
    if let Some(p) = env::var_os("PELLICULE_ALBUM") {
        return Some(absolute(Path::new(&p)));
    }
    let text = fs::read_to_string(path()?).ok()?;
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("album"))
        .filter_map(|l| l.trim_start().strip_prefix('='))
        .map(|v| PathBuf::from(v.trim()))
        .next()
}

pub fn save_album(album: &Path) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::other("dossier de l'exécutable introuvable"))?;
    fs::write(p, format!("# Emplacement de l'album Pellicule\nalbum = {}\n", album.display()))
}

pub fn default_album() -> PathBuf {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(home).join("Pictures").join("Pellicule")
}
