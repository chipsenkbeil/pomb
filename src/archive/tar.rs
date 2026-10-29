use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use super::ArchiveError;
use super::import::ImportError;
use super::spool::Spool;

/// Bytes in a tar block, which every header fills and every file's data pads to.
pub(super) const BLOCK: usize = 512;

/// Zero bytes that pad a file's data to a block, and twice the block that ends the archive.
pub(super) const ZEROS: [u8; 2 * BLOCK] = [0; 2 * BLOCK];

/// Longest path, in bytes, a plain header holds.
const NAME_BYTES: usize = 100;

/// Largest size, in bytes, a header's 11 octal digits write.
const SIZE_LIMIT_BYTES: u64 = 1 << 33;

/// Returns the header bytes that open a regular file of `size` bytes at `path`, which is a PAX
/// header with the whole path ahead of the plain one when the path is longer than a plain header
/// holds.
///
/// # Errors
///
/// Returns [`ArchiveError`] when `size` does not fit the header.
pub(super) fn entry_header(path: &str, size: u64, mtime: u64) -> Result<Vec<u8>, ArchiveError> {
    let mut out = Vec::new();
    let mut name = path;
    if path.len() > NAME_BYTES {
        let record = pax_record("path", path);
        out.extend_from_slice(&header(
            "PaxHeaders/entry",
            record.len() as u64,
            mtime,
            b'x',
        )?);
        out.extend_from_slice(record.as_bytes());
        out.extend_from_slice(&ZEROS[..padding(record.len() as u64)]);
        let mut end = NAME_BYTES;
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        name = &path[..end];
    }
    out.extend_from_slice(&header(name, size, mtime, b'0')?);
    Ok(out)
}

/// Returns how many zero bytes pad `size` bytes of data to a whole block.
#[must_use]
pub(super) fn padding(size: u64) -> usize {
    let rest = (size % BLOCK as u64) as usize;
    (BLOCK - rest) % BLOCK
}

/// Returns the PAX record that sets `key` to `value`: its decimal length, a space, `key=value`
/// and a newline, where the length counts its own digits.
#[must_use]
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut digits = 1;
    loop {
        let total = body.len() + digits;
        if total.to_string().len() == digits {
            return format!("{total}{body}");
        }
        digits += 1;
    }
}

/// Returns one ustar header block of type `kind` for `name`, which is at most [`NAME_BYTES`] long.
fn header(name: &str, size: u64, mtime: u64, kind: u8) -> Result<[u8; BLOCK], ArchiveError> {
    if size >= SIZE_LIMIT_BYTES {
        return Err(ArchiveError::new(format!(
            "{name} is {size} bytes, which a tar header cannot hold"
        )));
    }
    let mut block = [0u8; BLOCK];
    block[..name.len()].copy_from_slice(name.as_bytes());
    put_octal(&mut block[100..108], 0o644);
    put_octal(&mut block[108..116], 0);
    put_octal(&mut block[116..124], 0);
    put_octal(&mut block[124..136], size);
    put_octal(&mut block[136..148], mtime);
    block[148..156].fill(b' ');
    block[156] = kind;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    Ok(block)
}

/// Writes `value` into `field` as zero-padded octal digits and leaves the last byte a NUL.
fn put_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(text.as_bytes());
}

/// Longest path, in bytes, the reader accepts, which every path of the archive format fits in.
const PATH_LIMIT_BYTES: usize = 512;

/// Largest PAX header, in bytes, the reader accepts.
const PAX_LIMIT_BYTES: u64 = 4096;

/// Bytes of a spooled file copied at a time, 64 KiB.
const SPOOL_CHUNK_BYTES: usize = 65_536;

/// Where the bytes of a file read from a tar stream are.
#[derive(Debug)]
pub(super) enum Data {
    /// The file is held in memory.
    Memory(Vec<u8>),
    /// The file is written to a file of the spool.
    Spooled(PathBuf),
}

