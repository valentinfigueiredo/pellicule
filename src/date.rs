//! Trouve la date de prise de vue d'une photo ou d'une vidéo.
//!
//! Ordre de priorité : métadonnées (EXIF / QuickTime) → nom du fichier → date de modification.

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Photo,
    Video,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Date EXIF de la photo (la plus fiable).
    Exif,
    /// Date enregistrée dans la vidéo.
    Video,
    /// Date trouvée dans le nom du fichier (WhatsApp, captures d'écran…).
    Nom,
    /// Date de modification du fichier (dernier recours).
    Fichier,
}

pub fn kind_of(ext: &str) -> Option<Kind> {
    match ext {
        "jpg" | "jpeg" | "jpe" | "heic" | "heif" | "hif" | "avif" | "png" | "gif" | "webp"
        | "bmp" | "tif" | "tiff" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "raf" | "orf"
        | "rw2" | "srw" | "pef" => Some(Kind::Photo),
        "mp4" | "mov" | "m4v" | "3gp" | "3g2" | "avi" | "mkv" | "mts" | "m2ts" | "wmv"
        | "webm" | "mpg" | "mpeg" => Some(Kind::Video),
        _ => None,
    }
}

/// Extension finale dans l'album (minuscules, `jpeg` → `jpg`).
pub fn normalize_ext(ext: &str) -> &str {
    match ext {
        "jpeg" | "jpe" => "jpg",
        "tif" => "tiff",
        "mpeg" => "mpg",
        e => e,
    }
}

pub fn detect(path: &Path, kind: Kind, ext: &str, mtime: Option<SystemTime>) -> (NaiveDateTime, Source) {
    let from_meta = match kind {
        Kind::Photo => exif_date(path).map(|d| (d, Source::Exif)),
        Kind::Video if is_quicktime(ext) => quicktime_date(path).map(|d| (d, Source::Video)),
        Kind::Video => None,
    };
    if let Some((d, src)) = from_meta.filter(|(d, _)| plausible(d, 1971)) {
        return (d, src);
    }

    let modified = mtime.map(|t| DateTime::<Local>::from(t).naive_local());
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if let Some((d, has_time)) = filename_date(stem) {
        // Nom sans heure (ex. IMG-20240715-WA0001) : on reprend l'heure du fichier si c'est le même jour.
        if !has_time {
            if let Some(m) = modified.filter(|m| m.date() == d.date()) {
                return (m, Source::Nom);
            }
        }
        return (d, Source::Nom);
    }

    (modified.unwrap_or_default(), Source::Fichier)
}

fn plausible(d: &NaiveDateTime, min_year: i32) -> bool {
    let max_year = Local::now().year() + 1;
    (min_year..=max_year).contains(&d.year())
}

fn ymd_hms(n: [u32; 6]) -> Option<NaiveDateTime> {
    NaiveDate::from_ymd_opt(n[0] as i32, n[1], n[2])?.and_hms_opt(n[3], n[4], n[5])
}

/// Lit `AAAA:MM:JJ HH:MM:SS` ou `AAAA-MM-JJTHH:MM:SS…` (séparateurs quelconques).
fn parse_stamp(s: &[u8]) -> Option<NaiveDateTime> {
    let head = s.get(..19)?;
    let digits: Vec<u32> = head.iter().filter(|c| c.is_ascii_digit()).map(|c| (c - b'0') as u32).collect();
    if digits.len() != 14 {
        return None;
    }
    let num = |a: usize, b: usize| digits[a..b].iter().fold(0, |acc, d| acc * 10 + d);
    ymd_hms([num(0, 4), num(4, 6), num(6, 8), num(8, 10), num(10, 12), num(12, 14)])
}

// ---------------------------------------------------------------- EXIF

