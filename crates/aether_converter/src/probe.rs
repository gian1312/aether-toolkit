//! `aether_converter probe`: the raw header facts of many GeoTIFFs, fast.
//!
//! One JSON line per listed file (schema `aether-probe/1`, CONTRACT §1.2
//! "probe"): dimensions, sample type, codec, layout, EPSG code, geotransform,
//! nodata, GDAL scale/offset, overview count and where the pixel data ends
//! (`last_chunk_end`) — read from the IFDs through [`BlockReader`] (a few
//! positioned 16 KB reads per file; pixel data is never read, and of the
//! strip/tile offset tables only the last block's entries). Header facts
//! only: no reprojection, no footprint — the caller does its own CRS work.
//!
//! Every fact reported for an `ok: true` file is the value GDAL reports for
//! it (tools/probe_vs_gdal.py checks that field by field against GDAL-written
//! files). A file this reader cannot vouch for that way — not a TIFF,
//! several samples per pixel, sub-byte or complex samples, a palette, a CRS
//! the GeoKeys define without an EPSG code, GCPs instead of a geotransform,
//! an unparseable nodata/scale — is `ok: false` with a `reason`, and the
//! caller falls back to GDAL for that one file.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::tiff_io::BlockReader;

/// The `schema` value of every line.
pub const SCHEMA: &str = "aether-probe/1";
/// IFDs followed when counting overviews (a guard against IFD loops and
/// absurd page counts, far above any real overview pyramid).
const MAX_IFDS: usize = 256;
/// Entries one IFD may declare before it is called corrupt.
const MAX_ENTRIES: u64 = 4096;
/// Bytes one tag value may need before it is called corrupt.
const MAX_TAG_BYTES: u64 = 4 << 20;

/// The default worker count: `min(64, 4 x CPUs)` — the work is open + a few
/// small reads per file, so it is latency-bound, not CPU-bound.
pub fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (4 * cpus).min(64)
}

/// Why a file is `ok: false`; the text becomes `reason`.
#[derive(Debug)]
struct Fail(String);

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Fail("truncated TIFF: an IFD or tag value lies past the end of the file".into())
        } else {
            Fail(format!("read error: {e}"))
        }
    }
}

fn fail<T>(msg: impl Into<String>) -> std::result::Result<T, Fail> {
    Err(Fail(msg.into()))
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Compression {
    pub code: u16,
    pub name: &'static str,
}

/// A nodata value: a JSON number, or `"nan"` / `"inf"` / `"-inf"`, which JSON
/// numbers cannot express (GDAL writes `nan` for Float32 DEMs routinely).
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Nodata {
    Num(f64),
    Special(&'static str),
}

/// The header facts of an `ok: true` file.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Facts {
    pub width: u64,
    pub height: u64,
    pub bands: u64,
    pub dtype: &'static str,
    pub compression: Compression,
    pub predictor: u64,
    pub tiled: bool,
    pub tile_w: u64,
    /// Tile height, or the rows per strip for a stripped file.
    pub tile_h: u64,
    pub epsg: Option<u32>,
    /// Reserved for a CRS the GeoKeys define without an EPSG code. Never
    /// filled by this version — such a file is `ok: false`.
    pub proj: Option<String>,
    /// GDAL order: `[x0, dx, rx, y0, ry, dy]`, PixelIsPoint already shifted
    /// to the corner as GDAL does.
    pub geotransform: [f64; 6],
    pub rotated: bool,
    pub pixel_is_point: bool,
    pub nodata: Option<Nodata>,
    pub scale: Option<f64>,
    pub offset: Option<f64>,
    pub overviews: u64,
    pub ifd_offset: u64,
    /// Offset + byte count of the image's LAST strip/tile (the bottom-right
    /// block GDAL's truncation test reads); 0 when that block is sparse
    /// (byte count 0, which GDAL reads as empty). Greater than the file size
    /// = the pixel data is cut off. `None` when the offset/byte-count arrays
    /// cannot be read (absent, shorter than the block count, not unsigned
    /// integers, or lying past the end of the file).
    pub last_chunk_end: Option<u64>,
}

#[derive(Serialize)]
struct Line<'a> {
    schema: &'static str,
    path: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    size: Option<u64>,
    mtime: Option<i64>,
    mtime_ns: Option<i64>,
    #[serde(flatten)]
    facts: Option<Facts>,
}

