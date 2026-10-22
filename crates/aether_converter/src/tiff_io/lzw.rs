//! TIFF 6.0 LZW (compression 5) decoded straight into the caller's buffer:
//! the fast path of [`super::DirectChunks::decode`].
//!
//! The stream format is weezl's `with_tiff_size_switch(Msb, 8)`: MSB-first
//! codes of 9 to 12 bits, ClearCode 256, EOI 257, and the TIFF "early
//! change" — the width grows as soon as the next free code is `2^w - 1`, one
//! code before it would have to. The table holds at most 4096 entries; a
//! full table takes no new ones until the next Clear.
//!
//! Every string a code >= 258 stands for has already been written to the
//! output once: it is the previous code's string followed by the first byte
//! of the code after it. So where libtiff keeps prefix chains and walks them
//! byte by byte, a table entry here is one 64-bit word: a string of up to
//! seven bytes — every literal, and most strings of noisy data — in full, to
//! go out with a single 8-byte store; a longer one as where that earlier copy
//! starts and how long it is, to be copied forward in 8-byte words. Past the
//! end of a short string the store leaves up to seven scratch bytes that the
//! next string overwrites; the last strings of the output are written
//! exactly. The decode loop is compiled once per code width, so the width is
//! a constant there. The table is kept per thread and reused for every chunk.
//!
//! [`fill`] answers `true` only when the output filled up through codes it
//! decodes exactly as weezl does. Anything else — the stream ends, breaks or
//! does something unusual before the output is full — answers `false`, and
//! the caller decodes the chunk again with weezl, whose verdict and bytes then
//! stand. The two therefore never disagree on a chunk.
//!
//! Plain portable Rust: no `unsafe`, no `std::arch`, every multi-byte value
//! read and written through `from_be_bytes`/`to_le_bytes` in the order it
//! needs, bit positions in `u64` and output offsets in 32 bits on every target.

use std::cell::RefCell;

const CLEAR: usize = 256;
#[cfg(test)]
const EOI: usize = 257;
/// The first code a table generation assigns.
const FIRST: usize = 258;
const MAX_CODES: usize = 4096;
/// Codes are at most 12 bits wide; masking one into the table states that.
const CODE_MASK: usize = MAX_CODES - 1;
/// The longest string a head word holds in full.
const INLINE: usize = 7;
/// Bit position of the length byte in a head word.
const TOP: u32 = 56;
/// The length byte of a long string.
const LONG: u64 = 8;
/// Room an inline string's store needs: a whole 8-byte word.
const WORD: usize = 8;

// A code's head word. Top byte: the string's length, capped at 8 (0 for
// Clear and EOI, which stand for no string). Bits 0..8: its first byte. A
// short string (1 to 7 bytes) has byte `i` in bits `8i..8i + 8` and zeros up
// to the top byte; a long one (8 bytes or more) has the output offset of a
// copy in bits 8..40 and its length in bits 40..53.

/// The head word of a long string: its first byte, where a copy starts in
/// the output, its length.
#[inline(always)]
fn long_head(first: u64, at: usize, len: usize) -> u64 {
    first | (at as u64) << 8 | (len as u64) << 40 | LONG << TOP
}

/// The head word of the string `prev` (`prev_len` bytes, written at `at`)
/// followed by the byte `first`.
#[inline(always)]
fn extend(prev: u64, prev_len: usize, at: usize, first: u64) -> u64 {
    if prev_len < INLINE {
        prev + (first << (8 * prev_len as u32)) + (1 << TOP)
    } else {
        long_head(prev & 0xFF, at, prev_len + 1)
    }
}

/// Where a long string's copy starts in the output, and its length.
#[inline(always)]
fn long_copy(h: u64) -> (usize, usize) {
    ((h >> 8) as u32 as usize, ((h >> 40) & 0x1FFF) as usize)
}

/// The table, per thread: one head word per code; literals preset.
type Table = Box<[u64; MAX_CODES]>;

