use super::ArchiveError;

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

#[cfg(test)]
mod tests {
    use super::{BLOCK, ZEROS, entry_header, padding, pax_record};

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
}
