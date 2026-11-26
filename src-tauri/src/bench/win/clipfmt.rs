//! Raw clip containers and the reader-side BGRA transforms. Legacy (macOS) takes have no file
//! header and fixed NV12 records; v2 adds a 64-byte header with a magic and fixed records; v3 is
//! the same headers with variable-length, compressed records, a CRC-32 per frame and an index.
//! All three share the 32-byte record header. Little-endian throughout.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The record header, in both containers.
pub const HEADER: usize = 32;
/// The v2 file header.
pub const FILE_HEADER: usize = 64;
pub const MAGIC: [u8; 8] = *b"SERAW2\0\0";
pub const VERSION: u32 = 2;

/// v2 record flags (the record header's last word).
pub const FLAG_REPEATED: u32 = 1 << 0;
/// WGC reported a dirty region list and it was empty.
pub const FLAG_DIRTY_EMPTY: u32 = 1 << 1;
/// The pointer moved on, onto or off the captured monitor since the previous delivered frame.
pub const FLAG_CURSOR_MOVED: u32 = 1 << 2;
/// Dirty regions were readable for this frame (without it `FLAG_DIRTY_EMPTY` means nothing).
pub const FLAG_DIRTY_KNOWN: u32 = 1 << 3;
/// `SystemRelativeTime` was readable and is in `os_ns`.
pub const FLAG_OS_TIME: u32 = 1 << 4;

/// v2 file flags.
pub const FILE_CLOSED: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    Bgra,
    Nv12,
}

impl PixelFormat {
    pub fn tag(self) -> [u8; 4] {
        match self {
            PixelFormat::Bgra => *b"BGRA",
            PixelFormat::Nv12 => *b"NV12",
        }
    }

    fn from_tag(t: &[u8]) -> Option<PixelFormat> {
        match t {
            b"BGRA" => Some(PixelFormat::Bgra),
            b"NV12" => Some(PixelFormat::Nv12),
            _ => None,
        }
    }

    /// Minimum stride and picture bytes for a `w x h` picture with row stride `stride`.
    fn sizes(self, w: usize, h: usize, stride: usize) -> (usize, usize) {
        match self {
            PixelFormat::Bgra => (w * 4, stride * h),
            PixelFormat::Nv12 => (w, stride * h * 3 / 2),
        }
    }
}

/// The v2 file header. Bytes: 0 magic `SERAW2\0\0`, 8 header size (64), 12 version (2),
/// 16 pixel format tag (`BGRA`|`NV12`), 20 width, 24 height, 28 row stride, 32 record size
/// (32 + picture, u64), 40 nominal fps, 44 record header size (32), 48 frame count (u64,
/// written at close), 56 flags (bit 0 closed cleanly), 60 reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub rec_size: u64,
    pub nominal_fps: u32,
    pub frame_count: u64,
    pub flags: u32,
}

impl FileHeader {
    /// A header for tightly packed pictures.
    pub fn new(format: PixelFormat, width: u32, height: u32, nominal_fps: u32) -> FileHeader {
        let stride = format.sizes(width as usize, height as usize, 0).0 as u32;
        FileHeader {
            format,
            width,
            height,
            stride,
            rec_size: (HEADER
                + format
                    .sizes(width as usize, height as usize, stride as usize)
                    .1) as u64,
            nominal_fps,
            frame_count: 0,
            flags: 0,
        }
    }

    pub fn payload(&self) -> usize {
        self.rec_size as usize - HEADER
    }

    pub fn to_bytes(self) -> [u8; FILE_HEADER] {
        let mut b = [0u8; FILE_HEADER];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&(FILE_HEADER as u32).to_le_bytes());
        b[12..16].copy_from_slice(&VERSION.to_le_bytes());
        b[16..20].copy_from_slice(&self.format.tag());
        b[20..24].copy_from_slice(&self.width.to_le_bytes());
        b[24..28].copy_from_slice(&self.height.to_le_bytes());
        b[28..32].copy_from_slice(&self.stride.to_le_bytes());
        b[32..40].copy_from_slice(&self.rec_size.to_le_bytes());
        b[40..44].copy_from_slice(&self.nominal_fps.to_le_bytes());
        b[44..48].copy_from_slice(&(HEADER as u32).to_le_bytes());
        b[48..56].copy_from_slice(&self.frame_count.to_le_bytes());
        b[56..60].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    /// `None`: not a v2 file (no magic), so the caller reads it as the legacy layout.
    /// `Some(Err)`: the magic is there but the header is not one this reader accepts.
    pub fn parse(b: &[u8]) -> Option<Result<FileHeader, String>> {
        if b.len() < MAGIC.len() || b[..MAGIC.len()] != MAGIC {
            return None;
        }
        Some(Self::parse_v2(b))
    }

    fn parse_v2(b: &[u8]) -> Result<FileHeader, String> {
        if b.len() < FILE_HEADER {
            return Err(format!(
                "v2 file header truncated ({} of {FILE_HEADER} bytes)",
                b.len()
            ));
        }
        let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if u32at(8) as usize != FILE_HEADER {
            return Err(format!(
                "v2 file header size {} (want {FILE_HEADER})",
                u32at(8)
            ));
        }
        if u32at(12) != VERSION {
            return Err(format!(
                "container version {} (this reader: {VERSION})",
                u32at(12)
            ));
        }
        let format = PixelFormat::from_tag(&b[16..20]).ok_or_else(|| {
            format!(
                "unknown pixel format tag {:?}",
                String::from_utf8_lossy(&b[16..20])
            )
        })?;
        let (width, height, stride) = (u32at(20), u32at(24), u32at(28));
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return Err(format!("implausible picture size {w}x{h}"));
        }
        if format == PixelFormat::Nv12 && (w % 2 != 0 || h % 2 != 0) {
            return Err(format!("NV12 picture size {w}x{h} is not even"));
        }
        let (min_stride, payload) = format.sizes(w, h, stride as usize);
        if (stride as usize) < min_stride {
            return Err(format!("row stride {stride} is below {min_stride}"));
        }
        if u32at(44) as usize != HEADER {
            return Err(format!("record header size {} (want {HEADER})", u32at(44)));
        }
        let rec_size = u64at(32);
        if rec_size != (HEADER + payload) as u64 {
            return Err(format!(
                "record size {rec_size} does not match {HEADER} + {payload} picture bytes"
            ));
        }
        Ok(FileHeader {
            format,
            width,
            height,
            stride,
            rec_size,
            nominal_fps: u32at(40),
            frame_count: u64at(48),
            flags: u32at(56),
        })
    }

    pub fn layout(&self) -> Layout {
        Layout {
            v2: true,
            format: self.format,
            width: self.width as usize,
            height: self.height as usize,
            stride: self.stride as usize,
            payload: self.payload(),
            rec_size: self.rec_size,
            data_offset: FILE_HEADER as u64,
            nominal_fps: self.nominal_fps,
            declared_frames: (self.flags & FILE_CLOSED != 0).then_some(self.frame_count),
        }
    }
}

/// Where a clip's pictures are and what they are, for either container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub v2: bool,
    pub format: PixelFormat,
    pub width: usize,
    pub height: usize,
    /// Bytes per stored row (NV12: of the luma plane; chroma rows have the same stride).
    pub stride: usize,
    /// Picture bytes per record.
    pub payload: usize,
    pub rec_size: u64,
    /// Offset of the first record.
    pub data_offset: u64,
    /// The cap the take was recorded with (0 = unknown, and always 0 for a legacy clip).
    pub nominal_fps: u32,
    /// The frame count written at a clean close; `None` for a legacy or unclosed take.
    pub declared_frames: Option<u64>,
}

