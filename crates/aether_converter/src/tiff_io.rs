//! Positioned-read I/O for the streaming ingest path, and a direct decoder for
//! the commonest GeoTIFF chunk encodings.
//!
//! The `tiff` crate parses a header through `Read + Seek`, and every seek it
//! makes discards a `BufReader`'s buffer — a 64 KB buffer turned one header
//! into ~520 KB of reads. [`BlockReader`] instead serves the parse from a few
//! cached 16 KB blocks fetched with positioned reads, so the IFD and its tag
//! data cost a handful of small reads wherever they sit in the file.
//!
//! [`DirectChunks`] records, at header time, everything a chunk decode needs
//! (offsets, byte counts, codec, predictor, sample type, byte order), so a
//! row band reads exactly the bytes of the chunks it needs — one positioned
//! read per run of contiguous chunks — and decodes them without re-parsing
//! the IFD. It mirrors `tiff` 0.11.3 step for step (codec configuration,
//! byte-order fix, predictor arithmetic); anything it does not cover, and any
//! chunk it cannot decode, goes back to the `tiff` decoder, so a file decodes
//! to the same samples — or fails with the same error — either way. LZW
//! chunks go through an own decoder ([`lzw`], about three times weezl's
//! speed) wherever it fills the chunk exactly as weezl would; it hands every
//! other stream to the weezl decode `tiff` uses, whose verdict then stands.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use tiff::decoder::DecodingResult;
use tiff::tags::ByteOrder;

mod lzw;

/// One positioned read: as many bytes of `buf` as a single call returns,
/// starting at `offset`, without touching any shared file cursor.
#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// No positioned-read primitive (e.g. wasm32-unknown-unknown, where file I/O
/// fails anyway): seek + read on the handle. Callers never share a handle
/// between threads, so the moved cursor is harmless.
#[cfg(not(any(unix, windows)))]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

thread_local! {
    /// Bytes [`read_full_at`] has read on this thread (diagnostics only).
    static BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Bytes read from source files on this thread so far — every positioned
/// read of the ingest path goes through [`read_full_at`]. A caller takes the
/// difference around a call that stays on one thread (the per-tile
/// `[Rust] tile …` diagnostic line).
pub(crate) fn thread_bytes_read() -> u64 {
    BYTES_READ.with(|c| c.get())
}

/// Fill `buf` from `offset` with positioned reads; returns the bytes read,
/// fewer than `buf.len()` only at end of file.
pub(crate) fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match pread(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    BYTES_READ.with(|c| c.set(c.get() + done as u64));
    Ok(done)
}

/// Block size of [`BlockReader`].
pub(crate) const BLOCK: usize = 16 * 1024;
/// Blocks a [`BlockReader`] keeps; a header parse touches one or two.
const MAX_BLOCKS: usize = 8;

/// `Read + Seek` over a [`File`] in cached 16 KB blocks, fetched with
/// positioned reads. Seeking is free (it only moves `pos`); a read the cache
/// cannot serve fetches the block it starts in. Reads of two blocks or more
/// that miss the cache (chunk data on the `tiff`-decoder fallback) go straight
/// to the file in one call.
pub(crate) struct BlockReader {
    file: File,
    pos: u64,
    /// `(block index, bytes)`, most recently fetched last; a block shorter
    /// than [`BLOCK`] ends at end of file.
    blocks: Vec<(u64, Box<[u8]>)>,
}

impl BlockReader {
    pub(crate) fn new(file: File) -> Self {
        BlockReader { file, pos: 0, blocks: Vec::with_capacity(MAX_BLOCKS) }
    }

    /// Index into `blocks` of block `idx`, fetching it on a miss.
    fn block(&mut self, idx: u64) -> io::Result<usize> {
        if let Some(i) = self.blocks.iter().position(|b| b.0 == idx) {
            return Ok(i);
        }
        let mut buf = vec![0u8; BLOCK];
        let n = read_full_at(&self.file, &mut buf, idx * BLOCK as u64)?;
        buf.truncate(n);
        if self.blocks.len() >= MAX_BLOCKS {
            self.blocks.remove(0);
        }
        self.blocks.push((idx, buf.into_boxed_slice()));
        Ok(self.blocks.len() - 1)
    }

    /// Fetch block 0 now (the caller sniffs the magic from it) and return
    /// its first bytes.
    pub(crate) fn head(&mut self, n: usize) -> io::Result<&[u8]> {
        let i = self.block(0)?;
        let b = &self.blocks[i].1;
        Ok(&b[..n.min(b.len())])
    }
}

impl Read for BlockReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let idx = self.pos / BLOCK as u64;
        let off = (self.pos % BLOCK as u64) as usize;
        if buf.len() >= 2 * BLOCK && !self.blocks.iter().any(|b| b.0 == idx) {
            let n = read_full_at(&self.file, buf, self.pos)?;
            self.pos += n as u64;
            return Ok(n);
        }
        let i = self.block(idx)?;
        let data = &self.blocks[i].1;
        if off >= data.len() {
            return Ok(0); // end of file
        }
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for BlockReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.file.metadata()?.len().checked_add_signed(d),
        };
        match new {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid seek to a negative or overflowing position")),
        }
    }
}

/// Chunk codecs [`DirectChunks`] decodes itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    None,
    Lzw,
    Deflate,
}

/// TIFF predictors (tag 317): 1, 2 (horizontal) and 3 (floating point).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pred {
    None,
    Horizontal,
    Float,
}

/// Sample types [`DirectChunks`] decodes; each maps to the [`DecodingResult`]
/// variant the `tiff` crate returns for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sample {
    U8,
    U16,
    I16,
    I32,
    F32,
    F64,
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::U8 => 1,
            Sample::U16 | Sample::I16 => 2,
            Sample::I32 | Sample::F32 => 4,
            Sample::F64 => 8,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Sample::F32 | Sample::F64)
    }
}

