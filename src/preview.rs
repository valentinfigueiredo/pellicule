//! Aperçus des formats que Windows ne sait pas toujours lire sans extension payante :
//! HEIC (iPhone), RAW (appareils photo) et vidéos MP4/MOV en H.264 ou HEVC.
//! Tout est décodé en Rust, sans codec à installer.

use image::{DynamicImage, RgbImage};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

type Result<T> = std::result::Result<T, String>;

/// Ce module sait-il faire l'aperçu de cette extension ?
pub fn handles(ext: &str) -> bool {
    matches!(ext, "heic" | "heif" | "hif") || is_raw(ext) || crate::date::is_quicktime(ext)
}

pub fn is_raw(ext: &str) -> bool {
    matches!(ext, "dng" | "cr2" | "cr3" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "raf" | "orf" | "rw2" | "srw" | "pef" | "raw")
}

pub fn decode(path: &Path, ext: &str) -> Result<DynamicImage> {
    match ext {
        "heic" | "heif" | "hif" => heic(path),
        e if is_raw(e) => raw(path),
        _ => video(path),
    }
}

// ---------------------------------------------------------------- HEIC

fn heic(path: &Path) -> Result<DynamicImage> {
    let img = heif_oxide::decode_file(path).map_err(|e| format!("HEIC : {e}"))?;
    let rgba = img.to_rgba8();
    image::RgbaImage::from_raw(img.width, img.height, rgba)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| "HEIC : image invalide".into())
}

// ---------------------------------------------------------------- RAW

/// Les RAW contiennent presque tous un aperçu JPEG (souvent en pleine taille) : on prend le plus grand.
fn raw(path: &Path) -> Result<DynamicImage> {
    let mut data = Vec::new();
    File::open(path)
        .and_then(|f| f.take(96 << 20).read_to_end(&mut data))
        .map_err(|e| e.to_string())?;
    let (start, end) = embedded_jpegs(&data).into_iter().max_by_key(|(s, e)| e - s).ok_or("RAW : aucun aperçu intégré")?;
    let mut img = image::load_from_memory_with_format(&data[start..end], image::ImageFormat::Jpeg)
        .map_err(|e| format!("RAW : aperçu illisible ({e})"))?;
    // L'orientation est dans l'EXIF du RAW, pas toujours dans le JPEG intégré.
    if let Some(o) = raw_orientation(path) {
        img.apply_orientation(o);
    }
    Ok(img)
}

fn raw_orientation(path: &Path) -> Option<image::metadata::Orientation> {
    let file = File::open(path).ok()?;
    let ex = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()?;
    let v = ex.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?.value.get_uint(0)?;
    image::metadata::Orientation::from_exif(v as u8)
}