impl Layout {
    pub fn legacy_nv12(width: usize, height: usize) -> Layout {
        Layout {
            v2: false,
            format: PixelFormat::Nv12,
            width,
            height,
            stride: width,
            payload: width * height * 3 / 2,
            rec_size: (HEADER + width * height * 3 / 2) as u64,
            data_offset: 0,
            nominal_fps: 0,
            declared_frames: None,
        }
    }
}

/// One 32-byte record header: 0 `submit_ns` (host clock, u64), 8 `os_ns` (host clock, 0 = none),
/// 16 width, 20 height, 24 `force_idr`, 28 flags (legacy: non-zero = repeated; v2/v3: bit 0
/// repeated, then the `FLAG_*` bits).
pub fn record_header(
    submit_ns: u64,
    os_ns: u64,
    width: u32,
    height: u32,
    force_idr: bool,
    flags: u32,
) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..8].copy_from_slice(&submit_ns.to_le_bytes());
    h[8..16].copy_from_slice(&os_ns.to_le_bytes());
    h[16..20].copy_from_slice(&width.to_le_bytes());
    h[20..24].copy_from_slice(&height.to_le_bytes());
    h[24..28].copy_from_slice(&u32::from(force_idr).to_le_bytes());
    h[28..32].copy_from_slice(&flags.to_le_bytes());
    h
}

/// v3: variable-length records.
pub const VERSION_VAR: u32 = 3;
/// v3 file flag: the index is present at `index_sector * 4096`.
pub const FILE_INDEXED: u32 = 1 << 1;
pub const CODEC_SHIFT: u32 = 8;
pub const CODEC_NONE: u32 = 0;
pub const CODEC_LZ4: u32 = 1;
/// The frame word after each v3 record header.
pub const FRAME_WORD: usize = 16;
/// Record header + frame word.
pub const VAR_RECORD_HEAD: usize = HEADER + FRAME_WORD;
pub const ENC_STORED: u32 = 0;
pub const ENC_LZ4: u32 = 1;
pub const INDEX_MAGIC: [u8; 8] = *b"SEIDX\0\0\0";
pub const INDEX_HEADER: usize = 24;
pub const INDEX_ENTRY: usize = 24;
pub const INDEX_ALIGN: u64 = 4096;

/// The v3 file header: the v2 layout with version 3 (so a v2-only reader refuses it), flags bit 1
/// for "indexed", bits 8..15 the codec, and bytes 60..64 the index position in 4096-byte units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarFileHeader {
    pub base: FileHeader,
    pub codec: u32,
    /// Index position in 4096-byte units; 0 = none.
    pub index_sector: u32,
}

impl VarFileHeader {
    pub fn new(format: PixelFormat, width: u32, height: u32, nominal_fps: u32, codec: u32) -> Self {
        VarFileHeader {
            base: FileHeader::new(format, width, height, nominal_fps),
            codec,
            index_sector: 0,
        }
    }

    /// The flags word as written: the base flags' low byte, the codec, and the index bit.
    pub fn flags(&self) -> u32 {
        let mut f = (self.base.flags & 0xFF & !FILE_INDEXED) | ((self.codec & 0xFF) << CODEC_SHIFT);
        if self.index_sector != 0 {
            f |= FILE_INDEXED;
        }
        f
    }

    pub fn to_bytes(self) -> [u8; FILE_HEADER] {
        let mut b = FileHeader {
            flags: self.flags(),
            ..self.base
        }
        .to_bytes();
        b[12..16].copy_from_slice(&VERSION_VAR.to_le_bytes());
        b[60..64].copy_from_slice(&self.index_sector.to_le_bytes());
        b
    }

    /// `None`: not a v3 file (no magic, or another version). `Some(Err)`: a v3 header this reader
    /// does not accept.
    pub fn parse(b: &[u8]) -> Option<Result<VarFileHeader, String>> {
        if b.len() < 16 || b[..MAGIC.len()] != MAGIC {
            return None;
        }
        if u32::from_le_bytes(b[12..16].try_into().ok()?) != VERSION_VAR {
            return None;
        }
        Some(Self::parse_v3(b))
    }

    fn parse_v3(b: &[u8]) -> Result<VarFileHeader, String> {
        if b.len() < FILE_HEADER {
            return Err(format!(
                "v3 file header truncated ({} of {FILE_HEADER} bytes)",
                b.len()
            ));
        }
        // everything but the version is the v2 layout: validate it with the v2 rules
        let mut v2 = [0u8; FILE_HEADER];
        v2.copy_from_slice(&b[..FILE_HEADER]);
        v2[12..16].copy_from_slice(&VERSION.to_le_bytes());
        let mut base = FileHeader::parse_v2(&v2)?;
        let word = base.flags;
        // the codec and index bits live in `codec` and `index_sector`; `flags()` puts them back
        base.flags &= 0xFF & !FILE_INDEXED;
        let codec = (word >> CODEC_SHIFT) & 0xFF;
        if codec != CODEC_NONE && codec != CODEC_LZ4 {
            return Err(format!("unknown codec {codec}"));
        }
        let index_sector = u32::from_le_bytes(b[60..64].try_into().unwrap());
        Ok(VarFileHeader {
            base,
            codec,
            index_sector: if word & FILE_INDEXED != 0 {
                index_sector
            } else {
                0
            },
        })
    }
}

/// The 16-byte word after each v3 record header: `stored_len`, encoding (0 raw, 1 LZ4 block),
/// CRC-32 of the uncompressed picture, `raw_len`. Every frame decodes on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameWord {
    pub stored_len: u32,
    pub encoding: u32,
    pub crc32: u32,
    pub raw_len: u32,
}