/// `(mtime, mtime_ns)` the way Python's `os.stat` reports them:
/// `int(st_mtime)` (seconds through the float, as the plugin's fingerprint
/// computes it) and `st_mtime_ns`.
fn mtimes(modified: io::Result<SystemTime>) -> (Option<i64>, Option<i64>) {
    let Ok(t) = modified else { return (None, None) };
    let (sec, nsec) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i64),
        Err(e) => {
            let d = e.duration();
            let (s, n) = (d.as_secs() as i64, d.subsec_nanos() as i64);
            if n == 0 { (-s, 0) } else { (-s - 1, 1_000_000_000 - n) }
        }
    };
    let ns = sec.checked_mul(1_000_000_000).and_then(|v| v.checked_add(nsec));
    let float_s = sec as f64 + nsec as f64 * 1e-9;
    (Some(float_s.trunc() as i64), ns)
}

/// One TIFF directory entry, value field undecoded.
#[derive(Clone, Copy, Debug)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u64,
    raw: [u8; 8],
}

/// Bytes per value of a TIFF field type.
fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

struct Tiff {
    r: BlockReader,
    le: bool,
    big: bool,
}

impl Tiff {
    fn bytes_at(&mut self, off: u64, n: usize) -> std::result::Result<Vec<u8>, Fail> {
        let mut b = vec![0u8; n];
        self.r.seek(SeekFrom::Start(off))?;
        self.r.read_exact(&mut b)?;
        Ok(b)
    }

    fn u16(&self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }

    fn u64(&self, b: &[u8]) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[..8]);
        if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    }

    /// The entries of the IFD at *off* and the offset of the next IFD.
    fn ifd(&mut self, off: u64) -> std::result::Result<(Vec<Entry>, u64), Fail> {
        let (cnt_len, ent_len, next_len) = if self.big { (8, 20, 8) } else { (2, 12, 4) };
        let head = self.bytes_at(off, cnt_len)?;
        let n = if self.big { self.u64(&head) } else { self.u16(&head) as u64 };
        if n == 0 || n > MAX_ENTRIES {
            return fail(format!("corrupt TIFF: the IFD at offset {off} declares {n} entries"));
        }
        let n = n as usize;
        let body = self.bytes_at(off + cnt_len as u64, n * ent_len + next_len)?;
        let mut entries = Vec::with_capacity(n);
        for e in body[..n * ent_len].chunks_exact(ent_len) {
            let mut raw = [0u8; 8];
            let count = if self.big {
                raw.copy_from_slice(&e[12..20]);
                self.u64(&e[4..12])
            } else {
                raw[..4].copy_from_slice(&e[8..12]);
                self.u32(&e[4..8]) as u64
            };
            entries.push(Entry { tag: self.u16(&e[0..2]), typ: self.u16(&e[2..4]), count, raw });
        }
        let nb = &body[n * ent_len..];
        let next = if self.big { self.u64(nb) } else { self.u32(nb) as u64 };
        Ok((entries, next))
    }

    /// The bytes of the first `max_n` values of *e* (inline or out of line).
    fn value_bytes(&mut self, e: &Entry, max_n: u64) -> std::result::Result<Vec<u8>, Fail> {
        let Some(size) = type_size(e.typ) else {
            return fail(format!("corrupt TIFF: tag {} has unknown field type {}", e.tag, e.typ));
        };
        let total = e.count.checked_mul(size);
        let want = e.count.min(max_n) * size;
        if want > MAX_TAG_BYTES {
            return fail(format!("corrupt TIFF: tag {} needs {want} bytes", e.tag));
        }
        let inline_cap = if self.big { 8 } else { 4 };
        if total.is_some_and(|t| t <= inline_cap) {
            return Ok(e.raw[..want as usize].to_vec());
        }
        let off = if self.big { self.u64(&e.raw) } else { self.u32(&e.raw[..4]) as u64 };
        self.bytes_at(off, want as usize)
    }

    fn uints(&mut self, e: &Entry, max_n: u64) -> std::result::Result<Vec<u64>, Fail> {
        let b = self.value_bytes(e, max_n)?;
        Ok(match e.typ {
            1 | 7 => b.iter().map(|&x| x as u64).collect(),
            3 => b.chunks_exact(2).map(|c| self.u16(c) as u64).collect(),
            4 | 13 => b.chunks_exact(4).map(|c| self.u32(c) as u64).collect(),
            16 | 18 => b.chunks_exact(8).map(|c| self.u64(c)).collect(),
            _ => return fail(format!("corrupt TIFF: tag {} is field type {}, not an unsigned integer", e.tag, e.typ)),
        })
    }

    /// Value *i* of the unsigned-integer array *e*: one positioned read of
    /// that value alone (none when the array is inline).
    fn uint_at(&mut self, e: &Entry, i: u64) -> std::result::Result<u64, Fail> {
        let size = match e.typ {
            1 | 7 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return fail(format!("corrupt TIFF: tag {} is field type {}, not an unsigned integer", e.tag, e.typ)),
        };
        if i >= e.count {
            return fail(format!("corrupt TIFF: tag {} has {} values, no value {i}", e.tag, e.count));
        }
        let inline_cap = if self.big { 8 } else { 4 };
        let b = if e.count.checked_mul(size).is_some_and(|t| t <= inline_cap) {
            e.raw[(i * size) as usize..((i + 1) * size) as usize].to_vec()
        } else {
            let base = if self.big { self.u64(&e.raw) } else { self.u32(&e.raw[..4]) as u64 };
            let Some(at) = i.checked_mul(size).and_then(|o| base.checked_add(o)) else {
                return fail(format!("corrupt TIFF: tag {} points past any file", e.tag));
            };
            self.bytes_at(at, size as usize)?
        };
        Ok(match size {
            1 => b[0] as u64,
            2 => self.u16(&b) as u64,
            4 => self.u32(&b) as u64,
            _ => self.u64(&b),
        })
    }

    fn doubles(&mut self, e: &Entry, max_n: u64) -> std::result::Result<Vec<f64>, Fail> {
        let b = self.value_bytes(e, max_n)?;
        Ok(match e.typ {
            12 => b.chunks_exact(8).map(|c| f64::from_bits(self.u64(c))).collect(),
            11 => b.chunks_exact(4).map(|c| f32::from_bits(self.u32(c)) as f64).collect(),
            _ => return fail(format!("corrupt TIFF: tag {} is field type {}, not a float", e.tag, e.typ)),
        })
    }

    /// An ASCII tag as libtiff hands it to GDAL: up to the first NUL.
    fn ascii(&mut self, e: &Entry) -> std::result::Result<String, Fail> {
        if !matches!(e.typ, 1 | 2 | 7) {
            return fail(format!("corrupt TIFF: tag {} is field type {}, not ASCII", e.tag, e.typ));
        }
        let b = self.value_bytes(e, e.count)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        Ok(String::from_utf8_lossy(&b[..end]).into_owned())
    }
}