fn new_table() -> Table {
    let mut t: Table = vec![0; MAX_CODES].into_boxed_slice().try_into().expect("MAX_CODES entries");
    for (byte, h) in t.iter_mut().take(256).enumerate() {
        *h = byte as u64 | 1 << TOP;
    }
    t
}

thread_local! {
    static TABLE: RefCell<Option<Table>> = const { RefCell::new(None) };
}

/// Fill *out* from the TIFF LZW stream *inp*. `true` when *out* is full and
/// holds exactly what weezl (configured as `DirectChunks::decode` configures
/// it, driven by its `lzw_fill`) decodes there; `false` when the stream ends,
/// breaks or does something unusual first — *out* then holds a partial decode
/// and weezl must decide on the chunk.
pub(super) fn fill(inp: &[u8], out: &mut [u8]) -> bool {
    if out.is_empty() {
        return true; // lzw_fill's answer too, without reading a code
    }
    if u32::try_from(out.len()).is_err() {
        return false; // offsets into the output are kept in 32 bits
    }
    TABLE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut table) => decode(table.get_or_insert_with(new_table), inp, out),
        // Unreachable (`fill` does not recurse), but a fresh table is as good.
        Err(_) => decode(&mut new_table(), inp, out),
    })
}

/// The `width`-bit code (`width <= 12`) at bit `pos` of *inp*, MSB first;
/// `None` when fewer bits are left. The slow way, for the first codes of a
/// generation and the last bytes of the stream.
#[cold]
#[inline(never)]
fn code_at(inp: &[u8], pos: u64, width: u32) -> Option<usize> {
    if (pos + u64::from(width)).div_ceil(8) > inp.len() as u64 {
        return None;
    }
    let at = (pos >> 3) as usize;
    let mut b = [0u8; 4];
    let n = (inp.len() - at).min(4);
    b[..n].copy_from_slice(&inp[at..at + n]);
    Some(((u32::from_be_bytes(b) << (pos & 7) as u32) >> (32 - width)) as usize)
}

/// Copy the `len`-byte string at `from` (`len >= 8`, `from + len <= to`) to
/// `to`: in 8-byte words, the last one overlapping its predecessor, or past
/// 64 bytes with one `memcpy`.
#[inline(always)]
fn copy_long(out: &mut [u8], from: usize, to: usize, len: usize) {
    let (done, rest) = out.split_at_mut(to);
    let (src, dst) = (&done[from..from + len], &mut rest[..len]);
    if len > 64 {
        dst.copy_from_slice(src);
        return;
    }
    for (d, s) in dst.chunks_exact_mut(8).zip(src.chunks_exact(8)) {
        d.copy_from_slice(s);
    }
    dst[len - 8..].copy_from_slice(&src[len - 8..]);
}

/// Write exactly what fits at `op` of the inline string `h` (`len` bytes);
/// `true` when that fills the output. For the last strings of a chunk.
#[cold]
#[inline(never)]
fn put_inline_tail(out: &mut [u8], op: usize, h: u64, len: usize) -> bool {
    let n = len.min(out.len() - op);
    out[op..op + n].copy_from_slice(&h.to_le_bytes()[..n]);
    op + len >= out.len()
}

/// Write exactly what fits at `op` of the `len`-byte string at `from`, byte
/// by byte (a KwKwK string ends in its own first byte, at `op`); `true` when
/// that fills the output. For the last strings of a chunk.
#[cold]
#[inline(never)]
fn put_copy_tail(out: &mut [u8], from: usize, op: usize, len: usize) -> bool {
    let n = len.min(out.len() - op);
    for i in 0..n {
        out[op + i] = out[from + i];
    }
    op + len >= out.len()
}

/// Decoder state between the per-width loops.
struct Run {
    /// Bits of the input consumed.
    pos: u64,
    /// Bytes of the output written.
    op: usize,
    /// The next free code.
    next: usize,
    /// The previous code's head word and string length.
    prev: u64,
    prev_len: usize,
}