impl FrameWord {
    pub fn to_bytes(self) -> [u8; FRAME_WORD] {
        let mut b = [0u8; FRAME_WORD];
        b[0..4].copy_from_slice(&self.stored_len.to_le_bytes());
        b[4..8].copy_from_slice(&self.encoding.to_le_bytes());
        b[8..12].copy_from_slice(&self.crc32.to_le_bytes());
        b[12..16].copy_from_slice(&self.raw_len.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> FrameWord {
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        FrameWord {
            stored_len: u(0),
            encoding: u(4),
            crc32: u(8),
            raw_len: u(12),
        }
    }

    /// The word is one a writer of `payload`-byte pictures could have produced.
    pub fn check(&self, frame: usize, payload: usize) -> Result<(), String> {
        if self.raw_len as usize != payload {
            return Err(format!(
                "frame {frame}: raw length {} is not the clip's {payload}",
                self.raw_len
            ));
        }
        match self.encoding {
            ENC_STORED if self.stored_len as usize == payload => Ok(()),
            ENC_STORED => Err(format!(
                "frame {frame}: stored raw but {} bytes, not {payload}",
                self.stored_len
            )),
            ENC_LZ4 if self.stored_len > 0 && (self.stored_len as usize) < payload => Ok(()),
            ENC_LZ4 => Err(format!(
                "frame {frame}: LZ4 length {} out of range (1..{payload})",
                self.stored_len
            )),
            e => Err(format!("frame {frame}: unknown encoding {e}")),
        }
    }
}

/// Compress one picture into the reused `out`. Returns the word and whether the bytes to store
/// are in `out`; a picture LZ4 cannot shrink is stored raw, never expanded.
pub fn compress_frame(raw: &[u8], out: &mut Vec<u8>) -> (FrameWord, bool) {
    let crc32 = crc32fast::hash(raw);
    let need = lz4_flex::block::get_maximum_output_size(raw.len());
    if out.len() < need {
        out.resize(need, 0);
    }
    match lz4_flex::block::compress_into(raw, &mut out[..]) {
        Ok(n) if n > 0 && n < raw.len() => (
            FrameWord {
                stored_len: n as u32,
                encoding: ENC_LZ4,
                crc32,
                raw_len: raw.len() as u32,
            },
            true,
        ),
        _ => (
            FrameWord {
                stored_len: raw.len() as u32,
                encoding: ENC_STORED,
                crc32,
                raw_len: raw.len() as u32,
            },
            false,
        ),
    }
}

/// Decode one stored picture into `buf` and check its CRC. Every error names the frame.
pub fn decode_frame(
    frame: usize,
    word: &FrameWord,
    stored: &[u8],
    payload: usize,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    buf.resize(payload, 0);
    decode_frame_into(frame, word, stored, payload, &mut buf[..])
}

/// [`decode_frame`] into a caller's buffer of exactly `payload` bytes.
pub fn decode_frame_into(
    frame: usize,
    word: &FrameWord,
    stored: &[u8],
    payload: usize,
    buf: &mut [u8],
) -> Result<(), String> {
    word.check(frame, payload)?;
    if stored.len() != word.stored_len as usize {
        return Err(format!(
            "frame {frame}: {} stored bytes, the word says {}",
            stored.len(),
            word.stored_len
        ));
    }
    if buf.len() != payload {
        return Err(format!(
            "frame {frame}: a {} byte buffer for a {payload} byte picture",
            buf.len()
        ));
    }
    match word.encoding {
        ENC_STORED => buf.copy_from_slice(stored),
        _ => match lz4_flex::block::decompress_into(stored, buf) {
            Ok(n) if n == payload => {}
            Ok(n) => {
                return Err(format!(
                    "frame {frame}: LZ4 decoded {n} bytes, expected {payload}"
                ))
            }
            Err(e) => return Err(format!("frame {frame}: LZ4: {e}")),
        },
    }
    let crc = crc32fast::hash(buf);
    if crc != word.crc32 {
        return Err(format!(
            "frame {frame}: CRC-32 {crc:08x} does not match the stored {:08x}",
            word.crc32
        ));
    }
    Ok(())
}

/// Where one v3 record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLoc {
    /// Offset of the record header.
    pub offset: u64,
    /// Record header + frame word + stored bytes.
    pub len: u64,
    pub submit_ns: u64,
}

/// The records of a v3 file, and how they were found.
#[derive(Clone, Debug, Default)]
pub struct FrameTable {
    pub records: Vec<RecordLoc>,
    /// Read from the index, each entry checked against its record; otherwise walked.
    pub from_index: bool,
    /// Why the index was not used, when there is one.
    pub index_note: Option<String>,
    pub codec: u32,
    /// Frames stored raw / as LZ4.
    pub stored_raw: usize,
    pub stored_lz4: usize,
    /// Sum of `stored_len` (picture bytes on disk).
    pub stored_bytes: u64,
}

/// Builds the index while records are appended, and its bytes at close.
#[derive(Clone, Debug)]
pub struct IndexBuilder {
    pub records: Vec<RecordLoc>,
    /// Logical offset of the next record.
    pub pos: u64,
}

impl IndexBuilder {
    pub fn new(capacity: usize) -> IndexBuilder {
        IndexBuilder {
            records: Vec::with_capacity(capacity),
            pos: FILE_HEADER as u64,
        }
    }

    /// Note one record of `stored_len` picture bytes appended at `self.pos`.
    pub fn add(&mut self, submit_ns: u64, stored_len: u32) -> RecordLoc {
        let loc = RecordLoc {
            offset: self.pos,
            len: (VAR_RECORD_HEAD + stored_len as usize) as u64,
            submit_ns,
        };
        self.pos += loc.len;
        self.records.push(loc);
        loc
    }

    /// The bytes to append after the last record and the index position in 4096-byte units.
    /// Layout: zero padding to a 4096 boundary, `SEIDX\0\0\0`, count (u64), entry size (24),
    /// CRC-32 of the entries, then per frame offset, length and `submit_ns` (u64 each).
    pub fn tail(&self, out: &mut Vec<u8>) -> Result<u32, String> {
        out.clear();
        let at = self.pos.div_ceil(INDEX_ALIGN) * INDEX_ALIGN;
        out.resize((at - self.pos) as usize, 0);
        let mut entries = Vec::with_capacity(self.records.len() * INDEX_ENTRY);
        for r in &self.records {
            entries.extend_from_slice(&r.offset.to_le_bytes());
            entries.extend_from_slice(&r.len.to_le_bytes());
            entries.extend_from_slice(&r.submit_ns.to_le_bytes());
        }
        out.extend_from_slice(&INDEX_MAGIC);
        out.extend_from_slice(&(self.records.len() as u64).to_le_bytes());
        out.extend_from_slice(&(INDEX_ENTRY as u32).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&entries).to_le_bytes());
        out.extend_from_slice(&entries);
        u32::try_from(at / INDEX_ALIGN).map_err(|_| "file too large for the index position".into())
    }
}