fn find(entries: &[Entry], tag: u16) -> Option<&Entry> {
    entries.iter().find(|e| e.tag == tag)
}

fn first_uint(t: &mut Tiff, entries: &[Entry], tag: u16) -> std::result::Result<Option<u64>, Fail> {
    match find(entries, tag) {
        Some(e) => Ok(t.uints(e, 1)?.first().copied()),
        None => Ok(None),
    }
}

/// [`Facts::last_chunk_end`]: offset + byte count of block `blocks - 1` of
/// the first IFD — the image's last, bottom-right block — read from
/// TileOffsets/TileByteCounts (else StripOffsets/StripByteCounts, which
/// libtiff treats alike). `None` when those arrays cannot be read.
fn last_chunk_end(t: &mut Tiff, es: &[Entry], blocks: Option<u64>) -> Option<u64> {
    let last = blocks?.checked_sub(1)?;
    let offsets = *find(es, 324).or_else(|| find(es, 273))?;
    let counts = *find(es, 325).or_else(|| find(es, 279))?;
    let n = t.uint_at(&counts, last).ok()?;
    let off = t.uint_at(&offsets, last).ok()?;
    if n == 0 {
        return Some(0); // sparse: GDAL reads the block as empty
    }
    off.checked_add(n)
}

/// GDAL's name for a TIFF compression code (its `COMPRESSION` image-structure
/// item; `NONE` where GDAL reports none). LERC is `LERC` whatever its inner
/// codec (GDAL says `LERC_DEFLATE`/`LERC_ZSTD` from a separate tag).
fn compression_name(code: u16) -> &'static str {
    match code {
        1 => "NONE",
        2 => "CCITTRLE",
        3 => "CCITTFAX3",
        4 => "CCITTFAX4",
        5 => "LZW",
        6 => "OJPEG",
        7 => "JPEG",
        8 | 32946 => "DEFLATE",
        32773 => "PACKBITS",
        34661 => "JBIG",
        34676 => "SGILOG",
        34677 => "SGILOG24",
        34712 => "JPEG2000",
        34887 => "LERC",
        34925 => "LZMA",
        50000 => "ZSTD",
        50001 => "WEBP",
        50002 | 52546 => "JXL",
        _ => "UNKNOWN",
    }
}