/// How a decode loop ended.
enum Step {
    /// The table reached the next code width (after 12 bits: it is full).
    Widen,
    /// A Clear, and the code after it.
    Clear(usize),
    /// The output is full.
    Filled,
    /// The stream ends, breaks or does something unusual: weezl decides.
    Stop,
}

/// Decode `W`-bit codes until the table needs `W + 1` bits (or, for 12, is
/// full), a Clear, the end of the output, or a stop.
fn run<const W: u32>(t: &mut [u64; MAX_CODES], inp: &[u8], out: &mut [u8], s: &mut Run) -> Step {
    let limit = if W < 12 { (1 << W) - 1 } else { MAX_CODES };
    let Run { mut pos, mut op, mut next, mut prev, mut prev_len } = *s;
    let end = out.len();
    let step = loop {
        let at = (pos >> 3) as usize;
        let c = match inp.get(at..at + 4) {
            Some(b) => ((u32::from_be_bytes([b[0], b[1], b[2], b[3]]) << (pos & 7) as u32) >> (32 - W)) as usize,
            None => match code_at(inp, pos, W) {
                Some(c) => c,
                None => break Step::Stop,
            },
        };
        pos += u64::from(W);
        let (h, len);
        if c < next {
            h = t[c & CODE_MASK];
            let l = (h >> TOP) as usize;
            if l.wrapping_sub(1) < INLINE {
                len = l;
                if let Some(dst) = out.get_mut(op..op + WORD) {
                    dst.copy_from_slice(&h.to_le_bytes());
                } else if put_inline_tail(out, op, h, len) {
                    break Step::Filled;
                }
            } else if l != 0 {
                let from;
                (from, len) = long_copy(h);
                // A copy that reaches the end of the output goes the exact
                // way, which says so.
                if op + len < end {
                    copy_long(out, from, op, len);
                } else if put_copy_tail(out, from, op, len) {
                    break Step::Filled;
                }
            } else if c == CLEAR {
                break match code_at(inp, pos, 9) {
                    Some(c) => {
                        pos += 9;
                        Step::Clear(c)
                    }
                    None => Step::Stop,
                };
            } else {
                break Step::Stop; // EOI before the output is full
            }
        } else if c == next {
            // KwKwK: the previous string plus its own first byte — the very
            // entry this code makes.
            h = extend(prev, prev_len, op - prev_len, prev & 0xFF);
            len = prev_len + 1;
            if len <= INLINE {
                if let Some(dst) = out.get_mut(op..op + WORD) {
                    dst.copy_from_slice(&h.to_le_bytes());
                } else if put_inline_tail(out, op, h, len) {
                    break Step::Filled;
                }
            } else if op + len >= end {
                if put_copy_tail(out, op - prev_len, op, len) {
                    break Step::Filled;
                }
            } else if prev_len == INLINE {
                // Seven bytes inline and their first again.
                let w = (prev & ((1 << TOP) - 1)) | (prev & 0xFF) << TOP;
                out[op..op + WORD].copy_from_slice(&w.to_le_bytes());
            } else {
                copy_long(out, op - prev_len, op, prev_len);
                out[op + prev_len] = prev as u8;
            }
        } else {
            break Step::Stop; // beyond the table
        }
        // The entry this code makes: the previous string plus this string's
        // first byte, which now follows it in the output.
        t[next & CODE_MASK] = extend(prev, prev_len, op - prev_len, h & 0xFF);
        prev = h;
        prev_len = len;
        op += len;
        next += 1;
        if next == limit {
            break Step::Widen;
        }
    };
    *s = Run { pos, op, next, prev, prev_len };
    step
}

