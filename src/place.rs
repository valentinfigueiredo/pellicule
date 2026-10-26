//! Où une photo ou une vidéo a été prise : coordonnées GPS lues dans le fichier,
//! et index des lieux de l'album (`.pellicule/lieux.tsv`) pour la carte.

use crate::date::{self, Kind};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Latitude, longitude en degrés.
pub type LatLon = (f64, f64);

pub fn locate(path: &Path) -> Option<LatLon> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let found = match date::kind_of(&ext)? {
        Kind::Photo => exif_gps(path),
        Kind::Video if date::is_quicktime(&ext) => video_gps(path),
        Kind::Video => None,
    };
    // (0, 0) : en pleine mer au large de l'Afrique, c'est presque toujours un GPS vide.
    found.filter(|(lat, lon)| lat.abs() <= 90.0 && lon.abs() <= 180.0 && (lat.abs() > 1e-6 || lon.abs() > 1e-6))
}

fn exif_gps(path: &Path) -> Option<LatLon> {
    let file = File::open(path).ok()?;
    let ex = exif::Reader::new().read_from_container(&mut BufReader::new(file)).ok()?;
    let coord = |tag, ref_tag, negative: u8| -> Option<f64> {
        let field = ex.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(ref v) = field.value else { return None };
        let parts: Vec<f64> = v.iter().map(|r| r.to_f64()).collect();
        let deg = parts.first()? + parts.get(1).unwrap_or(&0.0) / 60.0 + parts.get(2).unwrap_or(&0.0) / 3600.0;
        let sign = match ex.get_field(ref_tag, exif::In::PRIMARY).map(|f| &f.value) {
            Some(exif::Value::Ascii(a)) if a.first().and_then(|s| s.first()) == Some(&negative) => -1.0,
            _ => 1.0,
        };
        deg.is_finite().then_some(sign * deg)
    };
    Some((
        coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, b'S')?,
        coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, b'W')?,
    ))
}

/// iPhone : clé `com.apple.quicktime.location.ISO6709` ; Android : boîte `©xyz` dans `udta`.
/// Les deux contiennent du texte ISO 6709 comme `+48.8584+002.2945+035.000/`.
fn video_gps(path: &Path) -> Option<LatLon> {
    let moov = date::read_moov(path)?;
    let kids = date::children(&moov);
    let apple = kids
        .iter()
        .filter(|(t, _)| t == b"meta")
        .find_map(|(_, m)| date::apple_key(m, b"com.apple.quicktime.location.ISO6709"))
        .and_then(parse_iso6709);
    apple.or_else(|| {
        let (_, udta) = kids.iter().find(|(t, _)| t == b"udta")?;
        let (_, xyz) = date::children(udta).into_iter().find(|(t, _)| t == b"\xa9xyz")?;
        // 2 octets de longueur + 2 octets de langue, puis le texte.
        parse_iso6709(xyz.get(4..)?)
    })
}

fn parse_iso6709(raw: &[u8]) -> Option<LatLon> {
    let s = std::str::from_utf8(raw).ok()?.trim_end_matches(['/', '\0']);
    // Découpe sur chaque signe : « +48.8584 », « +002.2945 », « +035.000 ».
    let mut nums = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c == '+' || c == '-' {
            if let Some(st) = start {
                nums.push(&s[st..i]);
            }
            start = Some(i);
        }
    }
    if let Some(st) = start {
        nums.push(&s[st..]);
    }
    Some((nums.first()?.parse().ok()?, nums.get(1)?.parse().ok()?))
}

/// Les lieux déjà lus, pour ne pas relire chaque fichier à chaque lancement.
pub struct Index {
    path: PathBuf,
    pub known: HashMap<String, Option<LatLon>>,
}

impl Index {
    pub fn load(album_root: &Path) -> Index {
        let path = album_root.join(".pellicule").join("lieux.tsv");
        let mut known = HashMap::new();
        if let Ok(f) = File::open(&path) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let mut it = line.splitn(3, '\t');
                if let (Some(lat), Some(lon), Some(rel)) = (it.next(), it.next(), it.next()) {
                    let place = lat.parse().ok().zip(lon.parse().ok());
                    known.insert(rel.to_string(), place);
                }
            }
        }
        Index { path, known }
    }

    pub fn save(&self) {
        let tmp = self.path.with_extension("tsv.tmp");
        let write = || -> std::io::Result<()> {
            let mut w = BufWriter::new(File::create(&tmp)?);
            for (rel, place) in &self.known {
                match place {
                    Some((lat, lon)) => writeln!(w, "{lat:.6}\t{lon:.6}\t{rel}")?,
                    None => writeln!(w, "-\t-\t{rel}")?,
                }
            }
            w.flush()
        };
        if write().is_ok() {
            let _ = fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boite(t: &[u8], corps: &[u8]) -> Vec<u8> {
        let mut b = ((8 + corps.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(t);
        b.extend_from_slice(corps);
        b
    }

    #[test]
    fn videos() {
        let dir = std::env::temp_dir().join("pellicule-test-lieux");
        fs::create_dir_all(&dir).unwrap();
        // iPhone : clé Apple dans moov/meta.
        let key = b"com.apple.quicktime.location.ISO6709";
        let mut keys = vec![0, 0, 0, 0, 0, 0, 0, 1];
        keys.extend(((8 + key.len()) as u32).to_be_bytes());
        keys.extend(b"mdta");
        keys.extend(key);
        let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
        data.extend(b"+43.2965+005.3698+012.000/");
        let meta = [boite(b"hdlr", &[0; 25]), boite(b"keys", &keys), boite(b"ilst", &boite(&1u32.to_be_bytes(), &boite(b"data", &data)))].concat();
        let iphone = [boite(b"ftyp", b"qt  \0\0\0\0"), boite(b"moov", &boite(b"meta", &meta))].concat();
        fs::write(dir.join("iphone.mov"), iphone).unwrap();
        assert_eq!(locate(&dir.join("iphone.mov")), Some((43.2965, 5.3698)));
        // Android : ©xyz dans moov/udta.
        let mut xyz = vec![0, 18, 0x15, 0xc7];
        xyz.extend(b"+45.7640+004.8357/");
        let android = [boite(b"ftyp", b"isom\0\0\0\0"), boite(b"mdat", &[0; 64]), boite(b"moov", &boite(b"udta", &boite(b"\xa9xyz", &xyz)))].concat();
        fs::write(dir.join("android.mp4"), android).unwrap();
        assert_eq!(locate(&dir.join("android.mp4")), Some((45.764, 4.8357)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn iso6709() {
        assert_eq!(parse_iso6709(b"+48.8584+002.2945+035.000/"), Some((48.8584, 2.2945)));
        assert_eq!(parse_iso6709(b"-33.8568+151.2153/"), Some((-33.8568, 151.2153)));
        assert_eq!(parse_iso6709(b"rien"), None);
    }
}