/// What a file of a tar stream may be: how large, and whether it is spooled to disk.
#[derive(Copy, Clone, Debug)]
pub(super) struct Cap {
    /// Most bytes the file may hold.
    pub(super) bytes: u64,
    /// Whether the file is written to the spool, not held in memory.
    pub(super) spooled: bool,
}

/// A regular file read from a tar stream.
#[derive(Debug)]
pub(super) struct TarFile {
    /// The file's path inside the archive.
    pub(super) path: String,
    /// The file's bytes.
    pub(super) data: Data,
}

/// What a tar stream may hold before [`read_files`] refuses it.
#[derive(Copy, Clone, Debug)]
pub(super) struct ReadLimits {
    /// Most bytes the stream may unpack to, headers and padding included.
    pub(super) unpacked_bytes: u64,
    /// Most files the stream may hold.
    pub(super) files: usize,
}

/// Refusal of a stream that unpacks to more bytes than its limit, carried inside an
/// [`io::Error`].
#[derive(Debug)]
struct Unpacked(u64);

impl fmt::Display for Unpacked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the archive unpacks to more than {} bytes", self.0)
    }
}

impl Error for Unpacked {}

/// A reader that fails once more bytes come out of it than its limit allows.
struct Capped<R> {
    /// The reader the bytes come from.
    inner: R,
    /// Bytes still allowed.
    left: u64,
    /// The limit the reader started with.
    limit: u64,
}

impl<R> Read for Capped<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    Unpacked(self.limit),
                )),
            };
        }
        let room = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read = self.inner.read(&mut buf[..room])?;
        self.left -= read as u64;
        Ok(read)
    }
}

/// One entry header as the reader understands it.
struct Entry {
    /// The path the header names, which a PAX header may replace.
    path: String,
    /// The size of the entry's data in bytes.
    size: u64,
    /// The type flag.
    kind: u8,
}

impl Entry {
    /// Returns the entry `block` describes.
    fn parse(block: &[u8; BLOCK]) -> Result<Self, ImportError> {
        let stored = parse_octal(&block[148..156])
            .ok_or_else(|| ImportError::invalid("a tar header has an unreadable checksum"))?;
        let sum: u64 = block
            .iter()
            .enumerate()
            .map(|(at, byte)| {
                if (148..156).contains(&at) {
                    u64::from(b' ')
                } else {
                    u64::from(*byte)
                }
            })
            .sum();
        if sum != stored {
            return Err(ImportError::invalid("a tar header fails its checksum"));
        }
        if &block[257..262] != b"ustar" {
            return Err(ImportError::invalid(
                "a tar header is not in the ustar format",
            ));
        }
        let size = parse_octal(&block[124..136])
            .ok_or_else(|| ImportError::invalid("a tar header has an unreadable size"))?;
        let name = text_until_nul(&block[..NAME_BYTES])?;
        let prefix = text_until_nul(&block[345..500])?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        Ok(Self {
            path,
            size,
            kind: block[156],
        })
    }
}

/// Returns the refusal of a read that failed.
fn read_error(error: io::Error) -> ImportError {
    if let Some(unpacked) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Unpacked>())
    {
        return ImportError::TooLarge(unpacked.to_string());
    }
    if error.kind() == io::ErrorKind::UnexpectedEof {
        return ImportError::invalid("the archive ends before its last entry does");
    }
    ImportError::invalid(format!(
        "the archive is not a readable gzipped tar: {error}"
    ))
}

/// Returns the number a tar numeric `field` writes in octal digits, padded with NULs or spaces.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(['\0', ' ']);
    if digits.is_empty() || !digits.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return None;
    }
    u64::from_str_radix(digits, 8).ok()
}

/// Returns the UTF-8 text of `field` up to its first NUL.
fn text_until_nul(field: &[u8]) -> Result<String, ImportError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map(str::to_string)
        .map_err(|_| ImportError::invalid("a tar path is not UTF-8"))
}