/// Everything needed to decode a chunk of one single-band GeoTIFF without
/// its IFD, recorded once at header time.
#[derive(Debug)]
pub(crate) struct DirectChunks {
    pub(crate) codec: Codec,
    pub(crate) pred: Pred,
    pub(crate) sample: Sample,
    /// The file's byte order (samples; the floating-point predictor's byte
    /// planes are MSB-first whatever this says).
    pub(crate) byte_order: ByteOrder,
    pub(crate) offsets: Vec<u64>,
    pub(crate) byte_counts: Vec<u64>,
    /// Stored samples per chunk row: the tile width (padding included), or
    /// the image width for strips.
    pub(crate) row_samples: usize,
}

/// Header facts [`DirectChunks::from_header`] decides on.
pub(crate) struct ChunkHeader {
    pub(crate) compression: u16,
    pub(crate) predictor: u16,
    pub(crate) photometric: Option<u16>,
    pub(crate) samples_per_pixel: u16,
    pub(crate) bits_per_sample: Vec<u16>,
    pub(crate) sample_format: Vec<u16>,
    pub(crate) planar_config: u16,
    pub(crate) byte_order: ByteOrder,
    pub(crate) offsets: Vec<u64>,
    pub(crate) byte_counts: Vec<u64>,
    pub(crate) row_samples: usize,
    /// Chunks the image grid needs (`across * down`).
    pub(crate) chunks: usize,
}

impl DirectChunks {
    /// The direct decoder for a file with header *h*, or `None` when any part
    /// of it is outside what this decoder mirrors (other codecs, sub-byte or
    /// mixed depths, several samples, WhiteIsZero, sample types without a
    /// fast conversion, inconsistent chunk tables) — such files keep the
    /// `tiff`-decoder path.
    pub(crate) fn from_header(h: ChunkHeader) -> Option<DirectChunks> {
        let codec = match h.compression {
            1 => Codec::None,
            5 => Codec::Lzw,
            8 | 32946 => Codec::Deflate,
            _ => return None,
        };
        // BlackIsZero only (WhiteIsZero is inverted by `tiff`), one sample:
        // the planar configuration is then immaterial.
        if h.photometric != Some(1) || h.samples_per_pixel != 1 {
            return None;
        }
        let bits = match h.bits_per_sample.as_slice() {
            [b] => *b,
            _ => return None,
        };
        let format = match h.sample_format.as_slice() {
            [] => 1,
            [f] => *f,
            _ => return None,
        };
        if !matches!(h.planar_config, 1 | 2) {
            return None;
        }
        let sample = match (format, bits) {
            (1, 8) => Sample::U8,
            (1, 16) => Sample::U16,
            (2, 16) => Sample::I16,
            (2, 32) => Sample::I32,
            (3, 32) => Sample::F32,
            (3, 64) => Sample::F64,
            _ => return None,
        };
        let pred = match h.predictor {
            1 => Pred::None,
            2 => Pred::Horizontal,
            3 if sample.is_float() => Pred::Float,
            _ => return None, // `tiff` refuses it; let it say so
        };
        if h.row_samples == 0
            || h.offsets.len() != h.byte_counts.len()
            || h.offsets.len() < h.chunks
        {
            return None;
        }
        Some(DirectChunks {
            codec,
            pred,
            sample,
            byte_order: h.byte_order,
            offsets: h.offsets,
            byte_counts: h.byte_counts,
            row_samples: h.row_samples,
        })
    }

    fn row_bytes(&self) -> usize {
        self.row_samples * self.sample.bytes()
    }

    /// The file range `(offset, len)` a decode of chunk `idx` with `rows`
    /// data rows reads. Uncompressed chunks are read for exactly their rows —
    /// as `tiff` does, whatever the byte count says.
    pub(crate) fn extent(&self, idx: u32, rows: usize) -> (u64, u64) {
        let off = self.offsets[idx as usize];
        match self.codec {
            Codec::None => (off, (self.row_bytes() * rows) as u64),
            _ => (off, self.byte_counts[idx as usize]),
        }
    }

    /// Decode one chunk's `rows` stored rows (full stored width) from its
    /// file bytes *raw*. `None` when the bytes do not decode to that many
    /// rows; the caller then asks the `tiff` decoder, which gives its own
    /// verdict (and error message) on the chunk.
    pub(crate) fn decode(&self, raw: &[u8], rows: usize, lzw: &mut Option<weezl::decode::Decoder>) -> Option<DecodingResult> {
        let n = self.row_samples * rows;
        let mut out = match self.sample {
            Sample::U8 => DecodingResult::U8(vec![0; n]),
            Sample::U16 => DecodingResult::U16(vec![0; n]),
            Sample::I16 => DecodingResult::I16(vec![0; n]),
            Sample::I32 => DecodingResult::I32(vec![0; n]),
            Sample::F32 => DecodingResult::F32(vec![0.0; n]),
            Sample::F64 => DecodingResult::F64(vec![0.0; n]),
        };
        let bytes: &mut [u8] = match &mut out {
            DecodingResult::U8(v) => v,
            DecodingResult::U16(v) => bytemuck::cast_slice_mut(v),
            DecodingResult::I16(v) => bytemuck::cast_slice_mut(v),
            DecodingResult::I32(v) => bytemuck::cast_slice_mut(v),
            DecodingResult::F32(v) => bytemuck::cast_slice_mut(v),
            DecodingResult::F64(v) => bytemuck::cast_slice_mut(v),
            _ => unreachable!("allocated above"),
        };
        match self.codec {
            Codec::None => bytes.copy_from_slice(raw.get(..bytes.len())?),
            Codec::Lzw => {
                // The fast decoder takes every chunk it can vouch for; weezl
                // decides on the rest (truncated, corrupt or unusual streams)
                // exactly as before.
                if !self::lzw::fill(raw, bytes) {
                    lzw_weezl(lzw, raw, bytes)?;
                }
            }
            Codec::Deflate => {
                // tiff's `DeflateReader` is `flate2::read::ZlibDecoder`.
                flate2::read::ZlibDecoder::new(raw).read_exact(bytes).ok()?;
            }
        }
        let bits = (self.sample.bytes() * 8) as u8;
        let mut scratch = Vec::new();
        for row in bytes.chunks_exact_mut(self.row_bytes()) {
            fix_endianness_and_predict(row, bits, self.byte_order, self.pred, &mut scratch);
        }
        Some(out)
    }
}