/// `dtype` of a (SampleFormat, BitsPerSample) pair, GDAL's data types in
/// numpy-ish short form.
fn dtype(format: u64, bits: u64) -> Option<&'static str> {
    Some(match (format, bits) {
        (1, 8) => "u8",
        (2, 8) => "i8",
        (1, 16) => "u16",
        (2, 16) => "i16",
        (1, 32) => "u32",
        (2, 32) => "i32",
        (1, 64) => "u64",
        (2, 64) => "i64",
        (3, 32) => "f32",
        (3, 64) => "f64",
        _ => return None,
    })
}

/// The CRS the GeoKeyDirectory names, mirroring the converter's own rule
/// (ingest `epsg_from_geokeys`): ProjectedCSTypeGeoKey (3072) first, else
/// GeographicTypeGeoKey (2048). `Ok(None)`: no CRS at all (no directory, or
/// one that names no coordinate system). A coordinate system the keys
/// describe without a usable EPSG code is a failure — GDAL would build a
/// custom CRS from the parameter keys, which a code cannot carry.
fn crs_of(dir: &[u16]) -> std::result::Result<Option<u32>, Fail> {
    if dir.len() < 4 {
        return Ok(None);
    }
    let n_keys = dir[3] as usize;
    let (mut projected, mut geographic, mut model) = (None, None, None);
    let mut other_crs_keys = false;
    for i in 0..n_keys {
        let off = 4 + i * 4;
        if off + 4 > dir.len() {
            break;
        }
        let (key, location, value) = (dir[off], dir[off + 1], dir[off + 3]);
        match key {
            3072 => projected = Some((location, value)),
            2048 => geographic = Some((location, value)),
            1024 => model = Some(value),
            2049..=2999 | 3073..=3999 => other_crs_keys = true,
            _ => {}
        }
    }
    let usable = |k: Option<(u16, u16)>| match k {
        Some((0, v)) if v != 0 && v != 32767 => Some(v as u32),
        _ => None,
    };
    if projected.is_some() {
        return match usable(projected) {
            Some(code) => Ok(Some(code)),
            None => fail("user-defined CRS: ProjectedCSTypeGeoKey carries no EPSG code"),
        };
    }
    if geographic.is_some() {
        return match usable(geographic) {
            Some(code) => Ok(Some(code)),
            None => fail("user-defined CRS: GeographicTypeGeoKey carries no EPSG code"),
        };
    }
    if other_crs_keys || matches!(model, Some(1..=3)) {
        return fail("user-defined CRS: the GeoKeys describe a coordinate system without an EPSG code");
    }
    Ok(None)
}

/// Value of one attribute (`name="v"` or `name='v'`) inside an XML start tag.
fn xml_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = rest[i + name.len()..].trim_start();
        if before_ok {
            if let Some(after_eq) = after.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                let q = after_eq.chars().next()?;
                if q == '"' || q == '\'' {
                    let body = &after_eq[1..];
                    return body.find(q).map(|end| &body[..end]);
                }
            }
        }
        rest = &rest[i + name.len()..];
    }
    None
}

/// Band-1 `(scale, offset)` items of a GDAL_METADATA document, each `None`
/// when absent (the same items ingest applies).
fn gdal_scale_offset(xml: &str) -> std::result::Result<(Option<f64>, Option<f64>), Fail> {
    let (mut scale, mut offset) = (None, None);
    let mut rest = xml;
    while let Some(i) = rest.find("<Item") {
        rest = &rest[i + 5..];
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[..gt];
        if attrs.trim_end().ends_with('/') {
            rest = &rest[gt + 1..];
            continue;
        }
        let body = &rest[gt + 1..];
        let Some(end) = body.find("</Item>") else { break };
        let value = body[..end].trim();
        rest = &body[end..];
        if xml_attr(attrs, "sample") != Some("0") {
            continue;
        }
        let role = xml_attr(attrs, "role").or_else(|| xml_attr(attrs, "name")).unwrap_or("").to_ascii_lowercase();
        let slot = match role.as_str() {
            "scale" => &mut scale,
            "offset" => &mut offset,
            _ => continue,
        };
        match value.parse::<f64>() {
            Ok(v) if v.is_finite() => *slot = Some(v),
            _ => return fail(format!("GDAL_METADATA {} {value:?} is not a finite number", role.to_ascii_uppercase())),
        }
    }
    Ok((scale, offset))
}