/// Returns the path a PAX header's `data` sets, which is the only key the format uses.
fn pax_path(data: &[u8]) -> Result<String, ImportError> {
    let bad = || ImportError::invalid("an extended tar header is malformed");
    let mut rest = std::str::from_utf8(data).map_err(|_| bad())?;
    let mut path = None;
    while !rest.is_empty() {
        let (length, _) = rest.split_once(' ').ok_or_else(bad)?;
        let length: usize = length.parse().map_err(|_| bad())?;
        let record = rest.get(..length).ok_or_else(bad)?;
        let body = record.strip_suffix('\n').ok_or_else(bad)?;
        let (_, pair) = body.split_once(' ').ok_or_else(bad)?;
        let (key, value) = pair.split_once('=').ok_or_else(bad)?;
        if key != "path" {
            return Err(ImportError::invalid(format!(
                "an extended tar header sets {key}, which the archive format never uses"
            )));
        }
        if path.replace(value.to_string()).is_some() {
            return Err(bad());
        }
        rest = &rest[length..];
    }
    path.ok_or_else(bad)
}

/// Refuses a path that is empty, absolute, too long, climbs out of the archive, or holds a
/// character the writer never emits.
fn check_path(path: &str) -> Result<(), ImportError> {
    let refuse = |why: &str| ImportError::invalid(format!("the path {path:?} {why}"));
    if path.is_empty() {
        return Err(refuse("is empty"));
    }
    if path.len() > PATH_LIMIT_BYTES {
        return Err(refuse("is too long"));
    }
    if path.starts_with('/') {
        return Err(refuse("is absolute"));
    }
    if path
        .chars()
        .any(|c| c.is_control() || c == '\\' || c == ':')
    {
        return Err(refuse("holds a control character, a backslash or a colon"));
    }
    for part in path.split('/') {
        match part {
            "" => return Err(refuse("holds an empty component")),
            "." | ".." => return Err(refuse("climbs out of the archive")),
            _ => {}
        }
    }
    Ok(())
}

/// Reads the `size` bytes of an entry's data and the zero bytes that pad them to a block.
fn read_data(input: &mut impl Read, size: u64) -> Result<Vec<u8>, ImportError> {
    let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    input
        .by_ref()
        .take(size)
        .read_to_end(&mut data)
        .map_err(read_error)?;
    if data.len() as u64 != size {
        return Err(ImportError::invalid(
            "the archive ends before its last entry does",
        ));
    }
    skip_padding(input, size)?;
    Ok(data)
}

/// Reads the zero bytes that pad `size` bytes of data to a block.
fn skip_padding(input: &mut impl Read, size: u64) -> Result<(), ImportError> {
    let mut pad = [0u8; BLOCK];
    input
        .read_exact(&mut pad[..padding(size)])
        .map_err(read_error)
}

/// Copies the `size` bytes of an entry's data to the file `path` in chunks, and reads the padding.
fn spool_data(input: &mut impl Read, size: u64, path: PathBuf) -> Result<PathBuf, ImportError> {
    let storage = |error: io::Error| ImportError::Storage(error.to_string());
    let mut file = File::create(&path).map_err(storage)?;
    let mut data = input.by_ref().take(size);
    let mut chunk = vec![0u8; SPOOL_CHUNK_BYTES];
    let mut copied = 0u64;
    loop {
        let read = data.read(&mut chunk).map_err(read_error)?;
        if read == 0 {
            break;
        }
        file.write_all(&chunk[..read]).map_err(storage)?;
        copied += read as u64;
    }
    if copied != size {
        return Err(ImportError::invalid(
            "the archive ends before its last entry does",
        ));
    }
    skip_padding(input, size)?;
    Ok(path)
}

/// Refuses a stream that holds anything but zero bytes after its end marker.
fn expect_end(input: &mut impl Read) -> Result<(), ImportError> {
    let mut buf = [0u8; BLOCK];
    loop {
        match input.read(&mut buf).map_err(read_error)? {
            0 => return Ok(()),
            read if buf[..read].iter().any(|byte| *byte != 0) => {
                return Err(ImportError::invalid(
                    "the archive holds data after its end marker",
                ));
            }
            _ => {}
        }
    }
}