/// Fill *out* from the LZW stream *raw* with weezl, configured as tiff 0.11.3
/// stream.rs configures it (`LZWReader::new`, verbatim) and built on first
/// use into *lzw*; `None` unless *out* fills (see [`lzw_fill`]).
fn lzw_weezl(lzw: &mut Option<weezl::decode::Decoder>, raw: &[u8], out: &mut [u8]) -> Option<()> {
    let dec = lzw.get_or_insert_with(|| {
        weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .with_yield_on_full_buffer(true)
            .build()
    });
    dec.reset();
    lzw_fill(dec, raw, out)
}

/// Fill *out* from the LZW stream *raw*; `None` unless it fills completely
/// without a decode error (on `NoProgress`, `Done` or an error short of a
/// full buffer the `tiff` path decides).
fn lzw_fill(dec: &mut weezl::decode::Decoder, raw: &[u8], out: &mut [u8]) -> Option<()> {
    let (mut ip, mut op) = (0usize, 0usize);
    while op < out.len() {
        let r = dec.decode_bytes(&raw[ip..], &mut out[op..]);
        ip += r.consumed_in;
        op += r.consumed_out;
        match r.status {
            Ok(weezl::LzwStatus::Ok) => {
                if r.consumed_in == 0 && r.consumed_out == 0 {
                    return None;
                }
            }
            Ok(weezl::LzwStatus::Done) | Ok(weezl::LzwStatus::NoProgress) => break,
            Err(_) => return None,
        }
    }
    (op == out.len()).then_some(())
}

// ── tiff 0.11.3 decoder/mod.rs, mirrored for one sample per pixel ─────────

/// `fix_endianness_and_predict` for `samples == 1`.
fn fix_endianness_and_predict(row: &mut [u8], bits: u8, byte_order: ByteOrder, pred: Pred, scratch: &mut Vec<u8>) {
    match pred {
        Pred::None => fix_endianness(row, byte_order, bits),
        Pred::Horizontal => {
            fix_endianness(row, byte_order, bits);
            rev_hpredict(row, bits);
        }
        Pred::Float => {
            scratch.clear();
            scratch.extend_from_slice(row);
            match bits {
                32 => predict_float::<4>(scratch, row),
                64 => predict_float::<8>(scratch, row),
                _ => unreachable!("Pred::Float is only chosen for 32/64-bit floats"),
            }
        }
    }
}

/// Byte-swap every sample when the file's order is not the host's.
fn fix_endianness(buf: &mut [u8], byte_order: ByteOrder, bits: u8) {
    let file_little = byte_order == ByteOrder::LittleEndian;
    if file_little == cfg!(target_endian = "little") {
        return;
    }
    match bits {
        0..=8 => {}
        9..=16 => buf.chunks_exact_mut(2).for_each(|c| c.reverse()),
        17..=32 => buf.chunks_exact_mut(4).for_each(|c| c.reverse()),
        _ => buf.chunks_exact_mut(8).for_each(|c| c.reverse()),
    }
}

/// `rev_hpredict_nsamp(buf, bits, 1)`: undo horizontal differencing on the
/// native-order integer representation of each sample.
fn rev_hpredict(buf: &mut [u8], bits: u8) {
    match bits {
        0..=8 => {
            for i in 1..buf.len() {
                buf[i] = buf[i].wrapping_add(buf[i - 1]);
            }
        }
        9..=16 => {
            for i in (2..buf.len()).step_by(2) {
                let v = u16::from_ne_bytes([buf[i], buf[i + 1]]);
                let p = u16::from_ne_bytes([buf[i - 2], buf[i - 1]]);
                buf[i..i + 2].copy_from_slice(&v.wrapping_add(p).to_ne_bytes());
            }
        }
        17..=32 => {
            for i in (4..buf.len()).step_by(4) {
                let v = u32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
                let p = u32::from_ne_bytes(buf[i - 4..i].try_into().unwrap());
                buf[i..i + 4].copy_from_slice(&v.wrapping_add(p).to_ne_bytes());
            }
        }
        _ => {
            for i in (8..buf.len()).step_by(8) {
                let v = u64::from_ne_bytes(buf[i..i + 8].try_into().unwrap());
                let p = u64::from_ne_bytes(buf[i - 8..i].try_into().unwrap());
                buf[i..i + 8].copy_from_slice(&v.wrapping_add(p).to_ne_bytes());
            }
        }
    }
}

/// `predict_f32` / `predict_f64` for `samples == 1`: undo the byte-wise
/// differencing of *input*, then reassemble each sample from its `N`
/// MSB-first byte planes into *output* in native order.
fn predict_float<const N: usize>(input: &mut [u8], output: &mut [u8]) {
    for i in 1..input.len() {
        input[i] = input[i].wrapping_add(input[i - 1]);
    }
    let plane = input.len() / N;
    for (i, chunk) in output.chunks_mut(N).enumerate() {
        let mut be = [0u8; N];
        for (k, b) in be.iter_mut().enumerate() {
            *b = input[plane * k + i];
        }
        // from_be_bytes(..).to_ne_bytes() == the big-endian bytes, reordered
        // for the host.
        if cfg!(target_endian = "little") {
            be.reverse();
        }
        chunk.copy_from_slice(&be);
    }
}