/// Repère les JPEG complets (de SOI à EOI) cachés dans un fichier.
fn embedded_jpegs(d: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 4 < d.len() {
        if d[i] == 0xFF && d[i + 1] == 0xD8 && d[i + 2] == 0xFF {
            if let Some(end) = jpeg_end(d, i) {
                found.push((i, end));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    found
}

/// Suit les segments d'un JPEG jusqu'à la fin des données compressées.
fn jpeg_end(d: &[u8], start: usize) -> Option<usize> {
    let mut p = start + 2;
    loop {
        if p + 4 > d.len() || d[p] != 0xFF {
            return None;
        }
        let marker = d[p + 1];
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => p += 2,
            0xD9 => return Some(p + 2),
            0xDA => {
                // Données compressées : on cherche le prochain marqueur qui n'est ni un 0xFF00 ni un RST.
                let len = u16::from_be_bytes([d[p + 2], d[p + 3]]) as usize;
                p += 2 + len;
                while p + 1 < d.len() {
                    if d[p] == 0xFF && d[p + 1] != 0x00 && !(0xD0..=0xD7).contains(&d[p + 1]) {
                        break;
                    }
                    p += 1;
                }
                if p + 1 >= d.len() {
                    return None;
                }
                if d[p + 1] == 0xD9 {
                    return Some(p + 2);
                }
            }
            _ => {
                let len = u16::from_be_bytes([d[p + 2], d[p + 3]]) as usize;
                if len < 2 {
                    return None;
                }
                p += 2 + len;
            }
        }
    }
}

// ---------------------------------------------------------------- vidéos

/// Une image clé de la piste vidéo, décodée en H.264 ou HEVC.
fn video(path: &Path) -> Result<DynamicImage> {
    let moov = crate::date::read_moov(path).ok_or("vidéo : métadonnées introuvables")?;
    let track = video_track(&moov).ok_or("vidéo : pas de piste vidéo")?;
    let sample = track.keyframe().ok_or("vidéo : pas d'image clé")?;

    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut data = vec![0u8; sample.1 as usize];
    f.seek(SeekFrom::Start(sample.0)).and_then(|_| f.read_exact(&mut data)).map_err(|e| e.to_string())?;

    // On repasse au format « Annex B » (codes de début) qu'attendent les deux décodeurs.
    let mut stream = Vec::new();
    for ps in &track.params {
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(ps);
    }
    let mut p = 0;
    while p + track.length_size <= data.len() {
        let n = data[p..p + track.length_size].iter().fold(0usize, |a, b| (a << 8) | *b as usize);
        p += track.length_size;
        if n == 0 || p + n > data.len() {
            break;
        }
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&data[p..p + n]);
        p += n;
    }

    match track.codec {
        Codec::H264 => {
            let mut dec = rust_h264::decoder::Decoder::new();
            let mut frame = None;
            for nal in &rust_h264::nal::parse_annex_b(&stream) {
                if let Ok(Some(fr)) = dec.decode_nal(nal) {
                    frame = Some(fr);
                }
            }
            let fr = frame.or_else(|| dec.flush()).ok_or("vidéo H.264 : image non décodée")?;
            Ok(yuv_to_rgb(fr.width, fr.height, &fr.y, &fr.u, &fr.v, 8))
        }
        Codec::Hevc => {
            let mut dec = rust_h265::Decoder::new();
            let mut frame = None;
            for nal in &rust_h265::parse_annex_b(&stream) {
                if let Ok(Some(fr)) = dec.decode_nal(nal) {
                    frame = Some(fr);
                }
            }
            let fr = frame.or_else(|| dec.flush()).ok_or("vidéo HEVC : image non décodée")?;
            let plane = |p: &rust_h265::PixelData| -> Vec<u8> {
                match p {
                    rust_h265::PixelData::U8(v) => v.clone(),
                    rust_h265::PixelData::U16(v) => v.iter().map(|&s| (s >> (fr.bit_depth - 8)) as u8).collect(),
                }
            };
            Ok(yuv_to_rgb(fr.width, fr.height, &plane(&fr.y), &plane(&fr.u), &plane(&fr.v), fr.bit_depth))
        }
    }
}

/// YUV 4:2:0 « TV » (BT.709, comme les vidéos de téléphone) → RGB,
/// en sautant des pixels au passage : on n'a besoin que d'une miniature.
fn yuv_to_rgb(w: u32, h: u32, y: &[u8], u: &[u8], v: &[u8], _bits: u8) -> DynamicImage {
    let (w, h) = (w as usize, h as usize);
    let cw = w.div_ceil(2);
    let step = (w.min(h) / (2 * crate::thumbs::SIZE as usize)).max(1);
    let (ow, oh) = (w / step, h / step);
    let mut out = RgbImage::new(ow as u32, oh as u32);
    for orow in 0..oh {
        for ocol in 0..ow {
            let (row, col) = (orow * step, ocol * step);
            let yy = *y.get(row * w + col).unwrap_or(&16) as f32 - 16.0;
            let ci = (row / 2) * cw + col / 2;
            let uu = *u.get(ci).unwrap_or(&128) as f32 - 128.0;
            let vv = *v.get(ci).unwrap_or(&128) as f32 - 128.0;
            let c = 1.164 * yy;
            let px = [c + 1.793 * vv, c - 0.213 * uu - 0.533 * vv, c + 2.112 * uu];
            out.put_pixel(ocol as u32, orow as u32, image::Rgb(px.map(|x| x.clamp(0.0, 255.0) as u8)));
        }
    }
    DynamicImage::ImageRgb8(out)
}

enum Codec {
    H264,
    Hevc,
}

struct Track {
    codec: Codec,
    /// SPS/PPS (et VPS en HEVC), sans code de début.
    params: Vec<Vec<u8>>,
    length_size: usize,
    sizes: Vec<u32>,
    /// (premier bloc, échantillons par bloc), numérotés à partir de 1.
    chunks_map: Vec<(u32, u32)>,
    chunk_offsets: Vec<u64>,
    /// Numéros (à partir de 1) des images clés ; vide = toutes.
    sync: Vec<u32>,
}

impl Track {
    /// Une image clé vers 10 % de la vidéo (la toute première est souvent noire) : (position, taille).
    fn keyframe(&self) -> Option<(u64, u32)> {
        let count = self.sizes.len() as u32;
        if count == 0 {
            return None;
        }
        let target = (count / 10).max(1);
        let n = if self.sync.is_empty() {
            1
        } else {
            *self.sync.iter().min_by_key(|&&s| s.abs_diff(target)).unwrap()
        };
        self.locate(n)
    }

    fn locate(&self, sample: u32) -> Option<(u64, u32)> {
        let mut first_sample = 1u32;
        for (i, &(first_chunk, per_chunk)) in self.chunks_map.iter().enumerate() {
            let next_chunk = self.chunks_map.get(i + 1).map_or(self.chunk_offsets.len() as u32 + 1, |c| c.0);
            for chunk in first_chunk..next_chunk {
                if per_chunk == 0 {
                    break;
                }
                if sample < first_sample + per_chunk {
                    let mut offset = *self.chunk_offsets.get(chunk as usize - 1)?;
                    for s in first_sample..sample {
                        offset += *self.sizes.get(s as usize - 1)? as u64;
                    }
                    return Some((offset, *self.sizes.get(sample as usize - 1)?));
                }
                first_sample += per_chunk;
            }
        }
        None
    }
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn video_track(moov: &[u8]) -> Option<Track> {
    use crate::date::children;
    let find = |d: &[u8], t: &[u8; 4]| children(d).into_iter().find(|(k, _)| k == t).map(|(_, b)| b.to_vec());
    for (t, trak) in children(moov) {
        if &t != b"trak" {
            continue;
        }
        let mdia = find(trak, b"mdia")?;
        let hdlr = find(&mdia, b"hdlr")?;
        if hdlr.get(8..12)? != b"vide" {
            continue;
        }
        let stbl = find(&find(&mdia, b"minf")?, b"stbl")?;

        // Description du codec : avc1 + avcC, ou hvc1/hev1 + hvcC.
        let stsd = find(&stbl, b"stsd")?;
        let entry = stsd.get(8..)?;
        let (kind, body) = children(entry).into_iter().next()?;
        let config = children(body.get(78..)?);
        let (codec, params, length_size) = match &kind {
            b"avc1" | b"avc3" => {
                let (_, c) = config.into_iter().find(|(k, _)| k == b"avcC")?;
                let (p, l) = parse_avcc(c)?;
                (Codec::H264, p, l)
            }
            b"hvc1" | b"hev1" => {
                let (_, c) = config.into_iter().find(|(k, _)| k == b"hvcC")?;
                let (p, l) = parse_hvcc(c)?;
                (Codec::Hevc, p, l)
            }
            _ => return None,
        };

        let stsz = find(&stbl, b"stsz")?;
        let (fixed, count) = (be32(&stsz, 4)?, be32(&stsz, 8)?);
        let sizes = if fixed != 0 {
            vec![fixed; count as usize]
        } else {
            (0..count as usize).map(|i| be32(&stsz, 12 + i * 4)).collect::<Option<_>>()?
        };
        let stsc = find(&stbl, b"stsc")?;
        let chunks_map = (0..be32(&stsc, 4)? as usize)
            .map(|i| Some((be32(&stsc, 8 + i * 12)?, be32(&stsc, 12 + i * 12)?)))
            .collect::<Option<_>>()?;
        let chunk_offsets = if let Some(stco) = find(&stbl, b"stco") {
            (0..be32(&stco, 4)? as usize).map(|i| be32(&stco, 8 + i * 4).map(u64::from)).collect::<Option<_>>()?
        } else {
            let co64 = find(&stbl, b"co64")?;
            (0..be32(&co64, 4)? as usize)
                .map(|i| Some(u64::from_be_bytes(co64.get(8 + i * 8..16 + i * 8)?.try_into().ok()?)))
                .collect::<Option<_>>()?
        };
        let sync = find(&stbl, b"stss")
            .and_then(|s| (0..be32(&s, 4)? as usize).map(|i| be32(&s, 8 + i * 4)).collect::<Option<_>>())
            .unwrap_or_default();
        return Some(Track { codec, params, length_size, sizes, chunks_map, chunk_offsets, sync });
    }
    None
}

fn parse_avcc(c: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    let length_size = (*c.get(4)? & 3) as usize + 1;
    let mut params = Vec::new();
    let mut p = 5;
    for count_mask in [0x1f, 0xff] {
        let n = (*c.get(p)? & count_mask) as usize;
        p += 1;
        for _ in 0..n {
            let len = u16::from_be_bytes(c.get(p..p + 2)?.try_into().ok()?) as usize;
            params.push(c.get(p + 2..p + 2 + len)?.to_vec());
            p += 2 + len;
        }
    }
    Some((params, length_size))
}

fn parse_hvcc(c: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    let length_size = (*c.get(21)? & 3) as usize + 1;
    let arrays = *c.get(22)? as usize;
    let mut params = Vec::new();
    let mut p = 23;
    for _ in 0..arrays {
        let n = u16::from_be_bytes(c.get(p + 1..p + 3)?.try_into().ok()?) as usize;
        p += 3;
        for _ in 0..n {
            let len = u16::from_be_bytes(c.get(p..p + 2)?.try_into().ok()?) as usize;
            params.push(c.get(p + 2..p + 2 + len)?.to_vec());
            p += 2 + len;
        }
    }
    Some((params, length_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_cache_dans_un_fichier() {
        // Un faux RAW : des octets quelconques, un petit JPEG, encore des octets.
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 48, image::Rgb([200, 30, 30])))
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let mut raw = vec![0x42u8; 1000];
        raw.extend_from_slice(&jpeg);
        raw.extend(vec![0x17u8; 500]);
        let found = embedded_jpegs(&raw);
        assert_eq!(found, vec![(1000, 1000 + jpeg.len())]);
    }
}