/// GDAL_NODATA (42113) the way GDAL reads it.
fn parse_nodata(text: &str) -> std::result::Result<Nodata, Fail> {
    let s = text.trim_matches(['\0', ' ', '\t', '\r', '\n']);
    match s.parse::<f64>() {
        Ok(v) if v.is_nan() => Ok(Nodata::Special("nan")),
        Ok(v) if v == f64::INFINITY => Ok(Nodata::Special("inf")),
        Ok(v) if v == f64::NEG_INFINITY => Ok(Nodata::Special("-inf")),
        Ok(v) => Ok(Nodata::Num(v)),
        Err(_) => fail(format!("GDAL_NODATA {s:?} is not a number")),
    }
}

/// The header facts of the TIFF behind *r*.
fn probe_tiff(r: BlockReader) -> std::result::Result<Facts, Fail> {
    let mut t = Tiff { r, le: true, big: false };
    let head = t.r.head(16)?.to_vec();
    if head.len() >= 4 && &head[..4] == b"AETH" {
        return fail("not a TIFF file (an .abt tile)");
    }
    if head.len() < 8 {
        return fail("not a TIFF file");
    }
    t.le = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return fail("not a TIFF file"),
    };
    let ifd_offset = match t.u16(&head[2..4]) {
        42 => t.u32(&head[4..8]) as u64,
        43 => {
            if head.len() < 16 || t.u16(&head[4..6]) != 8 || t.u16(&head[6..8]) != 0 {
                return fail("corrupt BigTIFF header");
            }
            t.big = true;
            t.u64(&head[8..16])
        }
        _ => return fail("not a TIFF file"),
    };
    if ifd_offset == 0 {
        return fail("corrupt TIFF: no IFD");
    }
    let (es, mut next) = t.ifd(ifd_offset)?;

    let width = first_uint(&mut t, &es, 256)?.unwrap_or(0);
    let height = first_uint(&mut t, &es, 257)?.unwrap_or(0);
    if width == 0 || height == 0 {
        return fail("corrupt TIFF: no image width/height");
    }
    let bands = first_uint(&mut t, &es, 277)?.unwrap_or(1);
    if bands != 1 {
        return fail(format!("multi-band: {bands} samples per pixel"));
    }
    let photometric = first_uint(&mut t, &es, 262)?;
    if photometric == Some(3) || find(&es, 320).is_some() {
        return fail("palette (indexed colour) image");
    }
    let bits = first_uint(&mut t, &es, 258)?.unwrap_or(1);
    if bits < 8 {
        return fail(format!("sub-byte samples: {bits} bits"));
    }
    let format = first_uint(&mut t, &es, 339)?.unwrap_or(1);
    let Some(dtype) = dtype(format, bits) else {
        return fail(match format {
            5 | 6 => format!("complex samples ({bits} bits)"),
            _ => format!("unsupported sample type: {bits}-bit SampleFormat {format}"),
        });
    };
    if let Some(p) = photometric.filter(|&p| p != 1) {
        return fail(format!("photometric interpretation {p} (only BlackIsZero greyscale)"));
    }
    let code = first_uint(&mut t, &es, 259)?.unwrap_or(1);
    let code = u16::try_from(code).map_err(|_| Fail(format!("corrupt TIFF: compression {code}")))?;
    let predictor = first_uint(&mut t, &es, 317)?.unwrap_or(1);

    let tiled = find(&es, 322).is_some();
    let (tile_w, tile_h) = if tiled {
        let tw = first_uint(&mut t, &es, 322)?.unwrap_or(0);
        let th = first_uint(&mut t, &es, 323)?.unwrap_or(0);
        if tw == 0 || th == 0 {
            return fail("corrupt TIFF: tiled without a tile width/length");
        }
        (tw, th)
    } else {
        let rps = first_uint(&mut t, &es, 278)?.unwrap_or(u64::from(u32::MAX));
        (width, if rps == 0 { height } else { rps.min(height) })
    };
    // One sample per pixel, so one block per tile/strip whatever the
    // PlanarConfiguration.
    let blocks = if tiled {
        width.div_ceil(tile_w).checked_mul(height.div_ceil(tile_h))
    } else {
        Some(height.div_ceil(tile_h))
    };
    let last_chunk_end = last_chunk_end(&mut t, &es, blocks);

    // Geotransform, in GDAL's order of preference: ModelPixelScale (both
    // steps non-zero) + the first ModelTiepoint, else ModelTransformation.
    let scale_v = match find(&es, 33550) {
        Some(e) => t.doubles(e, 3)?,
        None => Vec::new(),
    };
    let (tie_n, tie) = match find(&es, 33922) {
        Some(e) => (e.count, t.doubles(e, 6)?),
        None => (0, Vec::new()),
    };
    let matrix = match find(&es, 34264) {
        Some(e) => t.doubles(e, 16)?,
        None => Vec::new(),
    };
    let geokeys: Option<Vec<u16>> = match find(&es, 34735) {
        Some(e) => {
            let v = t.uints(e, e.count)?;
            Some(v.into_iter().map(|x| x as u16).collect())
        }
        None => None,
    };
    let pixel_is_point = geokeys.as_deref().is_some_and(|dir| {
        let n = dir.get(3).copied().unwrap_or(0) as usize;
        (0..n).any(|i| dir.get(4 + i * 4..8 + i * 4).is_some_and(|k| k[0] == 1025 && k[1] == 0 && k[3] == 2))
    });

    let mut gt = if scale_v.len() >= 2 && scale_v[0] != 0.0 && scale_v[1] != 0.0 {
        if tie.len() < 6 {
            return fail("no geotransform: ModelPixelScale without a ModelTiepoint");
        }
        let dx = scale_v[0];
        // GDAL reads a negative ScaleY as north-up (it warns), not as a flip.
        let dy = if scale_v[1] < 0.0 { scale_v[1] } else { -scale_v[1] };
        [tie[3] - tie[0] * dx, dx, 0.0, tie[4] - tie[1] * dy, 0.0, dy]
    } else if matrix.len() == 16 {
        [matrix[3], matrix[0], matrix[1], matrix[7], matrix[4], matrix[5]]
    } else if tie_n >= 6 {
        return fail("GCP georeferencing (tiepoints without a pixel scale)");
    } else {
        return fail("no geotransform");
    };
    if pixel_is_point {
        gt[0] -= gt[1] * 0.5 + gt[2] * 0.5;
        gt[3] -= gt[4] * 0.5 + gt[5] * 0.5;
    }
    if gt.iter().any(|v| !v.is_finite()) {
        return fail("non-finite geotransform");
    }
    let epsg = match geokeys.as_deref() {
        Some(dir) => crs_of(dir)?,
        None => None,
    };

    let nodata = match find(&es, 42113) {
        Some(e) => Some(parse_nodata(&t.ascii(e)?)?),
        None => None,
    };
    let (scale, offset) = match find(&es, 42112) {
        Some(e) => gdal_scale_offset(&t.ascii(e)?)?,
        None => (None, None),
    };

    // Overviews: the later IFDs flagged reduced-resolution (NewSubfileType
    // bit 0) and not a mask (bit 2), with the image's sample count — what
    // GDAL lists as overviews.
    let mut overviews = 0;
    let mut seen = vec![ifd_offset];
    while next != 0 && seen.len() < MAX_IFDS && !seen.contains(&next) {
        seen.push(next);
        let (oes, nx) = t.ifd(next)?;
        let sub = first_uint(&mut t, &oes, 254)?.unwrap_or(0);
        let spp = first_uint(&mut t, &oes, 277)?.unwrap_or(1);
        if sub & 1 != 0 && sub & 4 == 0 && spp == bands {
            overviews += 1;
        }
        next = nx;
    }

    Ok(Facts {
        width,
        height,
        bands,
        dtype,
        compression: Compression { code, name: compression_name(code) },
        predictor,
        tiled,
        tile_w,
        tile_h,
        epsg,
        proj: None,
        geotransform: gt,
        rotated: gt[2] != 0.0 || gt[4] != 0.0,
        pixel_is_point,
        nodata,
        scale,
        offset,
        overviews,
        ifd_offset,
        last_chunk_end,
    })
}

