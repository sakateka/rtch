//! Independent checksummed zstd frames, with skippable rtch metadata.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub const FRAME_LIMIT: usize = 128 * 1024;
pub const HEADER_SIZE: u64 = 40;
pub const HEADER_BYTES: usize = 40;
const TRAILER_SIZE: usize = 24;
const HEADER_MAGIC: u32 = 0x184d_2a50;
const TRAILER_MAGIC: u32 = 0x184d_2a51;
const FRAME_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const ENCODED_LIMIT: usize = FRAME_LIMIT + 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub fn next_sequence(sequence: u64) -> io::Result<u64> {
    sequence
        .checked_add(1)
        .filter(|&n| n < u64::MAX)
        .ok_or_else(|| invalid("history sequence exhausted"))
}
fn number(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}
pub fn generation() -> io::Result<u64> {
    let mut bytes = [0; 8];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub generation: u64,
    pub sealed: bool,
    pub cut: bool,
    pub legacy_end: u64,
}
impl Header {
    pub fn write(self, file: &mut File) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES);
        bytes.extend(HEADER_MAGIC.to_le_bytes());
        bytes.extend(32_u32.to_le_bytes());
        bytes.extend(b"rtchlog\0");
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend((u32::from(self.sealed) | u32::from(self.cut) << 1).to_le_bytes());
        bytes.extend(self.generation.to_le_bytes());
        bytes.extend(self.legacy_end.to_le_bytes());
        file.rewind()?;
        file.write_all(&bytes)
    }
}
pub fn header(file: &mut File) -> io::Result<Option<Header>> {
    file.rewind()?;
    let mut magic = [0; 4];
    let n = file.read(&mut magic)?;
    if n < 4 || number(&magic) != HEADER_MAGIC {
        return Ok(None);
    }
    let mut bytes = [0; 36];
    file.read_exact(&mut bytes)
        .map_err(|_| invalid("incomplete rtch history header"))?;
    if number(&bytes[..4]) != 32
        || &bytes[4..12] != b"rtchlog\0"
        || number(&bytes[12..16]) != 1
        || number(&bytes[16..20]) > 3
    {
        return Err(invalid("unsupported rtch history header"));
    }
    Ok(Some(Header {
        generation: u64::from_le_bytes(bytes[20..28].try_into().expect("generation")),
        sealed: number(&bytes[16..20]) & 1 != 0,
        cut: number(&bytes[16..20]) & 2 != 0,
        legacy_end: u64::from_le_bytes(bytes[28..36].try_into().expect("legacy offset")),
    }))
}