#[cfg(test)]
mod tests {
    //! The direct decoder against the `tiff` crate's own decode of the same
    //! bytes: every codec x predictor x sample type x byte order, tiled with
    //! ragged right/bottom edge tiles and stripped with a short last strip.
    //! The files are written here (the `tiff` encoder has no tiles, no
    //! floating-point predictor and no big-endian output).
    use super::*;
    use std::io::Write;
    use tiff::decoder::{ChunkType, Decoder, Limits};
    use tiff::tags::Tag;

    #[derive(Clone, Copy, Debug)]
    enum Lay {
        Tiles(u32, u32),
        Strips(u32),
    }

    /// One sample's bytes in the given order, from a deterministic value.
    fn sample_bytes(s: Sample, i: usize, big: bool) -> Vec<u8> {
        let k = i as i64;
        let wave = ((k * 37) % 1000) - 400 + (k % 7) * 3; // varied, signed
        let mut b = match s {
            Sample::U8 => vec![(k * 13 % 251) as u8],
            Sample::U16 => ((k * 977 % 65_000) as u16).to_le_bytes().to_vec(),
            Sample::I16 => (wave as i16).to_le_bytes().to_vec(),
            Sample::I32 => ((wave * 70_001) as i32).to_le_bytes().to_vec(),
            Sample::F32 => ((wave as f32) * 0.37 + 1500.25).to_le_bytes().to_vec(),
            Sample::F64 => ((wave as f64) * 0.123_456_7 - 20.5).to_le_bytes().to_vec(),
        };
        if big {
            b.reverse();
        }
        b
    }

    /// Forward predictor on one stored row (file-order bytes).
    fn predict_row(row: &mut [u8], s: Sample, pred: u16, big: bool) {
        let n = s.bytes();
        match pred {
            1 => {}
            2 => {
                // Differences on the integer value of each sample (in the
                // file's byte order), last to first.
                let get = |r: &[u8], i: usize| -> u64 {
                    let mut v = [0u8; 8];
                    let mut b = r[i * n..(i + 1) * n].to_vec();
                    if big {
                        b.reverse();
                    }
                    v[..n].copy_from_slice(&b);
                    u64::from_le_bytes(v)
                };
                let count = row.len() / n;
                let vals: Vec<u64> = (0..count).map(|i| get(row, i)).collect();
                for i in (1..count).rev() {
                    let d = vals[i].wrapping_sub(vals[i - 1]);
                    let mut b = d.to_le_bytes()[..n].to_vec();
                    if big {
                        b.reverse();
                    }
                    row[i * n..(i + 1) * n].copy_from_slice(&b);
                }
            }
            3 => {
                // Byte planes MSB first, then byte-wise differencing.
                let count = row.len() / n;
                let mut planes = vec![0u8; row.len()];
                for i in 0..count {
                    let mut be = row[i * n..(i + 1) * n].to_vec();
                    if !big {
                        be.reverse();
                    }
                    for k in 0..n {
                        planes[k * count + i] = be[k];
                    }
                }
                for i in (1..planes.len()).rev() {
                    planes[i] = planes[i].wrapping_sub(planes[i - 1]);
                }
                row.copy_from_slice(&planes);
            }
            _ => unreachable!(),
        }
    }

    fn compress(codec: Codec, raw: &[u8]) -> Vec<u8> {
        match codec {
            Codec::None => raw.to_vec(),
            Codec::Lzw => weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(raw)
                .unwrap(),
            Codec::Deflate => {
                let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(raw).unwrap();
                e.finish().unwrap()
            }
        }
    }