fn exif_date(path: &Path) -> Option<NaiveDateTime> {
    let file = File::open(path).ok()?;
    let ex = exif::Reader::new().read_from_container(&mut BufReader::new(file)).ok()?;
    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime] {
        if let Some(field) = ex.get_field(tag, exif::In::PRIMARY) {
            if let exif::Value::Ascii(ref v) = field.value {
                if let Some(d) = v.first().and_then(|s| parse_stamp(s)) {
                    return Some(d);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- Vidéo (MP4 / MOV)

fn is_quicktime(ext: &str) -> bool {
    matches!(ext, "mp4" | "mov" | "m4v" | "3gp" | "3g2")
}

/// Secondes entre 1904-01-01 (origine QuickTime) et 1970-01-01.
const QT_EPOCH: i64 = 2_082_844_800;

fn quicktime_date(path: &Path) -> Option<NaiveDateTime> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    // Parcourt les boîtes de premier niveau jusqu'à `moov` (souvent à la fin du fichier).
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h).ok()?;
        let typ = &h[4..8];
        if !typ.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
            return None;
        }
        let (hdr, size) = match u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64 {
            1 => {
                let mut big = [0u8; 8];
                f.read_exact(&mut big).ok()?;
                (16, u64::from_be_bytes(big))
            }
            0 => (8, len - pos),
            s => (8, s),
        };
        if size < hdr {
            return None;
        }
        if typ == b"moov" {
            let body_len = size - hdr;
            if body_len > 256 << 20 {
                return None;
            }
            let mut body = vec![0u8; body_len as usize];
            f.read_exact(&mut body).ok()?;
            return parse_moov(&body);
        }
        pos += size;
    }
    None
}

fn children(mut d: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 8 {
        let size = u32::from_be_bytes(d[0..4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = d[4..8].try_into().unwrap();
        let (hdr, size) = match size {
            1 if d.len() >= 16 => (16, u64::from_be_bytes(d[8..16].try_into().unwrap()) as usize),
            1 => break,
            0 => (8, d.len()),
            s => (8, s),
        };
        if size < hdr || size > d.len() {
            break;
        }
        out.push((typ, &d[hdr..size]));
        d = &d[size..];
    }
    out
}

fn parse_moov(moov: &[u8]) -> Option<NaiveDateTime> {
    let kids = children(moov);
    // iPhone : heure locale exacte (avec fuseau) dans moov/meta.
    let apple = kids.iter().filter(|(t, _)| t == b"meta").find_map(|(_, b)| apple_creation_date(b));
    if let Some(d) = apple.filter(|d| plausible(d, 1971)) {
        return Some(d);
    }
    // Sinon : mvhd, en UTC → converti en heure locale du PC.
    let (_, mvhd) = kids.iter().find(|(t, _)| t == b"mvhd")?;
    let secs = match mvhd.first()? {
        1 => u64::from_be_bytes(mvhd.get(4..12)?.try_into().ok()?) as i64,
        _ => u32::from_be_bytes(mvhd.get(4..8)?.try_into().ok()?) as i64,
    };
    if secs <= QT_EPOCH {
        return None;
    }
    let utc = DateTime::from_timestamp(secs - QT_EPOCH, 0)?;
    Some(utc.with_timezone(&Local).naive_local())
}

fn apple_creation_date(meta: &[u8]) -> Option<NaiveDateTime> {
    // Le `meta` QuickTime n'a pas d'en-tête version/flags, celui de l'ISO en a un.
    let meta = if meta.get(4..8)? == b"hdlr" { meta } else { meta.get(4..)? };
    let kids = children(meta);
    let (_, keys) = kids.iter().find(|(t, _)| t == b"keys")?;
    let (_, ilst) = kids.iter().find(|(t, _)| t == b"ilst")?;

    let mut index = None;
    let (mut p, mut i) = (8usize, 1u32);
    while p + 8 <= keys.len() {
        let sz = u32::from_be_bytes(keys[p..p + 4].try_into().unwrap()) as usize;
        if sz < 8 || p + sz > keys.len() {
            break;
        }
        if &keys[p + 8..p + sz] == b"com.apple.quicktime.creationdate" {
            index = Some(i);
            break;
        }
        p += sz;
        i += 1;
    }
    let index = index?;

    children(ilst)
        .into_iter()
        .filter(|(t, _)| u32::from_be_bytes(*t) == index)
        .flat_map(|(_, item)| children(item))
        .filter(|(t, d)| t == b"data" && d.len() > 8)
        .find_map(|(_, d)| parse_stamp(&d[8..]))
}

// ---------------------------------------------------------------- Nom du fichier

/// Reconnaît `20240715_143208`, `IMG-20240715-WA0001`, `PXL_20240715_143208123`,
/// `Screenshot 2024-07-15 at 14.32.08`, `2024-07-15 14.32.08`…
/// Renvoie la date et si l'heure a été trouvée.
fn filename_date(name: &str) -> Option<(NaiveDateTime, bool)> {
    let runs: Vec<&str> = name.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).collect();
    for i in 0..runs.len() {
        let r = runs[i];
        let found = if r.len() >= 8 {
            date_of(&r[0..4], &r[4..6], &r[6..8]).map(|d| {
                let t = if r.len() >= 14 { hms(&r[8..10], &r[10..12], &r[12..14]) } else if r.len() == 8 { time_of(&runs[i + 1..]) } else { None };
                (d, t)
            })
        } else if r.len() == 4 && i + 2 < runs.len() && runs[i + 1].len() == 2 && runs[i + 2].len() == 2 {
            date_of(r, runs[i + 1], runs[i + 2]).map(|d| (d, time_of(&runs[i + 3..])))
        } else {
            None
        };
        if let Some((d, t)) = found {
            return Some((d.and_time(t.unwrap_or(NaiveTime::MIN)), t.is_some()));
        }
    }
    None
}

fn date_of(y: &str, m: &str, d: &str) -> Option<NaiveDate> {
    let date = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
    plausible(&date.and_time(NaiveTime::MIN), 1990).then_some(date)
}

fn hms(h: &str, m: &str, s: &str) -> Option<NaiveTime> {
    NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, s.parse().ok()?)
}