pub struct Codec {
    compressor: zstd::bulk::Compressor<'static>,
    decompressor: zstd::bulk::Decompressor<'static>,
}
impl Codec {
    pub fn new() -> io::Result<Self> {
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        compressor.include_checksum(true)?;
        let mut decompressor = zstd::bulk::Decompressor::new()?;
        decompressor.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(17))?;
        Ok(Self {
            compressor,
            decompressor,
        })
    }
    pub fn encode(&mut self, bytes: &[u8], sequence: u64) -> io::Result<Vec<u8>> {
        if bytes.is_empty() || bytes.len() > FRAME_LIMIT || sequence == 0 || sequence == u64::MAX {
            return Err(invalid("invalid history record input"));
        }
        let mut encoded = self.compressor.compress(bytes)?;
        let size = u32::try_from(encoded.len()).map_err(|_| invalid("oversized frame"))?;
        encoded.extend(TRAILER_MAGIC.to_le_bytes());
        encoded.extend(16_u32.to_le_bytes());
        encoded.extend(size.to_le_bytes());
        encoded.extend(
            u32::try_from(bytes.len())
                .expect("bounded frame")
                .to_le_bytes(),
        );
        encoded.extend(sequence.to_le_bytes());
        Ok(encoded)
    }
    /// Shorten an oversized record without ever staging plaintext on disk.
    pub fn fitting(
        &mut self,
        bytes: &[u8],
        sequence: u64,
        budget: usize,
        beginning: bool,
    ) -> io::Result<(Vec<u8>, usize)> {
        let mut n = bytes.len().min(FRAME_LIMIT);
        // At most one attempt per halving, plus the initial full-size attempt.
        // The fitting result remains contiguous; omitted prefix bytes seal capture.
        while n > 0 {
            let part = if beginning {
                &bytes[..n]
            } else {
                &bytes[bytes.len() - n..]
            };
            let encoded = self.encode(part, sequence)?;
            if encoded.len() <= budget {
                return Ok((encoded, n));
            }
            n = n.saturating_sub((encoded.len() - budget).max(n.div_ceil(2)));
        }
        Ok((Vec::new(), 0))
    }
    pub fn decode(&mut self, file: &mut File, record: Record) -> io::Result<Vec<u8>> {
        let mut encoded = vec![0; record.encoded];
        file.seek(SeekFrom::Start(record.start))?;
        file.read_exact(&mut encoded)?;
        if encoded.len() < 5 || encoded[..4] != FRAME_MAGIC || encoded[4] & 4 == 0 {
            return Err(invalid("history frame has no checksum"));
        }
        let size = zstd::zstd_safe::get_frame_content_size(&encoded)
            .map_err(|_| invalid("invalid history frame size"))?;
        if size != Some(record.decoded as u64) {
            return Err(invalid("history frame length mismatch"));
        }
        let mut decoded = vec![0; record.decoded];
        let n = self
            .decompressor
            .decompress_to_buffer(&encoded, decoded.as_mut_slice())
            .map_err(|e| invalid(&format!("corrupt history frame: {e}")))?;
        if n != record.decoded {
            return Err(invalid("history decoded length mismatch"));
        }
        Ok(decoded)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub start: u64,
    pub end: u64,
    pub encoded: usize,
    pub decoded: usize,
    pub sequence: u64,
}
fn trailer(bytes: &[u8], end: u64) -> io::Result<Record> {
    if number(&bytes[..4]) != TRAILER_MAGIC || number(&bytes[4..8]) != 16 {
        return Err(invalid("invalid history record trailer"));
    }
    let encoded = number(&bytes[8..12]) as usize;
    let decoded = number(&bytes[12..16]) as usize;
    let sequence = u64::from_le_bytes(bytes[16..24].try_into().expect("sequence"));
    if encoded == 0
        || encoded > ENCODED_LIMIT
        || decoded == 0
        || decoded > FRAME_LIMIT
        || sequence == 0
        || sequence == u64::MAX
    {
        return Err(invalid("invalid history record bounds"));
    }
    let size = encoded
        .checked_add(TRAILER_SIZE)
        .ok_or_else(|| invalid("history record overflow"))?;
    let start = end
        .checked_sub(size as u64)
        .filter(|&start| start >= HEADER_SIZE)
        .ok_or_else(|| invalid("invalid history record length"))?;
    Ok(Record {
        start,
        end,
        encoded,
        decoded,
        sequence,
    })
}
pub fn previous(file: &mut File, end: u64) -> io::Result<Option<Record>> {
    if end == HEADER_SIZE {
        return Ok(None);
    }
    if end < HEADER_SIZE + TRAILER_SIZE as u64 {
        return Err(invalid("incomplete history record"));
    }
    file.seek(SeekFrom::Start(end - TRAILER_SIZE as u64))?;
    let mut bytes = [0; TRAILER_SIZE];
    file.read_exact(&mut bytes)?;
    trailer(&bytes, end).map(Some)
}
/// An incomplete last frame/trailer is uncommitted. All complete records are strict.
pub fn next(file: &mut File, start: u64, end: u64) -> io::Result<Option<Record>> {
    if start == end {
        return Ok(None);
    }
    if start > end {
        return Err(invalid("invalid history read bounds"));
    }
    let remaining = usize::try_from((end - start).min((ENCODED_LIMIT + TRAILER_SIZE) as u64))
        .expect("bounded encoded frame");
    let mut length = remaining.min(64);
    let (bytes, encoded) = loop {
        let mut bytes = vec![0; length];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        let prefix = bytes.len().min(4);
        if bytes[..prefix] != FRAME_MAGIC[..prefix] {
            return Err(invalid("invalid history frame magic"));
        }
        if zstd::zstd_safe::get_frame_content_size(&bytes)
            .is_ok_and(|size| size.is_some_and(|size| size > FRAME_LIMIT as u64))
        {
            return Err(invalid("oversized decoded history frame"));
        }
        match zstd::zstd_safe::find_frame_compressed_size(&bytes) {
            Ok(n) if bytes.len() >= n + TRAILER_SIZE || length == remaining => break (bytes, n),
            Ok(_) => {}
            Err(error)
                if zstd::zstd_safe::get_error_name(error) == "Src size is incorrect"
                    || bytes.len() < 5 =>
            {
                if length == remaining && length < ENCODED_LIMIT + TRAILER_SIZE {
                    return Ok(None);
                }
            }
            Err(_) => return Err(invalid("invalid history frame structure")),
        }
        if length == remaining {
            return Err(invalid("oversized history frame"));
        }
        length = (length * 2).min(remaining);
    };
    if encoded > ENCODED_LIMIT {
        return Err(invalid("oversized history frame"));
    }
    if bytes.len() < encoded + TRAILER_SIZE {
        return Ok(None);
    }
    let record = trailer(
        &bytes[encoded..encoded + TRAILER_SIZE],
        start + (encoded + TRAILER_SIZE) as u64,
    )?;
    if record.start != start {
        return Err(invalid("history encoded length mismatch"));
    }
    Ok(Some(record))
}
fn committed_next(file: &mut File, start: u64, end: u64) -> io::Result<Option<Record>> {
    let record = next(file, start, end)?;
    if record.is_none() && start != end {
        return Err(invalid("committed history boundary not reached"));
    }
    Ok(record)
}
pub fn committed_end(file: &mut File) -> io::Result<u64> {
    let end = file.metadata()?.len();
    if end == HEADER_SIZE {
        return Ok(end);
    }
    if end >= HEADER_SIZE + TRAILER_SIZE as u64 {
        file.seek(SeekFrom::Start(end - TRAILER_SIZE as u64))?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        if number(&magic) == TRAILER_MAGIC
            && let Ok(Some(record)) = previous(file, end)
        {
            // A structurally valid trailer commits this frame. It must parse all
            // the way to that boundary, even if its block length was corrupted.
            let parsed = committed_next(file, record.start, end)?
                .ok_or_else(|| invalid("missing committed frame"))?;
            if parsed.end != end {
                return Err(invalid("invalid committed frame boundary"));
            }
            return Ok(end);
        }
    }
    // Exceptional partial-tail recovery scans metadata, never decoded history.
    let mut offset = HEADER_SIZE;
    let mut sequence = 0;
    while let Some(record) = next(file, offset, end)? {
        if record.sequence <= sequence {
            return Err(invalid("unordered history records"));
        }
        sequence = record.sequence;
        offset = record.end;
    }
    Ok(offset)
}
pub fn validate(file: &mut File, codec: &mut Codec) -> io::Result<(u64, u64, usize)> {
    let end = committed_end(file)?;
    let mut offset = HEADER_SIZE;
    let mut sequence = 0;
    let mut decoded = 0_usize;
    while let Some(record) = committed_next(file, offset, end)? {
        if record.sequence <= sequence {
            return Err(invalid("unordered history records"));
        }
        codec.decode(file, record)?;
        decoded = decoded.saturating_add(record.decoded);
        sequence = record.sequence;
        offset = record.end;
    }
    Ok((end, sequence, decoded))
}