/// Read a v3 index. `Ok(None)` when the file is cut inside it; `Err` when it is there and wrong.
fn read_index<R: Read + Seek>(
    f: &mut R,
    len: u64,
    at: u64,
) -> Result<Option<Vec<RecordLoc>>, String> {
    if at + INDEX_HEADER as u64 > len {
        return Ok(None);
    }
    let mut h = [0u8; INDEX_HEADER];
    f.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
    f.read_exact(&mut h)
        .map_err(|e| format!("index header: {e}"))?;
    if h[0..8] != INDEX_MAGIC {
        return Err(format!("no index magic at offset {at}"));
    }
    let count = u64::from_le_bytes(h[8..16].try_into().unwrap());
    let entry = u32::from_le_bytes(h[16..20].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(h[20..24].try_into().unwrap());
    if entry != INDEX_ENTRY {
        return Err(format!("index entry size {entry} (want {INDEX_ENTRY})"));
    }
    let bytes = count
        .checked_mul(INDEX_ENTRY as u64)
        .ok_or("index count overflows")?;
    if at + INDEX_HEADER as u64 + bytes > len {
        return Ok(None);
    }
    let mut e = vec![0u8; bytes as usize];
    f.read_exact(&mut e).map_err(|e| format!("index: {e}"))?;
    if crc32fast::hash(&e) != crc {
        return Err("index CRC-32 mismatch: the index is corrupt".into());
    }
    let u = |o: usize| u64::from_le_bytes(e[o..o + 8].try_into().unwrap());
    Ok(Some(
        (0..count as usize)
            .map(|i| RecordLoc {
                offset: u(i * INDEX_ENTRY),
                len: u(i * INDEX_ENTRY + 8),
                submit_ns: u(i * INDEX_ENTRY + 16),
            })
            .collect(),
    ))
}

/// Read and check the record head (record header + frame word) at `pos`.
fn read_head<R: Read + Seek>(
    f: &mut R,
    frame: usize,
    pos: u64,
    layout: &Layout,
) -> Result<([u8; HEADER], FrameWord), String> {
    let mut b = [0u8; VAR_RECORD_HEAD];
    f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
    f.read_exact(&mut b)
        .map_err(|e| format!("frame {frame}: record head at {pos}: {e}"))?;
    let mut h = [0u8; HEADER];
    h.copy_from_slice(&b[..HEADER]);
    let w = u32::from_le_bytes(h[16..20].try_into().unwrap()) as usize;
    let ht = u32::from_le_bytes(h[20..24].try_into().unwrap()) as usize;
    if (w, ht) != (layout.width, layout.height) {
        return Err(format!(
            "frame {frame}: record at {pos} is {w}x{ht}, not {}x{}",
            layout.width, layout.height
        ));
    }
    let word = FrameWord::from_bytes(&b[HEADER..]);
    word.check(frame, layout.payload)?;
    Ok((h, word))
}

/// What `scan_records` found in a v3 file.
pub struct VarScan {
    pub header: VarFileHeader,
    pub layout: Layout,
    pub table: FrameTable,
    /// Each record's 32-byte header, in order.
    pub heads: Vec<[u8; HEADER]>,
}

/// Find the records of a v3 file: from its index when `use_index` and the index is whole,
/// otherwise by walking them and keeping every complete one.
pub fn scan_records(path: &Path, use_index: bool) -> Result<VarScan, String> {
    let mut f = std::io::BufReader::with_capacity(
        64 << 10,
        File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?,
    );
    let len = f.get_ref().metadata().map_err(|e| e.to_string())?.len();
    let mut first = [0u8; FILE_HEADER];
    let got = f
        .read(&mut first)
        .map_err(|e| format!("first header: {e}"))?;
    let vh = match VarFileHeader::parse(&first[..got]) {
        Some(r) => r?,
        None => return Err("not a v3 (variable-length) clip".into()),
    };
    let layout = vh.base.layout();
    let index_at = vh.index_sector as u64 * INDEX_ALIGN;
    // records end where the index begins, when the file reaches that far
    let data_end = if index_at != 0 && index_at <= len {
        index_at
    } else {
        len
    };
    let mut table = FrameTable {
        codec: vh.codec,
        ..Default::default()
    };
    let mut heads = Vec::new();
    let tally = |table: &mut FrameTable, word: &FrameWord| {
        if word.encoding == ENC_LZ4 {
            table.stored_lz4 += 1;
        } else {
            table.stored_raw += 1;
        }
        table.stored_bytes += word.stored_len as u64;
    };
    let mut indexed = None;
    if index_at == 0 {
        table.index_note = Some("no index (the take was not closed)".into());
    } else if use_index {
        match read_index(&mut f, len, index_at)? {
            Some(e) => indexed = Some(e),
            None => table.index_note = Some("index truncated; records walked instead".into()),
        }
    } else {
        table.index_note = Some("index not used (sequential scan asked for)".into());
    }
    if let Some(entries) = indexed {
        let mut expect = FILE_HEADER as u64;
        for (i, loc) in entries.iter().enumerate() {
            if loc.offset != expect || loc.offset + loc.len > data_end {
                return Err(format!(
                    "frame {i}: index entry at {} (+{}) does not follow the previous record \
                     (expected {expect}) or runs past the data",
                    loc.offset, loc.len
                ));
            }
            let (h, word) = read_head(&mut f, i, loc.offset, &layout)?;
            let t = u64::from_le_bytes(h[0..8].try_into().unwrap());
            let rec = VAR_RECORD_HEAD as u64 + word.stored_len as u64;
            if loc.len != rec || loc.submit_ns != t {
                return Err(format!(
                    "frame {i}: the index says {} bytes at {} ns, the record says {rec} at {t}",
                    loc.len, loc.submit_ns
                ));
            }
            tally(&mut table, &word);
            heads.push(h);
            expect += loc.len;
        }
        table.records = entries;
        table.from_index = true;
        if vh.base.flags & FILE_CLOSED != 0 && vh.base.frame_count != table.records.len() as u64 {
            return Err(format!(
                "the header declares {} frames, the index holds {}",
                vh.base.frame_count,
                table.records.len()
            ));
        }
    } else {
        let mut pos = FILE_HEADER as u64;
        let mut i = 0usize;
        while pos + VAR_RECORD_HEAD as u64 <= data_end {
            // Zeros are the padding before the index or the unused reserve of an unclosed take.
            let mut peek = [0u8; VAR_RECORD_HEAD];
            f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            f.read_exact(&mut peek)
                .map_err(|e| format!("frame {i}: record head at {pos}: {e}"))?;
            if peek.iter().all(|&b| b == 0) {
                break;
            }
            let (h, word) = read_head(&mut f, i, pos, &layout)?;
            let rec = VAR_RECORD_HEAD as u64 + word.stored_len as u64;
            if pos + rec > data_end {
                break; // cut inside this frame: keep the complete ones
            }
            tally(&mut table, &word);
            table.records.push(RecordLoc {
                offset: pos,
                len: rec,
                submit_ns: u64::from_le_bytes(h[0..8].try_into().unwrap()),
            });
            heads.push(h);
            pos += rec;
            i += 1;
        }
    }
    if table.records.is_empty() {
        return Err("clip has no complete records".into());
    }
    Ok(VarScan {
        header: vh,
        layout,
        table,
        heads,
    })
}

/// `clip::scan` for a v3 file.
pub fn scan_variable(path: &Path, use_index: bool) -> Result<super::super::clip::ClipInfo, String> {
    let s = scan_records(path, use_index)?;
    let frames = s.heads.len();
    let mut info = super::super::clip::ClipInfo {
        width: s.layout.width,
        height: s.layout.height,
        frames,
        rec_size: s.layout.rec_size,
        times: Vec::with_capacity(frames),
        idr: Vec::with_capacity(frames),
        repeated: Vec::with_capacity(frames),
        layout: s.layout,
        os_ns: Vec::with_capacity(frames),
        flags: Vec::with_capacity(frames),
        table: None,
    };
    let (mut t0, mut last) = (0u64, 0u64);
    for (i, h) in s.heads.iter().enumerate() {
        let t = u64::from_le_bytes(h[0..8].try_into().unwrap());
        if i == 0 {
            t0 = t;
            last = t;
        }
        let t = t.max(last);
        last = t;
        let word = u32::from_le_bytes(h[28..32].try_into().unwrap());
        info.times.push(t - t0);
        info.idr
            .push(u32::from_le_bytes(h[24..28].try_into().unwrap()) != 0);
        info.repeated.push(word & FLAG_REPEATED != 0);
        info.os_ns
            .push(u64::from_le_bytes(h[8..16].try_into().unwrap()));
        info.flags.push(word);
    }
    info.table = Some(std::sync::Arc::new(s.table));
    Ok(info)
}

/// `true` when these first bytes are a v3 file header (so `clip::scan` hands the file here).
pub fn is_variable(first: &[u8]) -> bool {
    VarFileHeader::parse(first).is_some()
}

/// `ClipReader`'s position in a v3 file.
pub struct VarCursor {
    table: std::sync::Arc<FrameTable>,
    next: usize,
    /// Where the underlying reader is.
    pos: u64,
    stored: Vec<u8>,
    payload: usize,
}

impl VarCursor {
    pub fn new(table: std::sync::Arc<FrameTable>, payload: usize, pos: u64) -> VarCursor {
        VarCursor {
            table,
            next: 0,
            pos,
            stored: Vec::new(),
            payload,
        }
    }

    pub fn seek(&mut self, frame: usize) {
        self.next = frame;
        // Force a seek on the next read.
        self.pos = u64::MAX;
    }

    /// Read, decode and check the next frame into `buf`.
    pub fn read_next<R: Read + Seek>(
        &mut self,
        f: &mut R,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        let i = self.next;
        let mut stored = std::mem::take(&mut self.stored);
        let word = self.read_stored(f, &mut stored);
        let r = word.and_then(|w| {
            decode_frame(i, &w, &stored, self.payload, buf)
                .map_err(|m| std::io::Error::new(std::io::ErrorKind::InvalidData, m))
        });
        self.stored = stored;
        r
    }

    /// Read the next frame as stored (its word and its possibly compressed bytes), without
    /// decoding it, so the decode can run on another thread.
    pub fn read_stored<R: Read + Seek>(
        &mut self,
        f: &mut R,
        stored: &mut Vec<u8>,
    ) -> std::io::Result<FrameWord> {
        use std::io::{Error, ErrorKind};
        let i = self.next;
        let Some(loc) = self.table.records.get(i).copied() else {
            return Err(Error::new(ErrorKind::UnexpectedEof, "no more frames"));
        };
        if self.pos != loc.offset {
            f.seek(SeekFrom::Start(loc.offset))?;
        }
        let bad = |m: String| Error::new(ErrorKind::InvalidData, m);
        let mut head = [0u8; VAR_RECORD_HEAD];
        f.read_exact(&mut head)?;
        let word = FrameWord::from_bytes(&head[HEADER..]);
        word.check(i, self.payload).map_err(bad)?;
        if loc.len != VAR_RECORD_HEAD as u64 + word.stored_len as u64 {
            return Err(bad(format!(
                "frame {i}: record length {} does not match its word ({})",
                loc.len, word.stored_len
            )));
        }
        stored.resize(word.stored_len as usize, 0);
        f.read_exact(stored)?;
        self.pos = loc.offset + loc.len;
        self.next = i + 1;
        Ok(word)
    }

    /// The index of the frame the next read returns.
    pub fn position(&self) -> usize {
        self.next
    }
}

/// Puts items that finish in any order back into sequence order. A sequence number outside the
/// `capacity` window is refused rather than misplaced.
pub struct Reorder<T> {
    slots: Vec<Option<T>>,
    next: u64,
}

impl<T> Reorder<T> {
    pub fn new(capacity: usize) -> Reorder<T> {
        Reorder {
            slots: (0..capacity.max(1)).map(|_| None).collect(),
            next: 0,
        }
    }

    pub fn insert(&mut self, seq: u64, item: T) -> Result<(), T> {
        let cap = self.slots.len() as u64;
        if seq < self.next || seq - self.next >= cap {
            return Err(item);
        }
        let slot = &mut self.slots[(seq % cap) as usize];
        if slot.is_some() {
            return Err(item);
        }
        *slot = Some(item);
        Ok(())
    }

    /// The next item in sequence, when it has arrived.
    pub fn pop(&mut self) -> Option<T> {
        let cap = self.slots.len() as u64;
        let item = self.slots[(self.next % cap) as usize].take()?;
        self.next += 1;
        Some(item)
    }

    pub fn next_seq(&self) -> u64 {
        self.next
    }
}

/// What happens to each BGRA picture on its way out of the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgraOp {
    None,
    /// Four copies side by side: 2w x 2h (1080p -> 4K).
    Tile2x2,
    /// A centred crop, even origin.
    Crop(usize, usize),
    /// 2x2 box filter: w/2 x h/2 (rounded to nearest).
    Down2x,
}