fn time_of(rest: &[&str]) -> Option<NaiveTime> {
    match rest {
        [t, ..] if t.len() >= 6 => hms(&t[0..2], &t[2..4], &t[4..6]),
        [h, m, s, ..] if h.len() == 2 && m.len() == 2 && s.len() == 2 => hms(h, m, s),
        _ => None,
    }
}

/// `AAAA-MM-JJ_HH-MM-SS` : se trie dans l'ordre chronologique.
pub fn file_stem_for(d: &NaiveDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd(name: &str) -> Option<String> {
        filename_date(name).map(|(d, t)| format!("{} {t}", file_stem_for(&d)))
    }

    #[test]
    fn noms_reconnus() {
        assert_eq!(fd("IMG_20240715_143208").as_deref(), Some("2024-07-15_14-32-08 true"));
        assert_eq!(fd("PXL_20240715_143208123").as_deref(), Some("2024-07-15_14-32-08 true"));
        assert_eq!(fd("VID-20240715-WA0001").as_deref(), Some("2024-07-15_00-00-00 false"));
        assert_eq!(fd("Screenshot 2024-07-15 at 14.32.08").as_deref(), Some("2024-07-15_14-32-08 true"));
        assert_eq!(fd("20240715143208").as_deref(), Some("2024-07-15_14-32-08 true"));
        assert_eq!(fd("Photo 2023-12-31").as_deref(), Some("2023-12-31_00-00-00 false"));
    }

    #[test]
    fn noms_ignores() {
        assert_eq!(fd("IMG_1234"), None);
        assert_eq!(fd("DSC01990"), None);
        assert_eq!(fd("1721053928123"), None);
        assert_eq!(fd("vacances"), None);
        assert_eq!(fd("20241345_000000"), None);
    }

    #[test]
    fn stamps() {
        assert!(parse_stamp(b"2023:08:14 18:42:05").is_some());
        assert!(parse_stamp(b"2023-08-14T18:42:05+0200").is_some());
        assert!(parse_stamp(b"0000:00:00 00:00:00").is_none());
    }
}