/// The outcome for one file.
struct Probed {
    line: String,
    ok: bool,
    open_ns: u64,
    read_ns: u64,
}

fn probe_one(path: &str) -> Probed {
    let t0 = Instant::now();
    let opened = File::open(Path::new(path));
    let open_ns = t0.elapsed().as_nanos() as u64;
    let t1 = Instant::now();
    let (meta, result) = match opened {
        Err(e) => (std::fs::metadata(path).ok(), Err(Fail(format!("cannot open: {e}")))),
        Ok(file) => match file.metadata() {
            Err(e) => (None, Err(Fail(format!("cannot stat: {e}")))),
            Ok(m) if !m.is_file() => (Some(m), Err(Fail("not a regular file".into()))),
            Ok(m) => (Some(m), probe_tiff(BlockReader::new(file))),
        },
    };
    let read_ns = t1.elapsed().as_nanos() as u64;
    let (mtime, mtime_ns) = match &meta {
        Some(m) => mtimes(m.modified()),
        None => (None, None),
    };
    let ok = result.is_ok();
    let (reason, facts) = match result {
        Ok(f) => (None, Some(f)),
        Err(Fail(r)) => (Some(r), None),
    };
    let line = Line {
        schema: SCHEMA,
        path,
        ok,
        reason,
        size: meta.as_ref().map(|m| m.len()),
        mtime,
        mtime_ns,
        facts,
    };
    let line = serde_json::to_string(&line).expect("a probe line always serializes");
    Probed { line, ok, open_ns, read_ns }
}