/// A workload's `transform` for a BGRA clip: an optional rate (`fps:N`, newest frame at each
/// deadline) and at most one picture operation, comma-separated: `fps:60`, `tile2x2`,
/// `crop:1280x720`, `down2x`, `fps:60,down2x`, `none`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReplayTransform {
    pub fps: Option<f64>,
    pub op: BgraOp,
}

impl ReplayTransform {
    pub fn parse(s: Option<&str>) -> Result<ReplayTransform, String> {
        let mut t = ReplayTransform {
            fps: None,
            op: BgraOp::None,
        };
        for part in s.unwrap_or("").split(',').map(str::trim) {
            if part.is_empty() || part == "none" {
                continue;
            }
            if let Some(f) = part.strip_prefix("fps:") {
                let f: f64 = f.parse().map_err(|_| format!("bad rate '{part}'"))?;
                if !(f.is_finite() && f > 0.0) {
                    return Err(format!("bad rate '{part}'"));
                }
                t.fps = Some(f);
                continue;
            }
            let op = if part == "tile2x2" {
                BgraOp::Tile2x2
            } else if part == "down2x" {
                BgraOp::Down2x
            } else if let Some(dims) = part.strip_prefix("crop:") {
                let (w, h) = dims
                    .split_once('x')
                    .ok_or_else(|| format!("bad crop '{part}'"))?;
                let w: usize = w.parse().map_err(|_| format!("bad crop '{part}'"))?;
                let h: usize = h.parse().map_err(|_| format!("bad crop '{part}'"))?;
                BgraOp::Crop(w & !1, h & !1)
            } else {
                return Err(format!("unknown transform '{part}'"));
            };
            if t.op != BgraOp::None {
                return Err(format!("more than one picture transform in '{part}'"));
            }
            t.op = op;
        }
        Ok(t)
    }

    pub fn out_dims(&self, w: usize, h: usize) -> Result<(usize, usize), String> {
        match self.op {
            BgraOp::None => Ok((w, h)),
            BgraOp::Tile2x2 => Ok((w * 2, h * 2)),
            BgraOp::Crop(cw, ch) if cw >= 16 && ch >= 16 && cw <= w && ch <= h => Ok((cw, ch)),
            BgraOp::Crop(cw, ch) => Err(format!("crop {cw}x{ch} does not fit {w}x{h}")),
            BgraOp::Down2x if w >= 2 && h >= 2 => Ok((w / 2, h / 2)),
            BgraOp::Down2x => Err(format!("{w}x{h} is too small to halve")),
        }
    }