    /// A single-band TIFF (no geo tags — only chunk decoding is under test).
    fn write(w: u32, h: u32, lay: Lay, s: Sample, codec: Codec, pred: u16, big: bool) -> Vec<u8> {
        let n = s.bytes();
        let (cw, ch, tiled) = match lay {
            Lay::Tiles(tw, th) => (tw, th, true),
            Lay::Strips(rps) => (w, rps, false),
        };
        let across = if tiled { w.div_ceil(cw) } else { 1 };
        let down = h.div_ceil(ch);
        let mut blobs = Vec::new();
        for cy in 0..down {
            for cx in 0..across {
                // Tiles store full tile rows; the last strip only its rows.
                let rows = if tiled { ch } else { ch.min(h - cy * ch) };
                let mut raw = Vec::new();
                for y in cy * ch..cy * ch + rows {
                    let mut row = Vec::new();
                    for x in cx * cw..(cx + 1) * cw {
                        if x < w && y < h {
                            row.extend(sample_bytes(s, (y * w + x) as usize, big));
                        } else {
                            row.extend(std::iter::repeat_n(0x5Au8, n)); // padding
                        }
                    }
                    predict_row(&mut row, s, pred, big);
                    raw.extend(row);
                }
                blobs.push(compress(codec, &raw));
            }
        }
        let (bits, fmt) = match s {
            Sample::U8 => (8u16, 1u16),
            Sample::U16 => (16, 1),
            Sample::I16 => (16, 2),
            Sample::I32 => (32, 2),
            Sample::F32 => (32, 3),
            Sample::F64 => (64, 3),
        };
        let comp = match codec {
            Codec::None => 1u16,
            Codec::Lzw => 5,
            Codec::Deflate => 8,
        };
        let u16b = |v: u16| if big { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
        let u32b = |v: u32| if big { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };

        let mut buf: Vec<u8> = if big { b"MM\x00\x2a".to_vec() } else { b"II\x2a\x00".to_vec() };
        buf.extend(u32b(0)); // IFD offset, patched below
        let mut offs = Vec::new();
        for b in &blobs {
            offs.push(buf.len() as u32);
            buf.extend_from_slice(b);
        }
        let counts: Vec<u32> = blobs.iter().map(|b| b.len() as u32).collect();
        let (off_tag, cnt_tag) = if tiled { (324u16, 325u16) } else { (273, 279) };
        // (tag, type, values as u32; SHORT=3 LONG=4)
        let mut entries: Vec<(u16, u16, Vec<u32>)> = vec![
            (256, 4, vec![w]),
            (257, 4, vec![h]),
            (258, 3, vec![bits as u32]),
            (259, 3, vec![comp as u32]),
            (262, 3, vec![1]),
            (277, 3, vec![1]),
            (284, 3, vec![1]),
            (317, 3, vec![pred as u32]),
            (339, 3, vec![fmt as u32]),
            (off_tag, 4, offs),
            (cnt_tag, 4, counts),
        ];
        if tiled {
            entries.push((322, 4, vec![cw]));
            entries.push((323, 4, vec![ch]));
        } else {
            entries.push((278, 4, vec![ch]));
        }
        entries.sort_by_key(|e| e.0);
        if buf.len() % 2 == 1 {
            buf.push(0);
        }
        let ifd = buf.len();
        let ifd_b = u32b(ifd as u32);
        buf[4..8].copy_from_slice(&ifd_b);
        let mut ext = ifd + 2 + entries.len() * 12 + 4;
        let mut ext_bytes = Vec::new();
        buf.extend(u16b(entries.len() as u16));
        for (tag, typ, vals) in &entries {
            let mut val = Vec::new();
            for &v in vals {
                if *typ == 3 {
                    val.extend(u16b(v as u16));
                } else {
                    val.extend(u32b(v));
                }
            }
            buf.extend(u16b(*tag));
            buf.extend(u16b(*typ));
            buf.extend(u32b(vals.len() as u32));
            if val.len() <= 4 {
                val.resize(4, 0);
                buf.extend(val);
            } else {
                buf.extend(u32b(ext as u32));
                ext += val.len();
                ext_bytes.extend(val);
            }
        }
        buf.extend(u32b(0));
        buf.extend(ext_bytes);
        buf
    }

    /// What the converter needs from the header, through the block reader.
    fn header(dec: &mut Decoder<BlockReader>) -> (Option<DirectChunks>, u32, u32, u32, u32, u32) {
        let (w, h) = dec.dimensions().unwrap();
        let tiled = matches!(dec.get_chunk_type(), ChunkType::Tile);
        let (cw, ch) = dec.chunk_dimensions();
        let (cw, across) = if tiled { (cw, w.div_ceil(cw)) } else { (w, 1) };
        let (ot, ct) = if tiled { (Tag::TileOffsets, Tag::TileByteCounts) } else { (Tag::StripOffsets, Tag::StripByteCounts) };
        let u = |dec: &mut Decoder<BlockReader>, t| dec.find_tag_unsigned::<u16>(t).ok().flatten();
        let hdr = ChunkHeader {
            compression: u(dec, Tag::Compression).unwrap_or(1),
            predictor: u(dec, Tag::Predictor).unwrap_or(1),
            photometric: u(dec, Tag::PhotometricInterpretation),
            samples_per_pixel: u(dec, Tag::SamplesPerPixel).unwrap_or(1),
            bits_per_sample: dec.find_tag_unsigned_vec(Tag::BitsPerSample).ok().flatten().unwrap_or(vec![1]),
            sample_format: dec.find_tag_unsigned_vec(Tag::SampleFormat).ok().flatten().unwrap_or_default(),
            planar_config: u(dec, Tag::PlanarConfiguration).unwrap_or(1),
            byte_order: dec.byte_order(),
            offsets: dec.get_tag_u64_vec(ot).unwrap(),
            byte_counts: dec.get_tag_u64_vec(ct).unwrap(),
            row_samples: cw as usize,
            chunks: (across * h.div_ceil(ch)) as usize,
        };
        (DirectChunks::from_header(hdr), w, h, cw, ch, across)
    }

    /// Samples of a `DecodingResult` as comparable bit patterns.
    fn bits_of(r: &DecodingResult) -> Vec<u64> {
        match r {
            DecodingResult::U8(v) => v.iter().map(|&x| x as u64).collect(),
            DecodingResult::U16(v) => v.iter().map(|&x| x as u64).collect(),
            DecodingResult::I16(v) => v.iter().map(|&x| x as u16 as u64).collect(),
            DecodingResult::I32(v) => v.iter().map(|&x| x as u32 as u64).collect(),
            DecodingResult::F32(v) => v.iter().map(|&x| x.to_bits() as u64).collect(),
            DecodingResult::F64(v) => v.iter().map(|&x| x.to_bits()).collect(),
            other => panic!("unexpected result type {:?}", std::mem::discriminant(other)),
        }
    }

    fn check(dir: &std::path::Path, lay: Lay, s: Sample, codec: Codec, pred: u16, big: bool) {
        let (w, h) = (75u32, 53u32); // ragged against 32x16 tiles and 20-row strips
        let path = dir.join(format!("{s:?}_{codec:?}_{pred}_{big}_{lay:?}.tif").replace(['(', ')', ',', ' '], "_"));
        std::fs::write(&path, write(w, h, lay, s, codec, pred, big)).unwrap();
        let what = format!("{s:?} {codec:?} predictor {pred} big-endian {big} {lay:?}");

        let mut dec = Decoder::new(BlockReader::new(File::open(&path).unwrap()))
            .unwrap()
            .with_limits(Limits::unlimited());
        let (direct, _, _, cw, ch, across) = header(&mut dec);
        let direct = direct.unwrap_or_else(|| panic!("{what}: no direct decoder"));
        let file = File::open(&path).unwrap();
        let mut lzw = None;
        let n_chunks = across * h.div_ceil(ch);
        for idx in 0..n_chunks {
            let (cx, cy) = (idx % across, idx / across);
            let (dw, dh) = (cw.min(w - cx * cw) as usize, ch.min(h - cy * ch) as usize);
            let (off, len) = direct.extent(idx, dh);
            let mut raw = vec![0u8; len as usize];
            let got = read_full_at(&file, &mut raw, off).unwrap();
            raw.truncate(got);
            let mine = direct.decode(&raw, dh, &mut lzw).unwrap_or_else(|| panic!("{what}: chunk {idx} did not decode"));
            let mine = bits_of(&mine);
            assert_eq!(mine.len(), direct.row_samples * dh, "{what}: chunk {idx} length");

            // The tiff crate's whole-chunk read (full stored rows), as
            // `ingest::decode_chunk` asks for it.
            let mut whole = DecodingResult::U8(Vec::new());
            dec.read_chunk_to_buffer(&mut whole, idx, direct.row_samples * s.bytes()).unwrap();
            assert_eq!(mine, bits_of(&whole)[..mine.len()], "{what}: chunk {idx} differs from tiff's whole-chunk read");

            // The tiff crate's own read_chunk, data samples only — where it
            // works (weezl 0.1 can fail it on padded LZW edge tiles).
            let cropped: Vec<u64> = (0..dh).flat_map(|y| mine[y * direct.row_samples..y * direct.row_samples + dw].to_vec()).collect();
            if let Ok(r) = dec.read_chunk(idx) {
                assert_eq!(cropped, bits_of(&r), "{what}: chunk {idx} differs from tiff's read_chunk");
            }
            // And the values written (pins the fixture writer too).
            let expect: Vec<u64> = (0..dh)
                .flat_map(|y| (0..dw).map(move |x| (cy as usize * ch as usize + y) * w as usize + cx as usize * cw as usize + x))
                .map(|i| {
                    let mut b = sample_bytes(s, i, false);
                    b.resize(8, 0);
                    let v = u64::from_le_bytes(b.try_into().unwrap());
                    v
                })
                .collect();
            assert_eq!(cropped, expect, "{what}: chunk {idx} differs from the samples written");
        }
    }

    #[test]
    fn the_direct_decoder_matches_the_tiff_crate_for_every_codec_predictor_and_type() {
        let dir = tempfile::tempdir().unwrap();
        let samples = [Sample::U8, Sample::U16, Sample::I16, Sample::I32, Sample::F32, Sample::F64];
        for lay in [Lay::Tiles(32, 16), Lay::Strips(20)] {
            for codec in [Codec::None, Codec::Lzw, Codec::Deflate] {
                for s in samples {
                    for pred in [1u16, 2, 3] {
                        if pred == 3 && !s.is_float() {
                            continue;
                        }
                        for big in [false, true] {
                            check(dir.path(), lay, s, codec, pred, big);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_truncated_chunk_is_left_to_the_tiff_decoder() {
        let bytes = write(40, 20, Lay::Tiles(16, 16), Sample::F32, Codec::Lzw, 3, false);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tif");
        std::fs::write(&path, &bytes).unwrap();
        let mut dec = Decoder::new(BlockReader::new(File::open(&path).unwrap())).unwrap();
        let (direct, ..) = header(&mut dec);
        let direct = direct.unwrap();
        let (off, len) = direct.extent(0, 16);
        let raw = &bytes[off as usize..(off + len) as usize];
        let mut lzw = None;
        assert!(direct.decode(raw, 16, &mut lzw).is_some());
        assert!(direct.decode(&raw[..raw.len() / 2], 16, &mut lzw).is_none(), "half a stream cannot fill a chunk");
        let uncompressed = write(40, 20, Lay::Strips(7), Sample::I16, Codec::None, 1, true);
        std::fs::write(&path, &uncompressed).unwrap();
        let mut dec = Decoder::new(BlockReader::new(File::open(&path).unwrap())).unwrap();
        let (direct, ..) = header(&mut dec);
        let direct = direct.unwrap();
        let (off, len) = direct.extent(0, 7);
        assert!(direct.decode(&uncompressed[off as usize..(off + len - 1) as usize], 7, &mut lzw).is_none());
    }

    #[test]
    fn the_block_reader_serves_reads_and_seeks_like_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.bin");
        let data: Vec<u8> = (0..(3 * BLOCK + 123)).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut r = BlockReader::new(File::open(&path).unwrap());
        for &(pos, len) in &[(0usize, 10usize), (BLOCK - 3, 9), (5, 3 * BLOCK), (3 * BLOCK + 100, 50), (2 * BLOCK, 2 * BLOCK)] {
            r.seek(SeekFrom::Start(pos as u64)).unwrap();
            let mut got = Vec::new();
            (&mut r).take(len as u64).read_to_end(&mut got).unwrap();
            let end = (pos + len).min(data.len());
            assert_eq!(got, data[pos..end], "read {len} at {pos}");
        }
        assert_eq!(r.seek(SeekFrom::End(-3)).unwrap(), data.len() as u64 - 3);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, data[data.len() - 3..]);
        assert!(r.seek(SeekFrom::Current(-(data.len() as i64) - 1)).is_err());
    }

    // ── LZW: the fast decoder (lzw.rs) on files from real encoders ──────────

    /// Every chunk of the single-band LZW GeoTIFF at *path* through
    /// `DirectChunks::decode`, against the `tiff` crate's whole-chunk read of
    /// the same file; and through `lzw::fill` alone, against weezl, for the
    /// data rows and (tiles) all stored rows — `fill` must take every chunk,
    /// or the fast path would not be taken on such files. Returns the chunk
    /// count and the bytes `decode` produced.
    fn lzw_file_matches_the_tiff_crate(path: &std::path::Path) -> (usize, usize) {
        let what = path.display().to_string();
        let mut dec = Decoder::new(BlockReader::new(File::open(path).unwrap())).unwrap().with_limits(Limits::unlimited());
        let tiled = matches!(dec.get_chunk_type(), ChunkType::Tile);
        let (direct, _, h, _, ch, across) = header(&mut dec);
        let direct = direct.unwrap_or_else(|| panic!("{what}: no direct decoder"));
        assert_eq!(direct.codec, Codec::Lzw, "{what}");
        let file = File::open(path).unwrap();
        let (mut fallback, mut bytes) = (None, 0);
        let n_chunks = across * h.div_ceil(ch);
        for idx in 0..n_chunks {
            let dh = ch.min(h - idx / across * ch) as usize;
            let (off, len) = direct.extent(idx, dh);
            let mut raw = vec![0u8; len as usize];
            let got = read_full_at(&file, &mut raw, off).unwrap();
            raw.truncate(got);
            let mine = direct.decode(&raw, dh, &mut fallback).unwrap_or_else(|| panic!("{what}: chunk {idx} did not decode"));
            let mine = bits_of(&mine);
            let mut whole = DecodingResult::U8(Vec::new());
            dec.read_chunk_to_buffer(&mut whole, idx, direct.row_bytes()).unwrap();
            assert!(mine == bits_of(&whole)[..mine.len()], "{what}: chunk {idx} differs from tiff's whole-chunk read");
            for rows in [dh, if tiled { ch as usize } else { dh }] {
                let mut out = vec![0u8; direct.row_bytes() * rows];
                assert!(lzw::fill(&raw, &mut out), "{what}: chunk {idx} ({rows} rows) left to weezl");
                let mut reference = vec![0u8; out.len()];
                lzw_weezl(&mut None, &raw, &mut reference).unwrap_or_else(|| panic!("{what}: weezl fails chunk {idx}"));
                assert!(out == reference, "{what}: chunk {idx} ({rows} rows) differs from weezl");
            }
            bytes += direct.row_bytes() * dh;
        }
        assert!(fallback.is_none(), "{what}: decode needed weezl");
        (n_chunks as usize, bytes)
    }

    /// The fixture whose padded edge tiles trip `tiff`'s own `read_chunk`
    /// (tests/test_lzw_edge_tiles.rs), written by GDAL.
    #[test]
    fn the_lzw_edge_tile_fixture_decodes_like_the_tiff_crate() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/lzw_edge_tiles.tif");
        assert_eq!(lzw_file_matches_the_tiff_crate(&path).0, 8 * 4);
    }

    /// Writes GDAL (libtiff) LZW files into `sys.argv[1]`: u8/i16/u16/f32 x
    /// terrain/noise/NoData-block x every predictor x strips (default height
    /// and 7 rows, short last strip; 333x211) and 128/256 tiles (padded edge
    /// tiles; 500x500).
    const GDAL_LZW_FILES: &str = r#"
import sys
import numpy as np
from osgeo import gdal
gdal.UseExceptions()
out = sys.argv[1]
rng = np.random.default_rng(1312)
types = {'u8': (gdal.GDT_Byte, np.uint8), 'i16': (gdal.GDT_Int16, np.int16),
         'u16': (gdal.GDT_UInt16, np.uint16), 'f32': (gdal.GDT_Float32, np.float32)}
def data(kind, w, h, t):
    y, x = np.mgrid[0:h, 0:w].astype(np.float64)
    z = 800 + 300 * np.sin(x / 41.0) * np.cos(y / 67.0) + 0.4 * x - 0.2 * y + rng.normal(0, 0.3, (h, w))
    if t == np.uint8:
        z = z / 6
    if kind == 'noise':
        z = rng.uniform(-1e4, 1e4, (h, w)) if t == np.float32 else rng.integers(np.iinfo(t).min, np.iinfo(t).max, (h, w), endpoint=True)
    if kind == 'nodata':
        z[h // 4:3 * h // 4, w // 5:4 * w // 5] = 0 if t in (np.uint8, np.uint16) else -9999
    if t == np.float32:
        return np.round(z, 2).astype(np.float32)
    return np.clip(np.round(z), np.iinfo(t).min, np.iinfo(t).max).astype(t)
layouts = {'strips': [], 'strips7': ['BLOCKYSIZE=7'],
           'tiles128': ['TILED=YES', 'BLOCKXSIZE=128', 'BLOCKYSIZE=128'],
           'tiles256': ['TILED=YES', 'BLOCKXSIZE=256', 'BLOCKYSIZE=256']}
for name, (gdt, t) in types.items():
    for kind in ('terrain', 'noise', 'nodata'):
        for pred in ((1, 2, 3) if t == np.float32 else (1, 2)):
            for lay, opts in layouts.items():
                w, h = (500, 500) if lay.startswith('tiles') else (333, 211)
                ds = gdal.GetDriverByName('GTiff').Create(f'{out}/{name}_{kind}_p{pred}_{lay}.tif', w, h, 1, gdt,
                                                          ['COMPRESS=LZW', f'PREDICTOR={pred}'] + opts)
                ds.GetRasterBand(1).WriteArray(data(kind, w, h, t))
                ds = None
"#;

    /// libtiff's LZW encoder is where real GeoTIFFs come from.
    #[test]
    #[ignore = "needs python3 with GDAL (osgeo) and numpy: cargo test -p aether_converter --lib gdal_lzw -- --ignored"]
    fn gdal_lzw_files_decode_like_the_tiff_crate() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(GDAL_LZW_FILES)
            .arg(dir.path())
            .status()
            .expect("python3 runs");
        assert!(status.success(), "the GDAL script failed");
        let mut paths: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
        paths.sort();
        assert_eq!(paths.len(), 108);
        let (mut chunks, mut bytes) = (0, 0);
        for path in &paths {
            let (c, b) = lzw_file_matches_the_tiff_crate(path);
            chunks += c;
            bytes += b;
        }
        eprintln!("GDAL LZW: {} files, {chunks} chunks, {bytes} bytes decoded, all equal", paths.len());
    }

    /// Every chunk of every LZW GeoTIFF directly in *dir*, whole (all stored
    /// rows), with its decoded size.
    fn lzw_corpus(dir: &std::path::Path) -> Vec<(Vec<u8>, usize)> {
        let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        paths.sort();
        let mut chunks = Vec::new();
        for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("tif"))) {
            let mut dec = Decoder::new(BlockReader::new(File::open(path).unwrap())).unwrap().with_limits(Limits::unlimited());
            let tiled = matches!(dec.get_chunk_type(), ChunkType::Tile);
            let (Some(direct), _, h, _, ch, across) = header(&mut dec) else { continue };
            if direct.codec != Codec::Lzw {
                continue;
            }
            let file = File::open(path).unwrap();
            for idx in 0..across * h.div_ceil(ch) {
                let rows = if tiled { ch } else { ch.min(h - idx / across * ch) } as usize;
                let (off, len) = direct.extent(idx, rows);
                let mut raw = vec![0u8; len as usize];
                let got = read_full_at(&file, &mut raw, off).unwrap();
                raw.truncate(got);
                chunks.push((raw, direct.row_bytes() * rows));
            }
        }
        chunks
    }

    /// 1024 128x128 Float32 DEM tiles (terrain plus centimetre noise),
    /// predictor 3, weezl-encoded: 64 MiB decoded.
    fn synthetic_lzw_corpus() -> Vec<(Vec<u8>, usize)> {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 40) as f32 * 0.01
        };
        (0..1024u32)
            .map(|t| {
                let (tx, ty) = ((t % 32 * 128) as f32, (t / 32 * 128) as f32);
                let mut raw = Vec::with_capacity(128 * 128 * 4);
                for y in 0..128 {
                    let mut row = Vec::with_capacity(128 * 4);
                    for x in 0..128 {
                        let (fx, fy) = (tx + x as f32, ty + y as f32);
                        let z = 900.0 + 250.0 * (fx / 97.0).sin() * (fy / 151.0).cos() + 0.3 * fx + noise();
                        row.extend_from_slice(&((z * 100.0).round() / 100.0).to_le_bytes());
                    }
                    predict_row(&mut row, Sample::F32, 3, false);
                    raw.extend(row);
                }
                (compress(Codec::Lzw, &raw), raw.len())
            })
            .collect()
    }