/// The paths of a list: one per line, UTF-8, `\n` or `\r\n`; blank lines are
/// skipped and a leading BOM is dropped. Nothing else is trimmed.
fn parse_list(text: &str) -> Vec<String> {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// `aether_converter probe`: probe every listed file with *jobs* workers and
/// print one line per file on stdout, in list order, then the `[Probe]`
/// summary on stderr. A file that cannot be probed is an `ok: false` line,
/// never an error; only an unreadable list or stdout fails the run.
pub fn run(list: Option<&Path>, jobs: Option<usize>) -> Result<()> {
    let text = match list {
        Some(p) => {
            let bytes = std::fs::read(p).with_context(|| format!("reading probe list {:?}", p))?;
            String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("probe list {:?} is not UTF-8", p))?
        }
        None => {
            let mut bytes = Vec::new();
            io::stdin().lock().read_to_end(&mut bytes).context("reading the probe list from stdin")?;
            String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("the probe list on stdin is not UTF-8"))?
        }
    };
    let paths = parse_list(&text);
    let jobs = match jobs {
        Some(0) => anyhow::bail!("--jobs must be at least 1"),
        Some(j) => j,
        None => default_jobs(),
    };
    let stats = probe_all(&paths, jobs, &mut io::stdout().lock())?;
    eprintln!("{}", stats.summary());
    Ok(())
}

/// Totals for the `[Probe]` line.
pub struct ProbeStats {
    pub files: usize,
    pub ok: usize,
    pub elapsed_s: f64,
    pub open_ns: u64,
    pub read_ns: u64,
}

impl ProbeStats {
    /// `[Probe] {n} files, {ok} ok, {s:.2}s, open {ms:.2} ms avg, read {ms:.2} ms avg`
    pub fn summary(&self) -> String {
        let avg_ms = |ns: u64| if self.files == 0 { 0.0 } else { ns as f64 / self.files as f64 / 1e6 };
        format!(
            "[Probe] {} files, {} ok, {:.2}s, open {:.2} ms avg, read {:.2} ms avg",
            self.files,
            self.ok,
            self.elapsed_s,
            avg_ms(self.open_ns),
            avg_ms(self.read_ns)
        )
    }
}

