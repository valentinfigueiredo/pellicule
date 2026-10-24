//! L'album : index des fichiers, détection des doublons, renommage et rangement.

use crate::date::{self, Source};
use chrono::{Datelike, Local};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use xxhash_rust::xxh3::Xxh3;

pub const INBOX: &str = "À trier";
pub const DUPLICATES: &str = "_Doublons";
const META: &str = ".pellicule";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

pub enum Outcome {
    Added { rel: String, source: Source },
    Duplicate { of: String },
    Ignored,
}

struct Entry {
    rel: String,
    size: u64,
    hash: Option<u128>,
    /// En mode essai, le fichier n'est pas encore dans l'album : on lit l'original.
    planned_from: Option<PathBuf>,
}

pub struct Album {
    pub root: PathBuf,
    pub dry: bool,
    entries: Vec<Entry>,
    by_size: HashMap<u64, Vec<usize>>,
    names: HashSet<String>,
    journal: Option<BufWriter<File>>,
}

impl Album {
    pub fn open(root: PathBuf, dry: bool) -> io::Result<Album> {
        let mut album = Album { root, dry, entries: Vec::new(), by_size: HashMap::new(), names: HashSet::new(), journal: None };
        if !dry {
            fs::create_dir_all(album.root.join(META))?;
            fs::create_dir_all(album.inbox())?;
            let j = OpenOptions::new().create(true).append(true).open(album.root.join(META).join("journal.tsv"))?;
            album.journal = Some(BufWriter::new(j));
        }

        // Empreintes déjà calculées lors des sessions précédentes.
        let mut known: HashMap<String, (u64, u128)> = HashMap::new();
        if let Ok(f) = File::open(album.index_path()) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let mut it = line.splitn(3, '\t');
                if let (Some(size), Some(hash), Some(rel)) = (it.next(), it.next(), it.next()) {
                    if let (Ok(size), Ok(hash)) = (size.parse(), u128::from_str_radix(hash, 16)) {
                        known.insert(rel.to_string(), (size, hash));
                    }
                }
            }
        }

        // L'album sur le disque fait foi : on y retrouve aussi ce que tu as ajouté à la main.
        let mut found = Vec::new();
        let root = album.root.clone();
        walk(&root, &mut |path, size| {
            let rel = path.strip_prefix(&root).unwrap();
            let top = rel.components().next().and_then(|c| c.as_os_str().to_str()).unwrap_or("");
            if rel.components().count() > 1 && (top == INBOX || top == DUPLICATES) {
                return;
            }
            if let Some(rel) = rel_string(rel) {
                found.push((rel, size));
            }
        });
        found.sort();
        for (rel, size) in found {
            let hash = known.get(&rel).filter(|(s, _)| *s == size).map(|(_, h)| *h);
            album.add(rel, size, hash, None);
        }
        Ok(album)
    }

    pub fn inbox(&self) -> PathBuf {
        self.root.join(INBOX)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(META).join("index.tsv")
    }

    pub fn path_of(&self, rel: &str) -> PathBuf {
        rel.split('/').fold(self.root.clone(), |p, c| p.join(c))
    }

    /// Chemins relatifs de tous les fichiers de l'album.
    pub fn files(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.rel.clone()).collect()
    }

    fn add(&mut self, rel: String, size: u64, hash: Option<u128>, planned_from: Option<PathBuf>) {
        self.names.insert(rel.to_lowercase());
        self.by_size.entry(size).or_default().push(self.entries.len());
        self.entries.push(Entry { rel, size, hash, planned_from });
    }

    pub fn save(&mut self) -> io::Result<()> {
        if self.dry {
            return Ok(());
        }
        let tmp = self.root.join(META).join("index.tsv.tmp");
        {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for e in &self.entries {
                if let Some(h) = e.hash {
                    writeln!(w, "{}\t{:032x}\t{}", e.size, h, e.rel)?;
                }
            }
            w.flush()?;
        }
        fs::rename(tmp, self.index_path())?;
        if let Some(j) = &mut self.journal {
            j.flush()?;
        }
        Ok(())
    }

    /// Cherche un fichier identique dans l'album. On ne calcule les empreintes
    /// que si un fichier de même taille existe déjà : c'est ce qui rend l'import rapide.
    fn find_duplicate(&mut self, path: &Path, size: u64) -> io::Result<(Option<String>, Option<u128>)> {
        let Some(candidates) = self.by_size.get(&size).cloned() else {
            return Ok((None, None));
        };
        let h = hash_file(path)?;
        for i in candidates {
            let e = &self.entries[i];
            let existing = match e.hash {
                Some(x) => x,
                None => match hash_file(&e.planned_from.clone().unwrap_or_else(|| self.path_of(&e.rel))) {
                    Ok(x) => {
                        self.entries[i].hash = Some(x);
                        x
                    }
                    Err(_) => continue,
                },
            };
            if existing == h {
                return Ok((Some(self.entries[i].rel.clone()), Some(h)));
            }
        }
        Ok((None, Some(h)))
    }

    fn free_name(&self, dir: &str, stem: &str, ext: &str) -> String {
        (0..)
            .map(|n| if n == 0 { format!("{dir}/{stem}.{ext}") } else { format!("{dir}/{stem}_{n:02}.{ext}") })
            .find(|rel| !self.names.contains(&rel.to_lowercase()) && !self.path_of(rel).exists())
            .unwrap()
    }

    pub fn import(&mut self, src: &Path, mode: Mode) -> io::Result<Outcome> {
        let Some(ext) = src.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) else {
            return Ok(Outcome::Ignored);
        };
        let Some(kind) = date::kind_of(&ext) else {
            return Ok(Outcome::Ignored);
        };
        let meta = fs::metadata(src)?;
        let size = meta.len();
        if size == 0 {
            return Ok(Outcome::Ignored);
        }

        let (dup, hash) = self.find_duplicate(src, size)?;
        if let Some(of) = dup {
            if mode == Mode::Move && !self.dry {
                self.set_aside(src)?;
            }
            return Ok(Outcome::Duplicate { of });
        }

        let (when, source) = date::detect(src, kind, &ext, meta.modified().ok());
        let dir = format!("{:04}/{:04}-{:02}", when.year(), when.year(), when.month());
        let rel = self.free_name(&dir, &date::file_stem_for(&when), date::normalize_ext(&ext));

        if !self.dry {
            let dst = self.path_of(&rel);
            fs::create_dir_all(dst.parent().unwrap())?;
            transfer(src, &dst, mode)?;
            self.log(if mode == Mode::Move { "déplacé" } else { "copié" }, src, &rel);
        }
        self.add(rel.clone(), size, hash, self.dry.then(|| src.to_path_buf()));
        Ok(Outcome::Added { rel, source })
    }

    /// Range un doublon de la boîte « À trier » dans `_Doublons` (jamais supprimé).
    fn set_aside(&mut self, src: &Path) -> io::Result<()> {
        let dir = self.root.join(DUPLICATES);
        fs::create_dir_all(&dir)?;
        let stem = src.file_stem().unwrap_or_default().to_string_lossy();
        let ext = src.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let dst = (0..)
            .map(|n| if n == 0 { dir.join(format!("{stem}{ext}")) } else { dir.join(format!("{stem}_{n:02}{ext}")) })
            .find(|p| !p.exists())
            .unwrap();
        transfer(src, &dst, Mode::Move)?;
        let rel = format!("{DUPLICATES}/{}", dst.file_name().unwrap().to_string_lossy());
        self.log("doublon", src, &rel);
        Ok(())
    }

    fn log(&mut self, action: &str, src: &Path, rel: &str) {
        if let Some(j) = &mut self.journal {
            let now = Local::now().format("%Y-%m-%d %H:%M:%S");
            let _ = writeln!(j, "{now}\t{action}\t{}\t{rel}", src.display());
        }
    }
}