/// Decode with a full table (4096 entries): 12-bit codes, every one of them
/// in the table, no new entries until a Clear. weezl's encoder writes its
/// Clear right after the table fills; libtiff's before it does.
#[cold]
#[inline(never)]
fn full_table(t: &[u64; MAX_CODES], inp: &[u8], out: &mut [u8], s: &mut Run) -> Step {
    let end = out.len();
    loop {
        let Some(c) = code_at(inp, s.pos, 12) else { return Step::Stop };
        s.pos += 12;
        let h = t[c & CODE_MASK];
        let l = (h >> TOP) as usize;
        let len;
        if l.wrapping_sub(1) < INLINE {
            len = l;
            if put_inline_tail(out, s.op, h, len) {
                return Step::Filled;
            }
        } else if l != 0 {
            let from;
            (from, len) = long_copy(h);
            let n = len.min(end - s.op);
            out.copy_within(from..from + n, s.op); // from + len <= op: disjoint
            if s.op + len >= end {
                return Step::Filled;
            }
        } else if c == CLEAR {
            return match code_at(inp, s.pos, 9) {
                Some(c) => {
                    s.pos += 9;
                    Step::Clear(c)
                }
                None => Step::Stop,
            };
        } else {
            return Step::Stop; // EOI
        }
        s.op += len;
    }
}