    /// Apply the picture operation to one BGRA picture (`h` rows of `stride` bytes, `w*4` of them
    /// pixels) into `out`, tightly packed. Returns the output size.
    pub fn apply(
        &self,
        src: &[u8],
        w: usize,
        h: usize,
        stride: usize,
        out: &mut Vec<u8>,
    ) -> Result<(usize, usize), String> {
        let (ow, oh) = self.out_dims(w, h)?;
        if stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
            return Err(format!(
                "picture of {} bytes is short for {w}x{h} at stride {stride}",
                src.len()
            ));
        }
        out.resize(ow * oh * 4, 0);
        let row = w * 4;
        match self.op {
            BgraOp::None => {
                for y in 0..h {
                    out[y * row..(y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
                }
            }
            BgraOp::Tile2x2 => {
                let orow = ow * 4;
                for y in 0..oh {
                    let s = &src[(y % h) * stride..(y % h) * stride + row];
                    out[y * orow..y * orow + row].copy_from_slice(s);
                    out[y * orow + row..(y + 1) * orow].copy_from_slice(s);
                }
            }
            BgraOp::Crop(..) => {
                let ox = ((w - ow) / 2) & !1;
                let oy = ((h - oh) / 2) & !1;
                let orow = ow * 4;
                for y in 0..oh {
                    let s = (y + oy) * stride + ox * 4;
                    out[y * orow..(y + 1) * orow].copy_from_slice(&src[s..s + orow]);
                }
            }
            BgraOp::Down2x => {
                let orow = ow * 4;
                for y in 0..oh {
                    let r0 = &src[2 * y * stride..];
                    let r1 = &src[(2 * y + 1) * stride..];
                    for x in 0..ow {
                        for c in 0..4 {
                            let a = r0[8 * x + c] as u32
                                + r0[8 * x + 4 + c] as u32
                                + r1[8 * x + c] as u32
                                + r1[8 * x + 4 + c] as u32;
                            out[y * orow + 4 * x + c] = ((a + 2) / 4) as u8;
                        }
                    }
                }
            }
        }
        Ok((ow, oh))
    }
}

/// A lower frame rate: at each deadline `k / fps` the newest frame at or before it, as the live
/// cadence gate picks; a deadline with nothing newer yields nothing. Returns `(record index,
/// deadline ns)` for the clip's relative times.
pub fn decimate(times: &[u64], fps: f64) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    if times.is_empty() || !(fps.is_finite() && fps > 0.0) {
        return out;
    }
    let period = 1e9 / fps;
    let last_t = *times.last().unwrap_or(&0);
    let mut next = 0usize; // first index not yet known to be <= the deadline
    let mut picked: Option<usize> = None;
    let mut k = 0u64;
    loop {
        let deadline = (k as f64 * period).round() as u64;
        while next < times.len() && times[next] <= deadline {
            next += 1;
        }
        if next > 0 {
            let newest = next - 1;
            if picked.is_none_or(|p| newest > p) {
                out.push((newest, deadline));
                picked = Some(newest);
            }
        }
        if deadline >= last_t {
            break;
        }
        k += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::clip::{scan, ClipReader};
    use std::io::Write;

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "se-bench-clipfmt-{}-{}-{name}",
            std::process::id(),
            crate::streamer::audio::host_now_ns()
        ));
        p
    }

    /// A 4x2 BGRA picture whose bytes say which frame and pixel they are.
    fn bgra_frame(i: u8, w: usize, h: usize) -> Vec<u8> {
        (0..w * h * 4)
            .map(|b| i.wrapping_mul(16) ^ b as u8)
            .collect()
    }

    fn write_v2(path: &std::path::Path, hdr: FileHeader, recs: &[([u8; HEADER], Vec<u8>)]) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&hdr.to_bytes()).unwrap();
        for (h, p) in recs {
            f.write_all(h).unwrap();
            f.write_all(p).unwrap();
        }
    }

    #[test]
    fn the_file_header_round_trips_and_has_the_documented_layout() {
        let mut h = FileHeader::new(PixelFormat::Bgra, 1920, 1080, 120);
        h.frame_count = 360;
        h.flags = FILE_CLOSED;
        let b = h.to_bytes();
        assert_eq!(&b[0..8], b"SERAW2\0\0");
        assert_eq!(&b[16..20], b"BGRA");
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 1920 * 4);
        assert_eq!(
            u64::from_le_bytes(b[32..40].try_into().unwrap()),
            32 + 1920 * 1080 * 4
        );
        assert_eq!(FileHeader::parse(&b).unwrap().unwrap(), h);
        let n = FileHeader::new(PixelFormat::Nv12, 64, 32, 60);
        assert_eq!(n.payload(), 64 * 32 * 3 / 2);
        assert_eq!(FileHeader::parse(&n.to_bytes()).unwrap().unwrap(), n);
        // No magic: not v2, the caller reads the legacy layout.
        assert!(FileHeader::parse(&[0u8; 64]).is_none());
        // Magic and nonsense: refused, not misread.
        let mut bad = b;
        bad[16..20].copy_from_slice(b"YUY2");
        assert!(FileHeader::parse(&bad).unwrap().is_err());
        let mut bad = b;
        bad[32] ^= 1;
        assert!(FileHeader::parse(&bad).unwrap().is_err());
        assert!(FileHeader::parse(&b[..40]).unwrap().is_err());
    }

    #[test]
    fn a_v2_take_round_trips_through_scan_and_the_reader() {
        let (w, h) = (4usize, 2usize);
        let mut fh = FileHeader::new(PixelFormat::Bgra, w as u32, h as u32, 120);
        fh.frame_count = 3;
        fh.flags = FILE_CLOSED;
        let flags = [
            FLAG_DIRTY_KNOWN | FLAG_OS_TIME,
            FLAG_DIRTY_KNOWN | FLAG_DIRTY_EMPTY | FLAG_CURSOR_MOVED,
            FLAG_REPEATED,
        ];
        let recs: Vec<_> = (0..3u8)
            .map(|i| {
                let t = 5_000_000_000 + i as u64 * 8_333_333;
                (
                    record_header(
                        t,
                        t + 9_000_000,
                        w as u32,
                        h as u32,
                        i == 0,
                        flags[i as usize],
                    ),
                    bgra_frame(i, w, h),
                )
            })
            .collect();
        let p = temp("v2.seraw");
        write_v2(&p, fh, &recs);
        let info = scan(&p).unwrap();
        assert!(info.layout.v2);
        assert_eq!(info.layout.format, PixelFormat::Bgra);
        assert_eq!(info.layout.data_offset, 64);
        assert_eq!(info.layout.declared_frames, Some(3));
        assert_eq!(info.layout.nominal_fps, 120);
        assert_eq!((info.width, info.height, info.frames), (4, 2, 3));
        assert_eq!(info.rec_size, 32 + 32);
        assert_eq!(info.times, [0, 8_333_333, 16_666_666]);
        assert_eq!(info.idr, [true, false, false]);
        assert_eq!(info.repeated, [false, false, true]);
        assert_eq!(info.flags, flags);
        assert_eq!(info.os_ns[1], 5_000_000_000 + 8_333_333 + 9_000_000);
        let mut r = ClipReader::open(&p, &info).unwrap();
        let mut buf = Vec::new();
        for i in 0..3u8 {
            r.next(&mut buf).unwrap();
            assert_eq!(buf, bgra_frame(i, w, h), "frame {i}");
        }
        assert!(r.next(&mut buf).is_err(), "no fourth record");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_file_without_the_magic_is_read_as_the_legacy_nv12_layout() {
        // Exactly what `macos_utils::streamer::raw_capture` writes: no file header, NV12.
        let (w, h) = (4usize, 2usize);
        let p = temp("legacy.raw");
        let mut f = std::fs::File::create(&p).unwrap();
        let mut pics = Vec::new();
        for i in 0..3u64 {
            let mut hd = [0u8; HEADER];
            hd[0..8].copy_from_slice(&(1000 + i * 16_666_667).to_le_bytes());
            hd[8..16].copy_from_slice(&(999 + i).to_le_bytes());
            hd[16..20].copy_from_slice(&(w as u32).to_le_bytes());
            hd[20..24].copy_from_slice(&(h as u32).to_le_bytes());
            hd[24..28].copy_from_slice(&u32::from(i == 0).to_le_bytes());
            // A legacy reader treats any non-zero word here as `repeated`.
            hd[28..32].copy_from_slice(&(if i == 2 { 6u32 } else { 0 }).to_le_bytes());
            let pic: Vec<u8> = (0..w * h * 3 / 2)
                .map(|b| (b as u8) ^ (i as u8 * 7))
                .collect();
            f.write_all(&hd).unwrap();
            f.write_all(&pic).unwrap();
            pics.push(pic);
        }
        drop(f);
        let info = scan(&p).unwrap();
        assert!(!info.layout.v2);
        assert_eq!(info.layout, Layout::legacy_nv12(4, 2));
        assert_eq!(info.frames, 3);
        assert_eq!(info.rec_size, (32 + 12) as u64);
        assert_eq!(info.times, [0, 16_666_667, 33_333_334]);
        assert_eq!(info.idr, [true, false, false]);
        assert_eq!(info.repeated, [false, false, true]);
        let mut r = ClipReader::open(&p, &info).unwrap();
        let mut buf = Vec::new();
        for pic in &pics {
            r.next(&mut buf).unwrap();
            assert_eq!(&buf, pic);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_truncated_file_yields_its_complete_records_and_says_how_many_were_declared() {
        let (w, h) = (4usize, 2usize);
        let mut fh = FileHeader::new(PixelFormat::Bgra, w as u32, h as u32, 60);
        fh.frame_count = 3;
        fh.flags = FILE_CLOSED;
        let recs: Vec<_> = (0..3u8)
            .map(|i| {
                (
                    record_header(i as u64 * 1000, 0, w as u32, h as u32, false, 0),
                    bgra_frame(i, w, h),
                )
            })
            .collect();
        let p = temp("trunc.seraw");
        write_v2(&p, fh, &recs);
        let full = std::fs::metadata(&p).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(full - 10)
            .unwrap();
        let info = scan(&p).unwrap();
        assert_eq!(info.frames, 2);
        assert_eq!(info.layout.declared_frames, Some(3));
        // Cut inside the file header: refused as a v2 file, not misread as legacy.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(40)
            .unwrap();
        assert!(scan(&p).is_err());
        // Header only: no complete records.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert!(scan(&p).is_err());
        let _ = std::fs::remove_file(&p);
        let p2 = temp("trunc2.seraw");
        write_v2(&p2, fh, &[]);
        assert!(scan(&p2).unwrap_err().contains("no complete records"));
        let _ = std::fs::remove_file(&p2);
    }

    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        buf[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    }

    #[test]
    fn tile_crop_and_downscale_have_the_promised_shapes_and_pixels() {
        let parse = |s: &str| ReplayTransform::parse(Some(s)).unwrap();
        assert_eq!(parse("tile2x2").out_dims(1920, 1080).unwrap(), (3840, 2160));
        assert_eq!(parse("down2x").out_dims(1920, 1080).unwrap(), (960, 540));
        assert_eq!(
            parse("crop:1281x721").out_dims(1920, 1080).unwrap(),
            (1280, 720)
        );
        assert!(parse("crop:4000x10").out_dims(1920, 1080).is_err());
        assert_eq!(
            parse("fps:60,down2x"),
            ReplayTransform {
                fps: Some(60.0),
                op: BgraOp::Down2x
            }
        );
        assert_eq!(parse("none").op, BgraOp::None);
        assert!(ReplayTransform::parse(Some("tile2x2,down2x")).is_err());
        assert!(ReplayTransform::parse(Some("fps:0")).is_err());
        assert!(ReplayTransform::parse(Some("blur")).is_err());

        // A 2x2 picture with padding in the stride: each pixel's bytes are (x, y, 10x+y, 255).
        let (w, h, stride) = (2usize, 2usize, 12usize);
        let mut src = vec![0xEEu8; stride * h];
        for y in 0..h {
            for x in 0..w {
                src[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[
                    x as u8,
                    y as u8,
                    (10 * x + y) as u8,
                    255,
                ]);
            }
        }
        let mut out = Vec::new();
        assert_eq!(
            parse("none").apply(&src, w, h, stride, &mut out).unwrap(),
            (2, 2)
        );
        assert_eq!(out.len(), 16, "padding stripped");
        assert_eq!(px(&out, 2, 1, 1), [1, 1, 11, 255]);
        assert_eq!(
            parse("tile2x2")
                .apply(&src, w, h, stride, &mut out)
                .unwrap(),
            (4, 4)
        );
        for (x, y) in [(0, 0), (1, 0), (2, 1), (3, 3), (0, 2)] {
            assert_eq!(
                px(&out, 4, x, y),
                [
                    (x % 2) as u8,
                    (y % 2) as u8,
                    (10 * (x % 2) + y % 2) as u8,
                    255
                ]
            );
        }
        assert_eq!(
            parse("down2x").apply(&src, w, h, stride, &mut out).unwrap(),
            (1, 1)
        );
        // Means of (0,1,0,1), (0,0,1,1), (0,10,1,11), (255 x4), rounded to nearest.
        assert_eq!(out, [1, 1, 6, 255]);

        // A centred crop with an even origin: 20x20 out of 36x24 starts at (8, 2).
        let (w, h) = (36usize, 24usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                src[(y * w + x) * 4..(y * w + x) * 4 + 4]
                    .copy_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        let c = parse("crop:20x20");
        assert_eq!(c.apply(&src, w, h, w * 4, &mut out).unwrap(), (20, 20));
        assert_eq!(px(&out, 20, 0, 0), [8, 2, 0, 255]);
        assert_eq!(px(&out, 20, 19, 19), [27, 21, 0, 255]);
        assert!(parse("down2x")
            .apply(&src[..100], w, h, w * 4, &mut out)
            .is_err());
    }

    #[test]
    fn decimation_picks_the_newest_frame_at_each_deadline() {
        // 120 fps exactly -> 60 fps: every other frame, starting with the first.
        let t120: Vec<u64> = (0..12)
            .map(|i| (i as f64 * 1e9 / 120.0).round() as u64)
            .collect();
        let d: Vec<usize> = decimate(&t120, 60.0).iter().map(|p| p.0).collect();
        // The last frame (91.7 ms) surfaces at the next deadline (100 ms).
        assert_eq!(d, [0, 2, 4, 6, 8, 10, 11]);
        // A frame that lands just after a deadline is not taken early; the one before it is.
        let jitter = [0, 8_000_000, 17_000_000, 25_000_000, 34_000_000];
        let picks = decimate(&jitter, 60.0);
        assert_eq!(
            picks,
            [(0, 0), (1, 16_666_667), (3, 33_333_333), (4, 50_000_000)]
        );
        // Nothing new at a deadline: nothing delivered for it.
        let gap = [0, 1_000_000, 100_000_000];
        let d: Vec<usize> = decimate(&gap, 60.0).iter().map(|p| p.0).collect();
        assert_eq!(d, [0, 1, 2]);
        // A faster target than the clip: every frame once, never twice.
        let d: Vec<usize> = decimate(&t120, 1000.0).iter().map(|p| p.0).collect();
        assert_eq!(d, (0..12).collect::<Vec<_>>());
        assert!(decimate(&[], 60.0).is_empty());
    }

    const W3: usize = 64;
    const H3: usize = 16;

    /// Three 64x16 BGRA pictures: flat (compresses), noise (does not: stored raw), a gradient.
    fn v3_frames() -> Vec<Vec<u8>> {
        let flat = vec![0x40u8; W3 * H3 * 4];
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let noise: Vec<u8> = (0..W3 * H3 * 4)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let grad: Vec<u8> = (0..W3 * H3 * 4).map(|i| ((i / 4) % W3) as u8).collect();
        vec![flat, noise, grad]
    }

    /// Write a v3 file with the same building blocks the recorder's packer uses. Returns the
    /// index records and the index offset.
    fn write_v3(
        path: &std::path::Path,
        frames: &[Vec<u8>],
        with_index: bool,
    ) -> (Vec<RecordLoc>, u64) {
        let mut idx = IndexBuilder::new(frames.len());
        let mut body = Vec::new();
        let mut out = Vec::new();
        for (i, raw) in frames.iter().enumerate() {
            let (word, in_out) = compress_frame(raw, &mut out);
            let t = 1_000_000 + i as u64 * 8_333_333;
            body.extend_from_slice(&record_header(t, 0, W3 as u32, H3 as u32, i == 0, 0));
            body.extend_from_slice(&word.to_bytes());
            body.extend_from_slice(if in_out {
                &out[..word.stored_len as usize]
            } else {
                raw
            });
            idx.add(t, word.stored_len);
        }
        let mut tail = Vec::new();
        let sector = idx.tail(&mut tail).unwrap();
        let mut vh = VarFileHeader::new(PixelFormat::Bgra, W3 as u32, H3 as u32, 120, CODEC_LZ4);
        if with_index {
            vh.base.frame_count = frames.len() as u64;
            vh.base.flags = FILE_CLOSED;
            vh.index_sector = sector;
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&vh.to_bytes()).unwrap();
        f.write_all(&body).unwrap();
        if with_index {
            f.write_all(&tail).unwrap();
        }
        (idx.records, sector as u64 * INDEX_ALIGN)
    }

    fn read_all(path: &std::path::Path) -> Result<Vec<Vec<u8>>, String> {
        let info = scan(path)?;
        let mut r = ClipReader::open(path, &info).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for _ in 0..info.frames {
            let mut b = Vec::new();
            r.next(&mut b).map_err(|e| e.to_string())?;
            out.push(b);
        }
        Ok(out)
    }

    fn patch(path: &std::path::Path, at: u64, f: impl FnOnce(u8) -> u8) {
        let mut bytes = std::fs::read(path).unwrap();
        bytes[at as usize] = f(bytes[at as usize]);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn the_v3_header_keeps_the_v2_layout_and_a_v2_reader_refuses_it() {
        let v2 = FileHeader::new(PixelFormat::Bgra, 1920, 1080, 120).to_bytes();
        assert_eq!(u32::from_le_bytes(v2[12..16].try_into().unwrap()), 2);
        assert_eq!(&v2[60..64], &[0, 0, 0, 0]);
        let mut vh = VarFileHeader::new(PixelFormat::Bgra, 1920, 1080, 120, CODEC_LZ4);
        vh.base.frame_count = 7;
        vh.base.flags = FILE_CLOSED;
        vh.index_sector = 9;
        let b = vh.to_bytes();
        // Same bytes as v2 except the version, the flags word and the index position.
        for (i, (x, y)) in b.iter().zip(v2.iter()).enumerate() {
            if !(12..16).contains(&i) && !(48..64).contains(&i) {
                assert_eq!(x, y, "byte {i}");
            }
        }
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 3);
        assert_eq!(
            u32::from_le_bytes(b[56..60].try_into().unwrap()),
            FILE_CLOSED | FILE_INDEXED | (CODEC_LZ4 << 8)
        );
        assert_eq!(VarFileHeader::parse(&b).unwrap().unwrap(), vh);
        assert!(VarFileHeader::parse(&v2).is_none());
        let refused = FileHeader::parse(&b).unwrap().unwrap_err();
        assert!(refused.contains("version 3"), "{refused}");
        let mut bad = b;
        bad[57] = 7; // codec 7
        assert!(VarFileHeader::parse(&bad).unwrap().is_err());
    }

    #[test]
    fn incompressible_frames_are_stored_raw_never_expanded() {
        let f = v3_frames();
        let mut out = Vec::new();
        let (w, in_out) = compress_frame(&f[1], &mut out);
        assert_eq!((w.encoding, in_out), (ENC_STORED, false));
        assert_eq!(w.stored_len as usize, f[1].len());
        let (w0, in0) = compress_frame(&f[0], &mut out);
        assert_eq!((w0.encoding, in0), (ENC_LZ4, true));
        assert!((w0.stored_len as usize) < f[0].len() / 10);
        let mut back = Vec::new();
        decode_frame(
            0,
            &w0,
            &out[..w0.stored_len as usize],
            f[0].len(),
            &mut back,
        )
        .unwrap();
        assert_eq!(back, f[0]);
        decode_frame(1, &w, &f[1], f[1].len(), &mut back).unwrap();
        assert_eq!(back, f[1]);
    }

    #[test]
    fn a_v3_take_round_trips_bit_exact_and_its_index_matches_a_walk() {
        let frames = v3_frames();
        let p = temp("v3.seraw");
        let (records, _) = write_v3(&p, &frames, true);
        let info = scan(&p).unwrap();
        let t = info.table.as_ref().unwrap();
        assert!(t.from_index);
        assert_eq!((t.stored_lz4, t.stored_raw), (2, 1));
        assert_eq!(t.records, records);
        assert_eq!(info.frames, 3);
        assert_eq!(info.layout.declared_frames, Some(3));
        assert_eq!(info.times, [0, 8_333_333, 16_666_666]);
        assert_eq!(info.idr, [true, false, false]);
        assert_eq!(read_all(&p).unwrap(), frames);
        // Index versus a walk of the records.
        let walk = scan_records(&p, false).unwrap();
        assert!(!walk.table.from_index);
        assert_eq!(walk.table.records, records);
        // Seeking: frame 2, then frame 1.
        let mut r = ClipReader::open(&p, &info).unwrap();
        let mut b = Vec::new();
        r.seek_frame(2).unwrap();
        r.next(&mut b).unwrap();
        assert_eq!(b, frames[2]);
        r.seek_frame(1).unwrap();
        r.next(&mut b).unwrap();
        assert_eq!(b, frames[1]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_v3_take_without_an_index_or_cut_short_yields_its_complete_frames() {
        let frames = v3_frames();
        // Never closed: no index, frame count 0.
        let p = temp("v3-noidx.seraw");
        let (records, index_at) = write_v3(&p, &frames, false);
        let info = scan(&p).unwrap();
        assert!(!info.table.as_ref().unwrap().from_index);
        assert_eq!(info.frames, 3);
        assert_eq!(info.layout.declared_frames, None);
        assert_eq!(read_all(&p).unwrap(), frames);
        // A zero-filled tail (the preallocated reserve of a crashed take) ends the records.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0u8; 5000]).unwrap();
        drop(f);
        assert_eq!(scan(&p).unwrap().frames, 3);
        // Cut inside frame 2: frames 0 and 1.
        let cut = records[2].offset + 60;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(cut)
            .unwrap();
        let info = scan(&p).unwrap();
        assert_eq!(info.frames, 2);
        assert_eq!(read_all(&p).unwrap(), frames[..2]);
        let _ = std::fs::remove_file(&p);

        // Closed, then cut inside the index: the records are walked instead.
        let p = temp("v3-cutidx.seraw");
        write_v3(&p, &frames, true);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(index_at + 30)
            .unwrap();
        let info = scan(&p).unwrap();
        let t = info.table.as_ref().unwrap();
        assert!(!t.from_index);
        assert!(t.index_note.as_deref().unwrap().contains("truncated"));
        assert_eq!(info.frames, 3);
        assert_eq!(read_all(&p).unwrap(), frames);
        // Cut inside the file header.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(40)
            .unwrap();
        assert!(scan(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corruption_is_an_error_naming_the_frame() {
        let frames = v3_frames();
        let p = temp("v3-bad.seraw");
        let (records, index_at) = write_v3(&p, &frames, true);
        // A flipped byte inside frame 0's LZ4 data: the scan is fine, reading frame 0 is not.
        patch(&p, records[0].offset + VAR_RECORD_HEAD as u64 + 5, |b| {
            b ^ 0x55
        });
        let e = read_all(&p).unwrap_err();
        assert!(e.contains("frame 0"), "{e}");
        // A flipped byte in frame 1 (stored raw): the CRC catches it.
        write_v3(&p, &frames, true);
        patch(&p, records[1].offset + VAR_RECORD_HEAD as u64 + 100, |b| {
            b ^ 1
        });
        let e = read_all(&p).unwrap_err();
        assert!(e.contains("frame 1") && e.contains("CRC"), "{e}");
        // A bad length word on frame 2.
        write_v3(&p, &frames, true);
        patch(&p, records[2].offset + HEADER as u64 + 3, |_| 0x7F);
        let e = scan(&p).unwrap_err();
        assert!(e.contains("frame 2"), "{e}");
        // A corrupt index.
        write_v3(&p, &frames, true);
        patch(&p, index_at + INDEX_HEADER as u64 + 2, |b| b ^ 1);
        let e = scan(&p).unwrap_err();
        assert!(e.contains("index"), "{e}");
        // An unknown encoding.
        write_v3(&p, &frames, true);
        patch(&p, records[0].offset + HEADER as u64 + 4, |_| 9);
        let e = scan(&p).unwrap_err();
        assert!(e.contains("frame 0") && e.contains("encoding"), "{e}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reorder_releases_in_sequence_whatever_order_items_finish_in() {
        let mut r: Reorder<u64> = Reorder::new(4);
        let mut out = Vec::new();
        for seq in [2u64, 0, 3, 1, 5, 4, 7, 6] {
            r.insert(seq, seq).unwrap();
            while let Some(x) = r.pop() {
                out.push(x);
            }
        }
        assert_eq!(out, (0..8).collect::<Vec<_>>());
        // Beyond the window, or already released, or twice: refused, never misplaced.
        assert_eq!(r.insert(8 + 4, 12), Err(12));
        assert_eq!(r.insert(3, 3), Err(3));
        r.insert(9, 9).unwrap();
        assert_eq!(r.insert(9, 9), Err(9));
        assert_eq!(r.pop(), None);
    }
}