/// Reads every regular file of the tar stream `reader`, whose paths `cap_of` accepts.
///
/// `cap_of` returns the [`Cap`] of the file at a path, or [`None`] for a path the format has no
/// file at. A spooled file is copied to a file of `spool` in chunks, so it is never held whole.
///
/// # Errors
///
/// Returns [`ImportError`] when the stream unpacks past `limits`, holds more files than they
/// allow, a file past its cap, a path outside the format, a duplicate path, an entry that is not a
/// regular file (a directory, a link or a device), a header that fails its checksum, or anything
/// after its end marker.
pub(super) fn read_files(
    reader: impl Read,
    limits: ReadLimits,
    spool: &Spool,
    cap_of: impl Fn(&str) -> Option<Cap>,
) -> Result<Vec<TarFile>, ImportError> {
    let mut input = Capped {
        inner: reader,
        left: limits.unpacked_bytes,
        limit: limits.unpacked_bytes,
    };
    let mut files: Vec<TarFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut long_path: Option<String> = None;
    loop {
        let mut block = [0u8; BLOCK];
        input.read_exact(&mut block).map_err(read_error)?;
        if block.iter().all(|byte| *byte == 0) {
            break;
        }
        let entry = Entry::parse(&block)?;
        match entry.kind {
            b'x' => {
                if long_path.is_some() {
                    return Err(ImportError::invalid("two extended tar headers in a row"));
                }
                if entry.size > PAX_LIMIT_BYTES {
                    return Err(ImportError::invalid(format!(
                        "an extended tar header is {} bytes, over the {PAX_LIMIT_BYTES} byte limit",
                        entry.size
                    )));
                }
                let data = read_data(&mut input, entry.size)?;
                long_path = Some(pax_path(&data)?);
            }
            b'0' | 0 => {
                let path = long_path.take().unwrap_or(entry.path);
                check_path(&path)?;
                let cap = cap_of(&path).ok_or_else(|| {
                    ImportError::invalid(format!("{path} is not part of a board archive"))
                })?;
                if entry.size > cap.bytes {
                    return Err(ImportError::TooLarge(format!(
                        "{path} is {} bytes, over its limit of {} bytes",
                        entry.size, cap.bytes
                    )));
                }
                if files.len() >= limits.files {
                    return Err(ImportError::TooLarge(format!(
                        "the archive holds more than {} files",
                        limits.files
                    )));
                }
                if !seen.insert(path.clone()) {
                    return Err(ImportError::invalid(format!(
                        "{path} appears twice in the archive"
                    )));
                }
                let data = if cap.spooled {
                    Data::Spooled(spool_data(&mut input, entry.size, spool.file(files.len()))?)
                } else {
                    Data::Memory(read_data(&mut input, entry.size)?)
                };
                files.push(TarFile { path, data });
            }
            other => {
                return Err(ImportError::invalid(format!(
                    "{} is not a regular file (tar entry type {:?})",
                    entry.path,
                    char::from(other)
                )));
            }
        }
    }
    if long_path.is_some() {
        return Err(ImportError::invalid("an extended tar header names no file"));
    }
    expect_end(&mut input)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::{
        BLOCK, Cap, Data, ImportError, ReadLimits, Spool, ZEROS, entry_header, header, padding,
        pax_record, read_files,
    };

    #[test]
    fn padding_should_round_up_to_a_whole_block() {
        assert_eq!(padding(0), 0);
        assert_eq!(padding(1), BLOCK - 1);
        assert_eq!(padding(BLOCK as u64), 0);
        assert_eq!(padding(BLOCK as u64 + 5), BLOCK - 5);
    }

    #[test]
    fn pax_record_should_count_its_own_length() {
        assert_eq!(pax_record("path", "a"), "9 path=a\n");
        let long = "x".repeat(100);
        let record = pax_record("path", &long);
        let (length, _) = record.split_once(' ').expect("length");
        assert_eq!(length.parse::<usize>().expect("number"), record.len());
    }

    #[test]
    fn entry_header_should_write_one_block_for_a_short_path() {
        let bytes = entry_header("board.json", 1234, 1_700_000_000).expect("header");
        assert_eq!(bytes.len(), BLOCK);
        assert_eq!(&bytes[..10], b"board.json");
        assert_eq!(&bytes[257..262], b"ustar");
        assert_eq!(&bytes[124..135], b"00000002322");
        assert_eq!(bytes[156], b'0');
        let mut zeroed = bytes.clone();
        zeroed[148..156].fill(b' ');
        let sum: u32 = zeroed.iter().map(|byte| u32::from(*byte)).sum();
        let stored = std::str::from_utf8(&bytes[148..154]).expect("utf8");
        assert_eq!(u32::from_str_radix(stored, 8).expect("octal"), sum);
    }

    #[test]
    fn entry_header_should_carry_a_long_path_in_a_pax_header() {
        let path = format!("attachments/{}/{}", "i".repeat(36), "n".repeat(200));
        let bytes = entry_header(&path, 7, 0).expect("header");
        assert_eq!(bytes[156], b'x');
        let record = pax_record("path", &path);
        assert_eq!(&bytes[BLOCK..BLOCK + record.len()], record.as_bytes());
        let plain = BLOCK + record.len() + padding(record.len() as u64);
        assert_eq!(bytes.len(), plain + BLOCK);
        assert_eq!(bytes[plain + 156], b'0');
        assert_eq!(
            &bytes[BLOCK + record.len()..plain],
            &ZEROS[..plain - BLOCK - record.len()]
        );
    }

    #[test]
    fn entry_header_should_refuse_a_size_the_header_cannot_hold() {
        assert!(entry_header("big", 1 << 33, 0).is_err());
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, data) in entries {
            out.extend(entry_header(path, data.len() as u64, 0).expect("header"));
            out.extend_from_slice(data);
            out.extend_from_slice(&ZEROS[..padding(data.len() as u64)]);
        }
        out.extend_from_slice(&ZEROS);
        out
    }

    const LIMITS: ReadLimits = ReadLimits {
        unpacked_bytes: 1 << 20,
        files: 8,
    };

    fn cap(bytes: u64, spooled: bool) -> Option<Cap> {
        Some(Cap { bytes, spooled })
    }

    fn anything(_: &str) -> Option<Cap> {
        cap(1 << 16, false)
    }

    fn refusal(tar: &[u8], cap_of: impl Fn(&str) -> Option<Cap>) -> ImportError {
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        read_files(tar, LIMITS, &spool, cap_of).expect_err("a refusal")
    }

    #[test]
    fn read_files_should_read_what_the_writer_wrote() {
        let long = format!("attachments/{}/{}", "i".repeat(36), "n".repeat(200));
        let tar = tar_of(&[("board.json", b"{}"), (&long, &[7; 600]), ("empty", b"")]);
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let spooled = |path: &str| cap(1 << 16, path.starts_with("attachments/"));
        let files = read_files(tar.as_slice(), LIMITS, &spool, spooled).expect("files");
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["board.json", long.as_str(), "empty"]);
        assert!(matches!(&files[0].data, Data::Memory(bytes) if bytes == b"{}"));
        let Data::Spooled(path) = &files[1].data else {
            panic!("an attachment is spooled");
        };
        assert_eq!(path.parent(), spool.file(0).parent());
        assert_eq!(std::fs::read(path).expect("spooled file"), [7; 600]);
        assert!(matches!(&files[2].data, Data::Memory(bytes) if bytes.is_empty()));
    }

    #[test]
    fn read_files_should_spool_a_large_file_in_chunks_and_refuse_a_short_one() {
        let big = vec![9u8; 3 * super::SPOOL_CHUNK_BYTES + 5];
        let tar = tar_of(&[("attachments/a/big", &big)]);
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let limits = ReadLimits {
            unpacked_bytes: 1 << 22,
            files: 8,
        };
        let files =
            read_files(tar.as_slice(), limits, &spool, |_| cap(1 << 22, true)).expect("files");
        let Data::Spooled(path) = &files[0].data else {
            panic!("spooled");
        };
        assert_eq!(std::fs::read(path).expect("spooled file"), big);
        let cut = &tar[..BLOCK + 1000];
        let error = read_files(cut, limits, &spool, |_| cap(1 << 22, true)).expect_err("short");
        assert!(matches!(error, ImportError::Invalid(_)), "{error}");
        drop(spool);
        assert_eq!(std::fs::read_dir(base.path()).expect("dir").count(), 0);
    }

    #[test]
    fn read_files_should_refuse_a_path_that_leaves_the_archive() {
        for path in [
            "../board.json",
            "a/../../b",
            "/etc/passwd",
            "a//b",
            "a/./b",
            "a\\b",
            "c:b",
            "a/",
        ] {
            let error = refusal(&tar_of(&[(path, b"x")]), anything);
            assert!(matches!(error, ImportError::Invalid(_)), "{path}: {error}");
        }
    }

    #[test]
    fn read_files_should_refuse_an_entry_that_is_not_a_regular_file() {
        for kind in *b"123456" {
            let mut tar = header("thing", 0, 0, kind).expect("header").to_vec();
            tar.extend_from_slice(&ZEROS);
            let error = refusal(&tar, anything);
            assert!(error.to_string().contains("not a regular file"), "{error}");
        }
    }

    #[test]
    fn read_files_should_refuse_a_path_twice_and_a_path_the_format_lacks() {
        let twice = refusal(&tar_of(&[("a", b"1"), ("a", b"2")]), anything);
        assert!(twice.to_string().contains("twice"), "{twice}");
        let lacking = refusal(&tar_of(&[("a", b"1")]), |_| None);
        assert!(lacking.to_string().contains("not part of"), "{lacking}");
    }

    #[test]
    fn read_files_should_refuse_a_file_past_its_cap_and_too_many_files() {
        let big = refusal(&tar_of(&[("a", &[0; 100])]), |_| cap(99, false));
        assert!(matches!(big, ImportError::TooLarge(_)), "{big}");
        let names: Vec<String> = (0..9).map(|n| format!("f{n}")).collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let many = refusal(&tar_of(&entries), anything);
        assert!(matches!(many, ImportError::TooLarge(_)), "{many}");
    }

    #[test]
    fn read_files_should_stop_a_stream_that_unpacks_past_its_limit() {
        let tar = tar_of(&[("a", &[0; 4096])]);
        let limits = ReadLimits {
            unpacked_bytes: 2048,
            files: 8,
        };
        let base = tempfile::tempdir().expect("tempdir");
        let spool = Spool::create(base.path()).expect("spool");
        let error = read_files(tar.as_slice(), limits, &spool, anything).expect_err("too large");
        assert!(matches!(error, ImportError::TooLarge(_)), "{error}");
    }

    #[test]
    fn read_files_should_refuse_a_damaged_stream() {
        let tar = tar_of(&[("a", &[1; 700])]);
        let mut damaged = tar.clone();
        damaged[10] ^= 1;
        assert!(matches!(
            refusal(&damaged, anything),
            ImportError::Invalid(_)
        ));
        let cut = &tar[..BLOCK + 100];
        assert!(matches!(refusal(cut, anything), ImportError::Invalid(_)));
        let mut trailing = tar;
        trailing.push(1);
        let error = refusal(&trailing, anything);
        assert!(error.to_string().contains("after its end"), "{error}");
        let empty = refusal(&[], anything);
        assert!(matches!(empty, ImportError::Invalid(_)));
    }
}