fn decode(t: &mut [u64; MAX_CODES], inp: &[u8], out: &mut [u8]) -> bool {
    let mut pos = 0;
    let Some(mut code) = code_at(inp, pos, 9) else { return false };
    pos += 9;
    if code == CLEAR {
        // The stream's leading Clear (weezl takes a stream without one too).
        let Some(c) = code_at(inp, pos, 9) else { return false };
        pos += 9;
        code = c;
    }
    let mut op = 0;
    // One pass per table generation; `code` is its first code.
    loop {
        // Read right after a Clear or at the start, it must be a literal. A
        // second Clear in a row (on which weezl's lzw_fill may stop short,
        // depending on what its bit buffer holds), EOI, or a code the fresh
        // table does not have yet: weezl decides.
        if code >= CLEAR {
            return false;
        }
        out[op] = code as u8;
        op += 1;
        if op == out.len() {
            return true;
        }
        let mut s = Run { pos, op, next: FIRST, prev: t[code], prev_len: 1 };
        let mut step = run::<9>(t, inp, out, &mut s);
        if let Step::Widen = step {
            step = run::<10>(t, inp, out, &mut s);
        }
        if let Step::Widen = step {
            step = run::<11>(t, inp, out, &mut s);
        }
        if let Step::Widen = step {
            step = run::<12>(t, inp, out, &mut s);
        }
        if let Step::Widen = step {
            step = full_table(t, inp, out, &mut s);
        }
        match step {
            Step::Clear(c) => (code, pos, op) = (c, s.pos, s.op),
            Step::Filled => return true,
            Step::Stop | Step::Widen => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! [`fill`] against weezl as `DirectChunks::decode` drove it before
    //! (`super::super::lzw_weezl`): wherever `fill` answers `true`, weezl must
    //! fill the same buffer with the same bytes — and `fill` must answer
    //! `true` on every well-formed stream long enough for the buffer, or the
    //! fast path would not be taken. Streams come from weezl's encoder, from a
    //! test encoder with libtiff's and other Clear points (or none, or no
    //! EOI), from random code sequences, and from garbage.
    use super::*;
    use std::collections::HashMap;

    /// splitmix64: deterministic test data without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// The reference: weezl through tiff_io's `lzw_fill`.
    fn weezl(raw: &[u8], n: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; n];
        super::super::lzw_weezl(&mut None, raw, &mut out).map(|()| out)
    }

    /// Everything weezl decodes from *raw* before it stops, however it stops.
    fn weezl_all(raw: &[u8]) -> Vec<u8> {
        let mut dec = weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .with_yield_on_full_buffer(true)
            .build();
        let (mut all, mut buf, mut ip) = (Vec::new(), vec![0u8; 1 << 16], 0);
        loop {
            let r = dec.decode_bytes(&raw[ip..], &mut buf);
            ip += r.consumed_in;
            all.extend_from_slice(&buf[..r.consumed_out]);
            match r.status {
                Ok(weezl::LzwStatus::Ok) if r.consumed_in > 0 || r.consumed_out > 0 => {}
                _ => return all,
            }
        }
    }

    #[derive(Default, Debug)]
    struct Tally {
        cases: usize,
        fast: usize,
        weezl_filled: usize,
    }

    impl Tally {
        /// Compare `fill` with weezl on (*raw*, an *n*-byte buffer); whether
        /// the fast path held.
        fn check(&mut self, raw: &[u8], n: usize, what: &dyn Fn() -> String) -> bool {
            let mut out = vec![0x5A; n];
            let fast = fill(raw, &mut out);
            let reference = weezl(raw, n);
            if fast {
                match &reference {
                    None => panic!("{}: fill filled {n} bytes, weezl does not", what()),
                    Some(r) => {
                        if let Some(i) = (0..n).find(|&i| r[i] != out[i]) {
                            panic!("{}: byte {i} of {n} is {:#04x}, weezl says {:#04x}", what(), out[i], r[i]);
                        }
                    }
                }
            }
            self.cases += 1;
            self.fast += fast as usize;
            self.weezl_filled += reference.is_some() as usize;
            fast
        }
    }

    /// MSB-first code packing.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl BitWriter {
        fn put(&mut self, code: usize, width: u32) {
            assert!(code < 1 << width, "code {code} does not fit {width} bits");
            self.acc = (self.acc << width) | code as u64;
            self.n += width;
            while self.n >= 8 {
                self.n -= 8;
                self.bytes.push((self.acc >> self.n) as u8);
            }
            self.acc &= (1 << self.n) - 1;
        }

        fn finish(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.bytes.push((self.acc << (8 - self.n)) as u8);
            }
            self.bytes
        }
    }

    /// The code widths a TIFF decoder reads with, followed code by code.
    struct Widths {
        next: usize,
        width: u32,
        /// The next code is the first of a generation (it makes no entry).
        fresh: bool,
        widest: u32,
        clears: usize,
    }

    impl Widths {
        fn new() -> Widths {
            Widths { next: FIRST, width: 9, fresh: true, widest: 9, clears: 0 }
        }

        fn emit(&mut self, w: &mut BitWriter, code: usize) {
            w.put(code, self.width);
            self.widest = self.widest.max(self.width);
            if code == CLEAR {
                (self.next, self.width, self.fresh) = (FIRST, 9, true);
                self.clears += 1;
            } else if code == EOI {
            } else if self.fresh {
                self.fresh = false;
            } else if self.next < MAX_CODES {
                self.next += 1;
                if self.next == (1 << self.width) - 1 && self.width < 12 {
                    self.width += 1;
                }
            }
        }
    }

    fn weezl_encode(data: &[u8]) -> Vec<u8> {
        weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap()
    }

    /// A TIFF LZW encoder that writes its Clear when its table reaches
    /// `clear_at` codes (libtiff: 4094) or never (`None`: the decoder's
    /// table then stays full), and EOI only if asked. Returns the stream, its
    /// Clear count (the leading one included) and its widest code.
    fn encode_with(data: &[u8], clear_at: Option<usize>, eoi: bool) -> (Vec<u8>, usize, u32) {
        let mut w = BitWriter::default();
        let mut m = Widths::new();
        m.emit(&mut w, CLEAR);
        let mut dict: HashMap<(usize, u8), usize> = HashMap::new();
        let mut free = FIRST;
        if let Some((&b0, rest)) = data.split_first() {
            let mut cur = b0 as usize;
            for &b in rest {
                if let Some(&c) = dict.get(&(cur, b)) {
                    cur = c;
                    continue;
                }
                m.emit(&mut w, cur);
                if free < MAX_CODES {
                    dict.insert((cur, b), free);
                    free += 1;
                }
                cur = b as usize;
                if clear_at == Some(free) {
                    m.emit(&mut w, CLEAR);
                    dict.clear();
                    free = FIRST;
                }
            }
            m.emit(&mut w, cur);
        }
        if eoi {
            m.emit(&mut w, EOI);
        }
        (w.finish(), m.clears, m.widest)
    }

    /// Random bytes.
    fn noise(rng: &mut Rng, n: usize) -> Vec<u8> {
        (0..n).map(|_| rng.next() as u8).collect()
    }

    /// Runs of one byte, 1 to 300 long.
    fn runs(rng: &mut Rng, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            let (b, k) = (rng.next() as u8, 1 + rng.below(300));
            v.extend(std::iter::repeat_n(b, k.min(n - v.len())));
        }
        v
    }

    /// Bytes from a `k`-letter alphabet: long strings, many KwKwK codes.
    fn alphabet(rng: &mut Rng, n: usize, k: usize) -> Vec<u8> {
        (0..n).map(|_| b'a' + rng.below(k) as u8).collect()
    }

    /// Rows of a 128-wide Float32 DEM tile after the floating-point predictor
    /// (MSB-first byte planes, differenced), as swissALTI3D chunks hold them.
    fn dem_rows(rng: &mut Rng, n: usize) -> Vec<u8> {
        const W: usize = 128;
        let base = 300.0 + rng.below(3000) as f32;
        let mut v = Vec::with_capacity(n + 4 * W);
        let mut y = 0;
        while v.len() < n {
            let mut planes = vec![0u8; 4 * W];
            for x in 0..W {
                let (fx, fy) = (x as f32, y as f32);
                let z = base + 40.0 * (fx * 0.031).sin() * (fy * 0.017).cos() + 0.07 * fx + rng.below(50) as f32 * 0.01;
                for (k, b) in ((z * 100.0).round() / 100.0).to_be_bytes().into_iter().enumerate() {
                    planes[k * W + x] = b;
                }
            }
            for i in (1..planes.len()).rev() {
                planes[i] = planes[i].wrapping_sub(planes[i - 1]);
            }
            v.extend_from_slice(&planes);
            y += 1;
        }
        v.truncate(n);
        v
    }

    type Gen = fn(&mut Rng, usize) -> Vec<u8>;

    fn generators() -> Vec<(&'static str, Gen)> {
        vec![
            ("noise", noise as Gen),
            ("runs", runs),
            ("zeros", |_, n| vec![0; n]),
            ("ab", |r, n| alphabet(r, n, 2)),
            ("abcd", |r, n| alphabet(r, n, 4)),
            ("dem", dem_rows),
            ("ramp", |_, n| (0..n).map(|i| (i / 3) as u8).collect()),
        ]
    }

    const SIZES: [usize; 14] = [0, 1, 2, 7, 8, 9, 255, 256, 257, 4095, 4096, 4097, 65_536, 65_537];

    /// The buffer sizes a stream of `len` decoded bytes is checked with:
    /// empty, short, exact, and longer than the stream.
    fn buffer_sizes(len: usize) -> Vec<usize> {
        let mut v = vec![0, 1, len / 2, len.saturating_sub(1), len, len + 1, len + 4096];
        v.sort_unstable();
        v.dedup();
        v
    }

    #[test]
    fn fills_like_weezl_on_encoded_data() {
        let mut rng = Rng(1);
        let mut tally = Tally::default();
        for (name, gen) in generators() {
            for &size in &SIZES {
                let data = gen(&mut rng, size);
                let mut streams = vec![("weezl".to_string(), weezl_encode(&data))];
                // Every Clear point and EOI-less streams on the smaller
                // sizes; libtiff's Clear point and a never-full table on all.
                let variants: &[(Option<usize>, bool)] = if size <= 4097 {
                    &[(Some(4094), true), (Some(4094), false), (Some(4093), true), (Some(4095), false), (Some(MAX_CODES), true), (None, true), (None, false)]
                } else {
                    &[(Some(4094), true), (None, false)]
                };
                for &(clear_at, eoi) in variants {
                    streams.push((format!("clear_at {clear_at:?} eoi {eoi}"), encode_with(&data, clear_at, eoi).0));
                }
                for (enc, raw) in &streams {
                    assert_eq!(weezl_all(raw), data, "{name} {size} {enc}: the test encoder is broken");
                    for n in buffer_sizes(size) {
                        let fast = tally.check(raw, n, &|| format!("{name} {size} B, {enc}, {n}-byte buffer"));
                        assert_eq!(fast, n <= size, "{name} {size} B, {enc}, {n}-byte buffer: fast path held = {fast}");
                    }
                }
            }
        }
        eprintln!("encoded data: {tally:?}");
        assert!(tally.cases > 4000 && tally.fast > 3000, "{tally:?}");
    }

    #[test]
    fn streams_reach_every_code_width_and_several_clears_on_the_fast_path() {
        let mut rng = Rng(2);
        let mut tally = Tally::default();
        for (name, gen) in generators() {
            let data = gen(&mut rng, 400_000);
            for clear_at in [Some(4094), Some(MAX_CODES), None] {
                let (raw, clears, widest) = encode_with(&data, clear_at, true);
                if name == "noise" {
                    assert_eq!(widest, 12, "{name}: noise reaches 12-bit codes");
                    if clear_at.is_some() {
                        assert!(clears > 20, "{name} clear_at {clear_at:?}: {clears} Clears");
                    }
                }
                assert!(tally.check(&raw, data.len(), &|| format!("{name} clear_at {clear_at:?}")), "{name} {clear_at:?}");
            }
            assert!(tally.check(&weezl_encode(&data), data.len(), &|| format!("{name} weezl")));
        }
        eprintln!("widths and clears: {tally:?}");
    }

    #[test]
    fn truncated_streams_get_weezls_verdict() {
        let mut rng = Rng(3);
        let mut tally = Tally::default();
        for (name, gen) in generators() {
            let data = gen(&mut rng, 1500);
            for (enc, raw) in [("weezl", weezl_encode(&data)), ("libtiff-like", encode_with(&data, Some(4094), true).0)] {
                for cut in 0..raw.len() {
                    for n in [data.len(), data.len() / 3] {
                        tally.check(&raw[..cut], n, &|| format!("{name} {enc} cut at {cut} of {}, {n}-byte buffer", raw.len()));
                    }
                }
            }
        }
        eprintln!("truncated: {tally:?}");
        assert!(tally.cases > 20_000 && tally.fast > 1000 && tally.fast < tally.cases, "{tally:?}");
    }

    #[test]
    fn garbage_gets_weezls_verdict() {
        let mut rng = Rng(4);
        let mut tally = Tally::default();
        for i in 0..4000 {
            let len = rng.below(600);
            let mut raw = noise(&mut rng, len);
            if i % 2 == 0 && raw.len() >= 2 {
                // A leading Clear: garbage that gets past the first code.
                raw[0] = 0x80;
                raw[1] &= 0x7F;
            }
            for n in [1, 2, 17, 300, 5000] {
                tally.check(&raw, n, &|| format!("garbage #{i} ({len} B), {n}-byte buffer"));
            }
        }
        eprintln!("garbage: {tally:?}");
        assert!(tally.cases == 20_000 && tally.weezl_filled > 100, "{tally:?}");
    }

    /// Permille mix of a random code sequence.
    struct Mix {
        clear: usize,
        eoi: usize,
        bad: usize,
        kwkwk: usize,
        literal: usize,
    }

    /// A random code sequence at the widths a decoder reads: codes in the
    /// table (Clear and EOI among them), KwKwK codes, literals, and at the
    /// given rates Clears, EOIs and codes beyond the table.
    fn random_codes(rng: &mut Rng, count: usize, mix: &Mix) -> Vec<u8> {
        let mut w = BitWriter::default();
        let mut m = Widths::new();
        if rng.below(8) != 0 {
            m.emit(&mut w, CLEAR);
        }
        for _ in 0..count {
            let r = rng.below(1000);
            let room = (1 << m.width) - m.next; // codes beyond the table that fit the width
            let code = if r < mix.clear {
                CLEAR
            } else if r < mix.clear + mix.eoi {
                EOI
            } else if r < mix.clear + mix.eoi + mix.bad && room > 1 {
                m.next + 1 + rng.below(room - 1)
            } else if m.fresh || r < mix.clear + mix.eoi + mix.bad + mix.literal {
                rng.below(256)
            } else if r < mix.clear + mix.eoi + mix.bad + mix.literal + mix.kwkwk && m.next < MAX_CODES {
                m.next
            } else {
                rng.below(m.next.min(MAX_CODES))
            };
            m.emit(&mut w, code);
        }
        w.finish()
    }

    #[test]
    fn random_code_sequences_match_weezl() {
        let mut rng = Rng(5);
        let mut tally = Tally::default();
        let mixes = [
            Mix { clear: 0, eoi: 0, bad: 0, kwkwk: 100, literal: 200 },
            Mix { clear: 0, eoi: 0, bad: 0, kwkwk: 400, literal: 50 },
            Mix { clear: 3, eoi: 0, bad: 0, kwkwk: 100, literal: 300 },
            Mix { clear: 30, eoi: 0, bad: 0, kwkwk: 100, literal: 300 },
            Mix { clear: 300, eoi: 0, bad: 0, kwkwk: 100, literal: 300 },
            Mix { clear: 2, eoi: 1, bad: 1, kwkwk: 100, literal: 200 },
            Mix { clear: 0, eoi: 0, bad: 2, kwkwk: 100, literal: 200 },
        ];
        for (k, mix) in mixes.iter().enumerate() {
            for i in 0..120 {
                let count = [3, 50, 500, 3000, 5000, 12_000][i % 6];
                let raw = random_codes(&mut rng, count, mix);
                let len = weezl_all(&raw).len();
                for n in buffer_sizes(len) {
                    tally.check(&raw, n, &|| format!("mix {k} sequence {i} ({count} codes, {len} B), {n}-byte buffer"));
                }
            }
        }
        eprintln!("random codes: {tally:?}");
        assert!(tally.cases > 4000 && tally.fast > 1500, "{tally:?}");
    }

    #[test]
    fn a_clear_right_after_a_clear_is_left_to_weezl() {
        // weezl's lzw_fill stops on such a stream when the second Clear is
        // already in its bit buffer, and decodes on when it is not.
        let mut tally = Tally::default();
        let mut verdicts = Vec::new();
        for lead in 0..9 {
            let mut w = BitWriter::default();
            let mut m = Widths::new();
            m.emit(&mut w, CLEAR);
            for i in 0..lead {
                m.emit(&mut w, b'x' as usize + i);
            }
            m.emit(&mut w, CLEAR);
            m.emit(&mut w, CLEAR);
            for b in b"after" {
                m.emit(&mut w, *b as usize);
            }
            m.emit(&mut w, EOI);
            let raw = w.finish();
            assert!(!tally.check(&raw, lead + 5, &|| format!("{lead} codes, Clear, Clear")));
            verdicts.push(weezl(&raw, lead + 5).is_some());
        }
        // A canary like tests/test_lzw_edge_tiles.rs's: once weezl decodes
        // every one of these, `decode` may take a second Clear itself.
        assert!(
            verdicts.contains(&true) && verdicts.contains(&false),
            "weezl's lzw_fill no longer depends on its bit buffer here ({verdicts:?}): fill could decode Clear, Clear"
        );
    }
}