pub struct Window {
    pub bytes: Vec<u8>,
    pub cut: bool,
    pub earlier: bool,
    pub end: u64,
    pub sequence: u64,
    pub header: Option<Header>,
}
fn legacy_snapshot(file: &mut File, end: u64, limit: usize, beginning: bool) -> io::Result<Window> {
    let offset = if beginning {
        0
    } else {
        end.saturating_sub(limit as u64)
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take((end - offset).min(limit as u64))
        .read_to_end(&mut bytes)?;
    let consumed = offset + bytes.len() as u64;
    Ok(Window {
        bytes,
        cut: offset > 0,
        earlier: offset > 0,
        end: consumed,
        sequence: 0,
        header: None,
    })
}
pub fn read(file: &mut File, limit: usize, beginning: bool) -> io::Result<Window> {
    let header = header(file)?;
    if header.is_none() {
        let end = file.metadata()?.len();
        return legacy_snapshot(file, end, limit, beginning);
    }
    let end = committed_end(file)?;
    let sequence = previous(file, end)?.map_or(0, |r| r.sequence);
    let mut start = HEADER_SIZE;
    if !beginning {
        start = end;
        let mut length = 0;
        let mut newer = u64::MAX;
        while length < limit {
            let Some(record) = previous(file, start)? else {
                break;
            };
            if record.sequence >= newer {
                return Err(invalid("unordered history records"));
            }
            newer = record.sequence;
            length += record.decoded;
            start = record.start;
        }
    }
    let mut codec = Codec::new()?;
    let mut bytes = Vec::new();
    let mut offset = start;
    let mut last_sequence = 0;
    while let Some(record) = committed_next(file, offset, end)? {
        if record.sequence <= last_sequence {
            return Err(invalid("unordered history records"));
        }
        last_sequence = record.sequence;
        bytes.extend(codec.decode(file, record)?);
        offset = record.end;
        if beginning && bytes.len() >= limit {
            break;
        }
    }
    let requested = if beginning {
        0
    } else {
        bytes.len().saturating_sub(limit)
    };
    let cut = header.is_some_and(|h| h.cut) || start > HEADER_SIZE || requested > 0;
    if beginning {
        bytes.truncate(limit);
    } else {
        bytes.drain(..requested);
    }
    Ok(Window {
        bytes,
        cut,
        earlier: start > HEADER_SIZE || requested > 0,
        end,
        sequence,
        header,
    })
}

fn temporary_path(path: &Path) -> io::Result<PathBuf> {
    let mut base = path
        .file_name()
        .ok_or_else(|| invalid("invalid history path"))?
        .to_string_lossy()
        .into_owned();
    // Compaction during migration replaces another temporary. Keep every stage
    // in the canonical session's namespace so crash recovery can find it.
    while let Some((stem, nonce)) = base.rsplit_once(".rtch-") {
        if !stem.starts_with('.')
            || nonce.len() != 16
            || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
        {
            break;
        }
        base = stem[1..].to_owned();
    }
    Ok(path.with_file_name(format!(".{base}.rtch-{:016x}", generation()?)))
}
fn same_file(path: &Path, file: &File) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .zip(file.metadata().ok())
        .is_some_and(|(a, b)| a.is_file() && a.ino() == b.ino() && a.dev() == b.dev())
}
struct Temporary {
    path: PathBuf,
    file: File,
}
impl Temporary {
    fn create(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)?;
        let result = Self { path, file }; // Cleanup begins only after successful creation.
        crate::os::lock(result.file.as_raw_fd())?;
        Ok(result)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if same_file(&self.path, &self.file) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn replace_at(
    path: &Path,
    temporary: PathBuf,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<File> {
    let mut temporary = Temporary::create(temporary)?;
    write(&mut temporary.file)?;
    fs::rename(&temporary.path, path)?;
    temporary.file.seek(SeekFrom::End(0))?;
    temporary.file.try_clone()
}
/// Create a locked private replacement. Collision failures never own the old path.
pub fn replace(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<File> {
    replace_at(path, temporary_path(path)?, write)
}
#[allow(clippy::verbose_bit_mask)] // Match the existing explicit Unix permission check.
fn private_temporary(metadata: &fs::Metadata, owner: u32) -> bool {
    metadata.is_file() && metadata.uid() == owner && metadata.mode() & 0o077 == 0
}
/// Reopen/removal recovery is scoped to these two exact session artifacts. An
/// exclusive file lock identifies live writers, including migration compaction.
pub fn cleanup_session(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let name = path
        .file_name()
        .ok_or_else(|| invalid("invalid session name"))?
        .to_string_lossy();
    let prefixes = [format!(".{name}.log.rtch-"), format!(".{name}.head.rtch-")];
    for entry in entries {
        let entry = entry?;
        let filename = entry.file_name();
        let text = filename.to_string_lossy();
        if !prefixes.iter().any(|prefix| {
            text.strip_prefix(prefix).is_some_and(|nonce| {
                nonce.len() == 16 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            })
        }) {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !private_temporary(&metadata, crate::os::uid()) {
            continue;
        }
        let Ok(mut file) = crate::storage::open_file(&entry.path(), false) else {
            continue;
        };
        if !private_temporary(&file.metadata()?, crate::os::uid())
            || !crate::os::try_lock(file.as_raw_fd())?
        {
            continue;
        }
        let mut magic = [0; 4];
        let n = file.read(&mut magic)?;
        let expected = HEADER_MAGIC.to_le_bytes();
        if magic[..n] != expected[..n] {
            continue;
        }
        if n == 4
            && file.metadata()?.len() >= HEADER_SIZE
            && !matches!(header(&mut file), Ok(Some(_)))
        {
            continue;
        }
        if same_file(&entry.path(), &file) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub struct Container {
    pub file: File,
    pub path: PathBuf,
    pub header: Header,
    pub end: u64,
    pub sequence: u64,
}
impl Container {
    pub fn compact(&mut self, allowance: usize, incoming: usize) -> io::Result<()> {
        if self.end + incoming as u64 <= allowance as u64 {
            return Ok(());
        }
        let target = (allowance - HEADER_BYTES).saturating_mul(7) / 8;
        let keep = target.max(incoming).saturating_sub(incoming);
        let mut start = self.end;
        while let Some(record) = previous(&mut self.file, start)? {
            if self.end - record.start > keep as u64 {
                break;
            }
            start = record.start;
        }
        self.header.cut |= start > HEADER_SIZE;
        let header = self.header;
        let count = self.end - start;
        self.file.seek(SeekFrom::Start(start))?;
        self.file = replace(&self.path, |file| {
            header.write(file)?;
            io::copy(&mut Read::by_ref(&mut self.file).take(count), file)?;
            Ok(())
        })?;
        self.end = HEADER_SIZE + count;
        Ok(())
    }
    pub fn append(&mut self, encoded: &[u8], allowance: usize) -> io::Result<()> {
        if encoded.is_empty() {
            return Ok(());
        }
        if encoded.len() < TRAILER_SIZE {
            return Err(invalid("short prepared history record"));
        }
        let record = trailer(
            &encoded[encoded.len() - TRAILER_SIZE..],
            HEADER_SIZE + encoded.len() as u64,
        )?;
        if record.start != HEADER_SIZE || record.sequence <= self.sequence {
            return Err(invalid("invalid prepared history sequence"));
        }
        self.compact(allowance, encoded.len())?;
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(encoded)?;
        self.end += encoded.len() as u64;
        self.sequence = record.sequence;
        Ok(())
    }
}

const FOLLOW_BATCH: u64 = FRAME_LIMIT as u64;
fn deliver_record(
    codec: &mut Codec,
    file: &mut File,
    record: Record,
    sequence: &mut u64,
    header: Header,
    legacy_seen: Option<u64>,
    consume: &mut impl FnMut(&[u8], bool) -> io::Result<()>,
) -> io::Result<()> {
    if record.sequence <= *sequence {
        return Err(invalid("unordered followed history"));
    }
    let legacy_record = record.sequence <= header.legacy_end;
    let gap = if legacy_record {
        record.sequence.saturating_sub(record.decoded as u64) > *sequence
    } else {
        record.sequence != next_sequence(*sequence)?
    };
    if gap {
        consume(&[], true)?;
    }
    let decoded = codec.decode(file, record)?;
    let skip = if legacy_record {
        legacy_seen.map_or(0, |seen| {
            usize::try_from(
                seen.saturating_sub(record.sequence.saturating_sub(record.decoded as u64)),
            )
            .unwrap_or(usize::MAX)
            .min(decoded.len())
        })
    } else {
        0
    };
    consume(&decoded[skip..], false)?;
    *sequence = record.sequence;
    Ok(())
}
pub struct Follower {
    pub file: File,
    pub path: PathBuf,
    pub end: u64,
    pub sequence: u64,
    pub header: Option<Header>,
    codec: Codec,
}
impl Follower {
    pub fn new(file: File, path: PathBuf, window: &Window) -> io::Result<Self> {
        Ok(Self {
            file,
            path,
            end: window.end,
            sequence: window.sequence,
            header: window.header,
            codec: Codec::new()?,
        })
    }
    /// True means progress was made, so callers should check signals and poll
    /// again immediately. Each legacy batch is bounded to its captured snapshot.
    pub fn poll(
        &mut self,
        mut consume: impl FnMut(&[u8], bool) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mut current = crate::storage::open_file(&self.path, false)?;
        let header = header(&mut current)?;
        let old = self.file.metadata()?;
        let metadata = current.metadata()?;
        let replaced = old.ino() != metadata.ino() || old.dev() != metadata.dev();
        let legacy_transition =
            self.header.is_none() && replaced && header.is_some_and(|h| h.legacy_end >= self.end);
        let legacy_seen = self.end;
        let reset = !legacy_transition
            && (header.map(|h| h.generation) != self.header.map(|h| h.generation)
                || header.is_none() && (replaced || metadata.len() < self.end));
        let mut progressed = false;
        if legacy_transition {
            self.sequence = legacy_seen;
        }
        if reset {
            self.end = header.map_or(0, |_| HEADER_SIZE);
            self.sequence = 0;
            consume(&[], true)?;
        } else if replaced && let Some(old_header) = self.header {
            // Recover unread committed records still available through the old
            // descriptor before switching to the retained suffix. Clear skips this.
            let end = committed_end(&mut self.file)?;
            let mut decoded = 0;
            while let Some(record) = committed_next(&mut self.file, self.end, end)? {
                deliver_record(
                    &mut self.codec,
                    &mut self.file,
                    record,
                    &mut self.sequence,
                    old_header,
                    None,
                    &mut consume,
                )?;
                self.end = record.end;
                progressed = true;
                decoded += record.decoded as u64;
                if decoded >= FOLLOW_BATCH && self.end < end {
                    return Ok(true);
                }
            }
        }
        if let Some(compressed_header) = header {
            let committed = committed_end(&mut current)?;
            if replaced || reset || metadata.len() < self.end {
                self.end = committed;
                while let Some(record) = previous(&mut current, self.end)? {
                    if record.sequence <= self.sequence {
                        break;
                    }
                    self.end = record.start;
                }
            }
            let mut decoded = 0;
            while let Some(record) = committed_next(&mut current, self.end, committed)? {
                deliver_record(
                    &mut self.codec,
                    &mut current,
                    record,
                    &mut self.sequence,
                    compressed_header,
                    legacy_transition.then_some(legacy_seen),
                    &mut consume,
                )?;
                self.end = record.end;
                progressed = true;
                decoded += record.decoded as u64;
                if decoded >= FOLLOW_BATCH {
                    break;
                }
            }
        } else {
            current.seek(SeekFrom::Start(self.end))?;
            let end = metadata.len().min(self.end.saturating_add(FOLLOW_BATCH));
            let mut bytes = [0; 8192];
            while self.end < end {
                let length = usize::try_from((end - self.end).min(bytes.len() as u64))
                    .expect("bounded legacy batch");
                let n = current.read(&mut bytes[..length])?;
                if n == 0 {
                    break;
                }
                consume(&bytes[..n], false)?;
                self.end += n as u64;
                progressed = true;
            }
        }
        self.header = header;
        self.file = current;
        Ok(progressed)
    }
}

/// Convert only at supervisor reopen, retaining bounded encoded records as we go.
pub fn migrate(
    path: &Path,
    source: Option<&mut File>,
    allowance: usize,
    header: Header,
    codec: &mut Codec,
) -> io::Result<Container> {
    let mut temporary = Temporary::create(temporary_path(path)?)?;
    header.write(&mut temporary.file)?;
    let mut container = Container {
        file: temporary.file.try_clone()?,
        path: temporary.path.clone(),
        header,
        end: HEADER_SIZE,
        sequence: 0,
    };
    if let Some(source) = source {
        source.rewind()?;
        let mut bytes = vec![0; FRAME_LIMIT];
        let source_end = source.metadata()?.len();
        let mut consumed = 0_u64;
        while consumed < source_end {
            let length = usize::try_from((source_end - consumed).min(bytes.len() as u64))
                .expect("bounded migration");
            let n = source.read(&mut bytes[..length])?;
            if n == 0 {
                break;
            }
            consumed = consumed
                .checked_add(n as u64)
                .filter(|&n| n < u64::MAX)
                .ok_or_else(|| invalid("migration sequence exhausted"))?;
            let (encoded, kept) =
                codec.fitting(&bytes[..n], consumed, allowance - HEADER_BYTES, false)?;
            if kept < n {
                container.header.cut = true;
                container.header.write(&mut container.file)?;
            }
            let result = container.append(&encoded, allowance);
            temporary.file = container.file.try_clone()?;
            result?;
        }
    }
    fs::rename(&temporary.path, path)?;
    path.clone_into(&mut container.path);
    Ok(container)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rtch-codec-review-{:016x}", generation().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn file(&self, name: &str) -> File {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(self.path(name))
                .unwrap()
        }
        fn container(&self) -> Container {
            let mut file = self.file("history");
            let header = Header {
                generation: 1,
                sealed: false,
                cut: false,
                legacy_end: 0,
            };
            header.write(&mut file).unwrap();
            Container {
                file,
                path: self.path("history"),
                header,
                end: HEADER_SIZE,
                sequence: 0,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn append(container: &mut Container, codec: &mut Codec, bytes: &[u8]) {
        let encoded = codec
            .encode(bytes, next_sequence(container.sequence).unwrap())
            .unwrap();
        container.append(&encoded, 1024 * 1024).unwrap();
    }
    fn follower(container: &Container) -> Follower {
        let mut file = File::open(&container.path).unwrap();
        let window = read(&mut file, FRAME_LIMIT, false).unwrap();
        Follower::new(file, container.path.clone(), &window).unwrap()
    }
    #[test]
    fn incomplete_frame_with_false_eof_trailer_magic_waits_without_error() {
        let fixture = Fixture::new();
        let mut container = fixture.container();
        let mut codec = Codec::new().unwrap();
        append(&mut container, &mut codec, b"SEEN\n");
        let committed = container.end;
        let mut follower = follower(&container);
        let random = (0..8192)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect::<Vec<_>>();
        let encoded = codec.encode(&random, 2).unwrap();
        // A raw-block payload is cut short, then happens to end in trailer magic.
        container.file.write_all(&encoded[..10]).unwrap();
        container
            .file
            .write_all(&TRAILER_MAGIC.to_le_bytes())
            .unwrap();
        container.file.write_all(&[0xff; 20]).unwrap();
        assert_eq!(committed_end(&mut container.file).unwrap(), committed);
        assert!(
            !follower
                .poll(|bytes, reset| {
                    assert!(!reset && bytes.is_empty());
                    Ok(())
                })
                .unwrap()
        );
    }
    #[test]
    fn beginning_reads_reject_decreasing_and_duplicate_selected_sequences() {
        for sequences in [[2, 1], [1, 1]] {
            let fixture = Fixture::new();
            let mut container = fixture.container();
            let mut codec = Codec::new().unwrap();
            for sequence in sequences {
                container
                    .file
                    .write_all(&codec.encode(b"line\n", sequence).unwrap())
                    .unwrap();
            }
            assert!(read(&mut container.file, FRAME_LIMIT, true).is_err());
        }
    }
    #[test]
    fn malformed_maximum_encoded_length_is_rejected_before_size_arithmetic() {
        let mut bytes = [0; TRAILER_SIZE];
        bytes[..4].copy_from_slice(&TRAILER_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&16_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[12..16].copy_from_slice(&1_u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&1_u64.to_le_bytes());
        assert!(trailer(&bytes, HEADER_SIZE + TRAILER_SIZE as u64).is_err());
    }
    #[test]
    fn exhausted_sequences_are_rejected_without_publishing_or_wrapping() {
        let fixture = Fixture::new();
        let mut container = fixture.container();
        let mut codec = Codec::new().unwrap();
        let record = codec.encode(b"last usable record", u64::MAX - 1).unwrap();
        container.append(&record, 1024).unwrap();
        let before = fs::read(&container.path).unwrap();
        assert!(next_sequence(container.sequence).is_err());
        assert!(codec.encode(b"forbidden", u64::MAX).is_err());
        let mut forbidden = codec.encode(b"forbidden", 1).unwrap();
        let length = forbidden.len();
        forbidden[length - 8..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(container.append(&forbidden, HEADER_BYTES).is_err());
        assert_eq!(fs::read(&container.path).unwrap(), before);
        assert_eq!(
            read(&mut container.file, FRAME_LIMIT, false)
                .unwrap()
                .sequence,
            u64::MAX - 1
        );
    }
    #[test]
    fn replacement_collision_preserves_preexisting_temporary_and_destination() {
        let fixture = Fixture::new();
        let target = fixture.path("target");
        let temporary = fixture.path("preexisting");
        fixture.file("target").write_all(b"TARGET").unwrap();
        fixture.file("preexisting").write_all(b"PRESERVE").unwrap();
        assert!(
            replace_at(&target, temporary.clone(), |_| panic!(
                "write after create failure"
            ))
            .is_err()
        );
        assert_eq!(fs::read(temporary).unwrap(), b"PRESERVE");
        assert_eq!(fs::read(target).unwrap(), b"TARGET");
    }
    #[test]
    fn abandoned_temporary_cleanup_preserves_live_unrelated_symlink_nonprivate_and_other_owner() {
        let fixture = Fixture::new();
        let session = fixture.path("session");
        let header = Header {
            generation: 1,
            sealed: false,
            cut: false,
            legacy_end: 0,
        };
        for name in [
            ".session.log.rtch-0000000000000001",
            ".session.head.rtch-0000000000000002",
            ".other.log.rtch-0000000000000003",
        ] {
            let mut file = fixture.file(name);
            header.write(&mut file).unwrap();
            file.write_all(b"interrupted encoded copy").unwrap();
        }
        let live_path = fixture.path(".session.log.rtch-0000000000000004");
        let mut live = Temporary::create(live_path.clone()).unwrap();
        header.write(&mut live.file).unwrap();
        let unrelated = fixture.path(".session.log.rtch-0000000000000005");
        fixture
            .file(unrelated.file_name().unwrap().to_str().unwrap())
            .write_all(b"UNRELATED")
            .unwrap();
        let public = fixture.path(".session.head.rtch-0000000000000006");
        let mut file = fixture.file(public.file_name().unwrap().to_str().unwrap());
        header.write(&mut file).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o644)).unwrap();
        let link = fixture.path(".session.log.rtch-0000000000000007");
        symlink(&unrelated, &link).unwrap();
        // The same metadata must also be rejected for a different expected owner.
        assert!(!private_temporary(
            &live.file.metadata().unwrap(),
            crate::os::uid().wrapping_add(1)
        ));
        cleanup_session(&session).unwrap();
        for name in [
            ".session.log.rtch-0000000000000001",
            ".session.head.rtch-0000000000000002",
        ] {
            assert!(!fixture.path(name).exists());
        }
        assert!(fixture.path(".other.log.rtch-0000000000000003").exists());
        assert!(live_path.exists() && public.exists());
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(unrelated).unwrap(), b"UNRELATED");
    }
    #[test]
    fn compacted_and_migrated_suffixes_remember_truncation_after_full_retained_read() {
        let fixture = Fixture::new();
        let mut container = fixture.container();
        let mut codec = Codec::new().unwrap();
        append(&mut container, &mut codec, b"\x1b]0;");
        let encoded = codec.encode(b"ORPHAN_PAYLOAD\n\x1b\\VISIBLE\n", 2).unwrap();
        container.append(&encoded, 1024).unwrap();
        container
            .compact(HEADER_BYTES + (encoded.len() * 8).div_ceil(7), 0)
            .unwrap();
        let window = read(&mut container.file, FRAME_LIMIT, false).unwrap();
        assert!(window.cut && !window.earlier);
        let boundary = crate::history::emulator_boundary(&window.bytes, window.cut);
        assert_eq!(&window.bytes[boundary..], b"VISIBLE\n");
        let mut source = fixture.file("legacy");
        let mut state = 0x1234_5678_u32;
        let random = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect::<Vec<_>>();
        source.write_all(&random).unwrap();
        source
            .write_all(b"ORPHAN_PAYLOAD\n\x1b\\VISIBLE\n")
            .unwrap();
        let legacy_end = source.metadata().unwrap().len();
        let mut migrated = migrate(
            &fixture.path("migrated"),
            Some(&mut source),
            128,
            Header {
                generation: 2,
                sealed: false,
                cut: false,
                legacy_end,
            },
            &mut codec,
        )
        .unwrap();
        let window = read(&mut migrated.file, FRAME_LIMIT, false).unwrap();
        assert!(window.cut && !window.earlier);
        let boundary = crate::history::emulator_boundary(&window.bytes, window.cut);
        assert_eq!(&window.bytes[boundary..], b"VISIBLE\n");
    }
    #[test]
    fn follower_resets_filter_after_evicted_terminator_sequence_gap() {
        let fixture = Fixture::new();
        let mut container = fixture.container();
        let mut codec = Codec::new().unwrap();
        append(&mut container, &mut codec, b"\x1b]10;?");
        let mut follower = follower(&container);
        let mut filter = crate::history::Filter::default();
        assert!(filter.feed(b"\x1b]10;?").is_empty());
        container.compact(HEADER_BYTES, 0).unwrap();
        append(&mut container, &mut codec, b"\x07");
        container.compact(HEADER_BYTES, 0).unwrap();
        append(&mut container, &mut codec, b"VISIBLE_AFTER_GAP\n");
        let mut seen = Vec::new();
        let mut resets = 0;
        follower
            .poll(|bytes, reset| {
                if reset {
                    resets += 1;
                    filter = crate::history::Filter::default();
                }
                seen.extend(filter.feed(bytes));
                Ok(())
            })
            .unwrap();
        assert_eq!(resets, 1);
        assert_eq!(seen, b"VISIBLE_AFTER_GAP\n");
    }
    #[test]
    fn follower_drains_old_inode_before_rotation_and_deduplicates_retained_records() {
        let fixture = Fixture::new();
        let mut container = fixture.container();
        let mut codec = Codec::new().unwrap();
        append(&mut container, &mut codec, b"SEEN\n");
        let mut follower = follower(&container);
        append(&mut container, &mut codec, b"UNREAD_OLD_A\n");
        append(&mut container, &mut codec, b"UNREAD_OLD_B\n");
        let last = previous(&mut container.file, container.end)
            .unwrap()
            .unwrap();
        let length = last.encoded + TRAILER_SIZE;
        container
            .compact(HEADER_BYTES + (length * 8).div_ceil(7), 0)
            .unwrap();
        append(&mut container, &mut codec, b"NEW_C\n");
        let mut seen = Vec::<u8>::new();
        follower
            .poll(|bytes, reset| {
                assert!(!reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"UNREAD_OLD_A\nUNREAD_OLD_B\nNEW_C\n");
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"UNREAD_OLD_A\nUNREAD_OLD_B\nNEW_C\n");
        append(&mut container, &mut codec, b"CLEARED_UNREAD\n");
        container.header.generation = 2;
        container.file = replace(&container.path, |file| container.header.write(file)).unwrap();
        container.end = HEADER_SIZE;
        container.sequence = 0;
        append(&mut container, &mut codec, b"AFTER_CLEAR\n");
        seen.clear();
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"AFTER_CLEAR\n");
    }
    #[test]
    fn legacy_snapshot_excludes_appends_after_captured_end_and_follow_emits_once() {
        let fixture = Fixture::new();
        let mut writer = fixture.file("legacy");
        writer.write_all(b"SEEN").unwrap();
        let captured = writer.metadata().unwrap().len();
        writer.write_all(b"UNSEEN").unwrap();
        let mut file = File::open(fixture.path("legacy")).unwrap();
        let window = legacy_snapshot(&mut file, captured, FRAME_LIMIT, false).unwrap();
        assert_eq!(window.bytes, b"SEEN");
        assert_eq!(window.end, captured);
        let mut follower = Follower::new(file, fixture.path("legacy"), &window).unwrap();
        let mut seen = Vec::<u8>::new();
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"UNSEEN");
    }
    #[test]
    fn growing_legacy_writer_cannot_extend_poll_past_its_snapshot_or_batch() {
        let fixture = Fixture::new();
        let mut writer = fixture.file("legacy");
        writer.write_all(b"SEEN").unwrap();
        let mut file = File::open(fixture.path("legacy")).unwrap();
        let window = read(&mut file, FRAME_LIMIT, false).unwrap();
        let mut follower = Follower::new(file, fixture.path("legacy"), &window).unwrap();
        writer.write_all(&vec![b'x'; 2 * FRAME_LIMIT]).unwrap();
        for _ in 0..2 {
            let position = follower.end;
            let mut calls = 0;
            let progress = follower
                .poll(|bytes, reset| {
                    assert!(!reset);
                    calls += 1;
                    assert!(
                        calls <= FRAME_LIMIT / 8192,
                        "poll followed the growing writer indefinitely"
                    );
                    writer.write_all(bytes).unwrap();
                    Ok(())
                })
                .unwrap();
            assert!(progress);
            assert_eq!(follower.end - position, FOLLOW_BATCH);
            assert_eq!(calls, FRAME_LIMIT / 8192);
        }
    }
}
