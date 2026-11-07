//! Les réglages (emplacement de l'album…) sont dans `%APPDATA%\Pellicule\pellicule.ini`,
//! pour que Pellicule les retrouve où que soit l'exe (installé, téléchargé, déplacé).
//! Les versions d'avant les gardaient à côté de l'exe : ils sont repris automatiquement.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn exe_dir() -> Option<PathBuf> {
    env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// `%APPDATA%\Pellicule` sous Windows ; ailleurs (pour développer), le dossier de l'exe.
pub fn dir() -> Option<PathBuf> {
    env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Pellicule")).or_else(exe_dir)
}

fn path() -> Option<PathBuf> {
    dir().map(|d| d.join("pellicule.ini"))
}

fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn read_all() -> Vec<(String, String)> {
    if let Some(text) = path().and_then(|p| fs::read_to_string(p).ok()) {
        return parse(&text);
    }
    // Ancien emplacement, à côté de l'exe : on le reprend et on le recopie au nouvel endroit.
    let old = exe_dir().map(|d| d.join("pellicule.ini")).and_then(|p| fs::read_to_string(p).ok());
    let values = old.map(|t| parse(&t)).unwrap_or_default();
    if !values.is_empty() {
        let _ = write_all(&values);
    }
    values
}

fn write_all(values: &[(String, String)]) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::other("dossier des réglages introuvable"))?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::from("# Réglages de Pellicule\n");
    for (k, v) in values {
        text += &format!("{k} = {v}\n");
    }
    fs::write(p, text)
}

pub fn get(key: &str) -> Option<String> {
    read_all().into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub fn set(key: &str, value: &str) -> io::Result<()> {
    let mut values = read_all();
    match values.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value.to_string(),
        None => values.push((key.to_string(), value.to_string())),
    }
    write_all(&values)
}

pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// L'album choisi : `--album`, puis la variable `PELLICULE_ALBUM`, puis les réglages.
/// `None` au tout premier lancement.
pub fn album(flag: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = flag {
        return Some(absolute(p));
    }
    if let Some(p) = env::var_os("PELLICULE_ALBUM") {
        return Some(absolute(Path::new(&p)));
    }
    get("album").map(PathBuf::from)
}

pub fn save_album(album: &Path) -> io::Result<()> {
    set("album", &album.display().to_string())
}

pub fn default_album() -> PathBuf {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(home).join("Pictures").join("Pellicule")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lecture() {
        let v = parse("# Emplacement de l'album Pellicule\nalbum = D:\\Photos\\Pellicule\nautre=1\n");
        assert_eq!(v, vec![("album".into(), "D:\\Photos\\Pellicule".into()), ("autre".into(), "1".into())]);
    }
}