/// Probe *paths* on *jobs* threads, writing each line to *out* in list order
/// as soon as every earlier line is out (flushed whenever no further result
/// is ready, so a reader sees lines as they finish).
fn probe_all(paths: &[String], jobs: usize, out: &mut impl Write) -> Result<ProbeStats> {
    let t0 = Instant::now();
    let next = AtomicUsize::new(0);
    let (open_ns, read_ns) = (AtomicU64::new(0), AtomicU64::new(0));
    let mut ok = 0usize;
    let workers = jobs.min(paths.len()).max(1);
    let (tx, rx) = mpsc::channel::<(usize, Probed)>();
    let printed: Result<()> = std::thread::scope(|s| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, open_ns, read_ns) = (&next, &open_ns, &read_ns);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= paths.len() {
                    break;
                }
                let p = probe_one(&paths[i]);
                open_ns.fetch_add(p.open_ns, Ordering::Relaxed);
                read_ns.fetch_add(p.read_ns, Ordering::Relaxed);
                if tx.send((i, p)).is_err() {
                    break; // the printer is gone (stdout failed): stop
                }
            });
        }
        drop(tx);
        let mut out = io::BufWriter::new(out);
        let mut pending: BTreeMap<usize, Probed> = BTreeMap::new();
        let mut want = 0usize;
        loop {
            let got = match rx.try_recv() {
                Ok(v) => v,
                Err(mpsc::TryRecvError::Empty) => {
                    out.flush()?;
                    match rx.recv() {
                        Ok(v) => v,
                        Err(_) => break,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            };
            pending.insert(got.0, got.1);
            while let Some(p) = pending.remove(&want) {
                ok += p.ok as usize;
                writeln!(out, "{}", p.line)?;
                want += 1;
            }
        }
        out.flush()?;
        Ok(())
    });
    printed?;
    Ok(ProbeStats {
        files: paths.len(),
        ok,
        elapsed_s: t0.elapsed().as_secs_f64(),
        open_ns: open_ns.into_inner(),
        read_ns: read_ns.into_inner(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parsing_takes_crlf_bom_and_blank_lines() {
        let v = parse_list("\u{feff}a.tif\r\n\r\n  \nb c.tif\nd.tif");
        assert_eq!(v, vec!["a.tif", "b c.tif", "d.tif"]);
    }

    #[test]
    fn geokeys_give_the_projected_code_first_and_refuse_user_defined() {
        // KeyDirectory header + (key, location, count, value) entries.
        let dir = |keys: &[(u16, u16)]| {
            let mut d = vec![1u16, 1, 0, keys.len() as u16];
            for &(k, v) in keys {
                d.extend_from_slice(&[k, 0, 1, v]);
            }
            d
        };
        assert_eq!(crs_of(&dir(&[(1024, 1), (1025, 1), (3072, 2056), (3076, 9001)])).unwrap(), Some(2056));
        assert_eq!(crs_of(&dir(&[(1024, 2), (2048, 4326)])).unwrap(), Some(4326));
        assert_eq!(crs_of(&dir(&[(2048, 4326), (3072, 32632)])).unwrap(), Some(32632));
        assert_eq!(crs_of(&dir(&[(1025, 1)])).unwrap(), None);
        assert!(crs_of(&dir(&[(1024, 1), (3072, 32767), (2048, 4326)])).is_err());
        assert!(crs_of(&dir(&[(1024, 2), (2048, 32767), (2050, 6326)])).is_err());
        assert!(crs_of(&dir(&[(1024, 1), (3075, 1)])).is_err());
        assert!(crs_of(&dir(&[(1024, 1)])).is_err());
    }

    #[test]
    fn nodata_and_scale_parse_like_gdal_reads_them() {
        assert_eq!(parse_nodata("-9999").unwrap(), Nodata::Num(-9999.0));
        assert_eq!(parse_nodata(" 3.5\0").unwrap(), Nodata::Num(3.5));
        assert_eq!(parse_nodata("nan").unwrap(), Nodata::Special("nan"));
        assert_eq!(parse_nodata("-inf").unwrap(), Nodata::Special("-inf"));
        assert!(parse_nodata("none").is_err());
        let xml = r#"<GDALMetadata><Item name="SCALE" sample="0" role="scale">0.1</Item><Item name="OFFSET" sample="0" role="offset">-100</Item><Item name="X">1</Item></GDALMetadata>"#;
        assert_eq!(gdal_scale_offset(xml).unwrap(), (Some(0.1), Some(-100.0)));
        assert_eq!(gdal_scale_offset("<GDALMetadata></GDALMetadata>").unwrap(), (None, None));
        assert!(gdal_scale_offset(r#"<Item name="SCALE" sample="0" role="scale">x</Item>"#).is_err());
    }

    #[test]
    fn mtime_seconds_go_through_the_float_like_python() {
        let t = UNIX_EPOCH + std::time::Duration::new(1_729_500_000, 123_456_789);
        assert_eq!(mtimes(Ok(t)), (Some(1_729_500_000), Some(1_729_500_000_123_456_789)));
        let before = UNIX_EPOCH - std::time::Duration::new(1, 500_000_000);
        assert_eq!(mtimes(Ok(before)), (Some(-1), Some(-1_500_000_000)));
    }
}