    /// Speed of `lzw::fill` against weezl (`lzw_weezl`) on the same chunks,
    /// best of 7 passes each, MB (10^6 bytes) of decoded output per second,
    /// one thread. Corpus: the LZW GeoTIFFs in `AETHER_LZW_BENCH`, else
    /// [`synthetic_lzw_corpus`]; at least 50 MB of chunks either way.
    #[test]
    #[ignore = "timing: AETHER_LZW_BENCH=<dir of LZW GeoTIFFs> cargo test --release -p aether_converter --lib lzw_speed -- --ignored --nocapture"]
    fn lzw_speed_against_weezl() {
        let (what, chunks) = match std::env::var_os("AETHER_LZW_BENCH") {
            Some(dir) => (format!("{dir:?}"), lzw_corpus(std::path::Path::new(&dir))),
            None => ("synthetic DEM tiles".to_string(), synthetic_lzw_corpus()),
        };
        let total: usize = chunks.iter().map(|c| c.1).sum();
        assert!(total >= 50_000_000, "{what}: only {total} bytes of LZW chunks");
        let mut buf = vec![0u8; chunks.iter().map(|c| c.1).max().unwrap()];
        let mut reference = buf.clone();
        let mut dec = None;
        for (i, (raw, n)) in chunks.iter().enumerate() {
            lzw_weezl(&mut dec, raw, &mut reference[..*n]).unwrap_or_else(|| panic!("{what}: weezl fails chunk {i}"));
            assert!(lzw::fill(raw, &mut buf[..*n]), "{what}: chunk {i} left to weezl");
            assert!(buf[..*n] == reference[..*n], "{what}: chunk {i} differs from weezl");
        }
        // Alternating passes, so both decoders see the same machine load.
        let (mut t_weezl, mut t_fill) = (f64::INFINITY, f64::INFINITY);
        for _ in 0..7 {
            let t = std::time::Instant::now();
            for (raw, n) in &chunks {
                lzw_weezl(&mut dec, raw, &mut buf[..*n]).unwrap();
            }
            t_weezl = t_weezl.min(t.elapsed().as_secs_f64());
            let t = std::time::Instant::now();
            for (raw, n) in &chunks {
                assert!(lzw::fill(raw, &mut buf[..*n]));
            }
            t_fill = t_fill.min(t.elapsed().as_secs_f64());
        }
        let mbs = |t: f64| total as f64 / t / 1e6;
        eprintln!(
            "{what}: {} chunks, {:.1} MB decoded; weezl {:.0} MB/s, lzw::fill {:.0} MB/s: {:.2}x",
            chunks.len(),
            total as f64 / 1e6,
            mbs(t_weezl),
            mbs(t_fill),
            t_weezl / t_fill
        );
        // The target the decoder was written for (libtiff's lead over weezl).
        if cfg!(debug_assertions) {
            eprintln!("debug build: the 1.8x target is checked in --release only");
        } else {
            assert!(t_weezl / t_fill >= 1.8, "{what}: lzw::fill is only {:.2}x weezl", t_weezl / t_fill);
        }
    }
}