fn rel_string(rel: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

fn hash_file(path: &Path) -> io::Result<u128> {
    let mut f = File::open(path)?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest128())
}

fn transfer(src: &Path, dst: &Path, mode: Mode) -> io::Result<()> {
    if mode == Mode::Move && fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Copie vers un fichier temporaire puis renommage : jamais de fichier à moitié copié dans l'album.
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = fs::copy(src, &tmp).and_then(|_| fs::rename(&tmp, dst)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if mode == Mode::Move {
        fs::remove_file(src)?;
    }
    Ok(())
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('$') || name == "System Volume Information"
}

/// Parcourt un dossier récursivement et appelle `f` pour chaque fichier photo/vidéo.
pub fn walk(dir: &Path, f: &mut dyn FnMut(&Path, u64)) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut items: Vec<_> = rd.filter_map(Result::ok).collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name();
        let name = name.to_string_lossy();
        let path = e.path();
        if ft.is_dir() {
            if !skip_dir(&name) {
                walk(&path, f);
            }
        } else if ft.is_file() && !name.starts_with('.') && is_media(&path) {
            if let Ok(m) = e.metadata() {
                f(&path, m.len());
            }
        }
    }
}

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| date::kind_of(&e.to_ascii_lowercase()).is_some())
}

/// Supprime les dossiers vides laissés dans la boîte « À trier » (mais pas la boîte elle-même).
pub fn prune_empty_dirs(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.filter_map(Result::ok) {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            let p = e.path();
            prune_empty_dirs(&p);
            let _ = fs::remove_dir(&p); // échoue sans rien faire si le dossier n'est pas vide
        }
    }
}

/// Ce qu'il faut importer à partir de chemins déposés par l'utilisateur.
#[derive(Default)]
pub struct Sources {
    /// Fichiers extérieurs à l'album : copiés.
    pub outside: Vec<PathBuf>,
    /// Fichiers déjà dans « À trier » : déplacés.
    pub inbox: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn gather(album_root: &Path, paths: &[PathBuf]) -> Sources {
    let root = fs::canonicalize(album_root).unwrap_or(album_root.to_path_buf());
    let inbox = root.join(INBOX);
    let mut s = Sources::default();
    for p in paths {
        let Ok(canon) = fs::canonicalize(p) else {
            s.warnings.push(format!("Introuvable : {}", p.display()));
            continue;
        };
        // `canonicalize` sert à comparer ; on garde le chemin lisible (sans `\\?\` sous Windows) pour le reste.
        let p = std::path::absolute(p).unwrap_or(canon.clone());
        if canon.starts_with(&inbox) {
            s.inbox.extend(collect(&p));
        } else if canon.starts_with(&root) {
            s.warnings.push(format!("Déjà dans l'album, ignoré : {}", p.display()));
        } else {
            s.outside.extend(collect(&p));
        }
    }
    s
}

/// Tous les fichiers photo/vidéo d'un chemin (fichier ou dossier), triés.
pub fn collect(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if path.is_file() {
        if is_media(path) {
            out.push(path.to_path_buf());
        }
    } else {
        walk(path, &mut |p, _| out.push(p.to_path_buf()));
    }
    out
}

type Signature = (u64, Option<SystemTime>);

/// Surveille « À trier » : un fichier est prêt quand sa taille et sa date n'ont pas
/// bougé entre deux passages (pour ne pas prendre une copie en cours).
#[derive(Default)]
pub struct InboxWatcher {
    seen: HashMap<PathBuf, Signature>,
    stuck: HashMap<PathBuf, Signature>,
}

impl InboxWatcher {
    pub fn poll(&mut self, inbox: &Path) -> Vec<PathBuf> {
        let mut ready = Vec::new();
        let mut next = HashMap::new();
        walk(inbox, &mut |p, size| {
            let sig = (size, fs::metadata(p).ok().and_then(|m| m.modified().ok()));
            if self.stuck.get(p) == Some(&sig) {
                return;
            }
            if self.seen.get(p) == Some(&sig) {
                ready.push(p.to_path_buf());
            } else {
                next.insert(p.to_path_buf(), sig);
            }
        });
        self.seen = next;
        ready
    }

    /// À appeler après traitement : ce qui est encore là (erreur, fichier vide) n'est pas retenté.
    pub fn done(&mut self, ready: Vec<PathBuf>) {
        for p in ready {
            if let Ok(m) = fs::metadata(&p) {
                self.stuck.insert(p, (m.len(), m.modified().ok()));
            }
        }
    }
}
