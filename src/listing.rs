// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archive listings: a SHA-256 checksum list with antiq's metadata in
//! comment lines, zstd-compressed.

use crate::archive::hashing::HashingWriter;
use crate::archive::zstd::decoder;
use crate::error::{ErrorKind, HasKind};
use crate::relpath::RelPath;
use ::zstd::stream::read::Decoder;
use ::zstd::stream::write::Encoder;
use jiff::Timestamp;
use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read, Write};

/// The format of the listings this version reads and writes.
pub const FORMAT: u32 = 1;

/// Listings compress at level 3 with the content checksum. They are small
/// next to the archive they describe, and writing one takes no worker of
/// the compression pool.
pub const LISTING_LEVEL: i32 = 3;

/// The type of a listed entry. Special files are not archived, so they
/// have no type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingType {
    File,
    Dir,
    Symlink,
    Hardlink,
}

/// What a link points to: any bytes. A symlink target is not a path inside
/// the piece, so it is not a [`RelPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget(Vec<u8>);

impl LinkTarget {
    pub fn from_bytes(bytes: Vec<u8>) -> LinkTarget {
        LinkTarget(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// One entry of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingEntry {
    /// Relative to the archive's top-level directory.
    pub path: RelPath,
    pub kind: ListingType,
    /// Bytes of a file; 0 for every other type.
    pub size: u64,
    /// Modification time in whole seconds since the epoch.
    pub mtime: i64,
    /// Permission bits.
    pub mode: u32,
    /// The target, for symlinks and hard links.
    pub link: Option<LinkTarget>,
    /// SHA-256 of the content as lower-case hex, for files and for hard
    /// links (the hash of the file they link to).
    pub sha256: Option<String>,
}

/// The head of a listing as a writer gives it. `files` and `bytes` are
/// derived from the entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub piece: RelPath,
    pub archived: Timestamp,
    pub archive: String,
    pub archive_bytes: u64,
    pub imported: Option<Timestamp>,
    pub rebuilt: Option<Timestamp>,
}

/// The head of a listing as far as it could be read: each field is present
/// when its line was found and parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Head {
    pub piece: Option<RelPath>,
    pub archived: Option<Timestamp>,
    pub archive: Option<String>,
    pub archive_bytes: Option<u64>,
    pub files: Option<u64>,
    pub bytes: Option<u64>,
    pub imported: Option<Timestamp>,
    pub rebuilt: Option<Timestamp>,
    /// The line holding only `#` was read.
    pub closed: bool,
}

impl Head {
    /// What is missing from the head, or `None` when `piece`, `archived`
    /// and `archive-bytes` are all there.
    pub fn damage(&self) -> Option<String> {
        let missing = [
            ("piece", self.piece.is_none()),
            ("archived", self.archived.is_none()),
            ("archive-bytes", self.archive_bytes.is_none()),
        ]
        .into_iter()
        .find_map(|(key, missing)| missing.then_some(key))?;
        Some(format!("the `{missing}` line is missing or unparsable"))
    }
}

/// A listing read to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub header: Header,
    pub files: u64,
    pub bytes: u64,
    pub entries: Vec<ListingEntry>,
}

/// What a written listing came to: the figures of the compressed bytes,
/// which are the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub size: u64,
    /// SHA-256 as lower-case hex.
    pub sha256: String,
}

/// An entry line found in a decoded listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedPath {
    pub kind: ListingType,
    /// The unescaped path; a directory has no trailing slash.
    pub path: Vec<u8>,
}

/// Count and total size of the stored regular files: a hard link counts
/// for neither.
///
/// `None` when the sizes add up beyond 64 bits.
pub fn counts(entries: &[ListingEntry]) -> Option<(u64, u64)> {
    entries
        .iter()
        .filter(|entry| entry.kind == ListingType::File)
        .try_fold((0u64, 0u64), |(files, bytes), entry| {
            Some((files.checked_add(1)?, bytes.checked_add(entry.size)?))
        })
}

// =========================================================
// Layout
// =========================================================

// An entry line is `# `, then four fields of fixed width separated by two
// spaces, then two spaces and the path: type 4, mode 4, time 20, size 16.
// The path therefore starts at byte 54 of the line.
const TYPE_AT: usize = 2;
const MODE_AT: usize = 8;
const TIME_AT: usize = 14;
const SIZE_AT: usize = 36;
const PATH_AT: usize = 54;

// A header line is `# `, the key padded to 14 characters, the value.
const KEY_WIDTH: usize = 14;
const VALUE_AT: usize = 2 + KEY_WIDTH;

const REQUIRED_KEYS: [&str; 6] = [
    "piece",
    "archived",
    "archive",
    "archive-bytes",
    "files",
    "bytes",
];

const OPTIONAL_KEYS: [&str; 2] = ["imported", "rebuilt"];

/// The seconds of 0001-01-01T00:00:00Z and of 9999-12-31T23:59:59Z, the
/// range the 20 characters of the time field can hold.
const MIN_SECOND: i64 = -62_135_596_800;
const MAX_SECOND: i64 = 253_402_300_799;

const SECONDS_PER_DAY: i64 = 86_400;

// =========================================================
// Times
// =========================================================

// The civil calendar conversions are Howard Hinnant's days-from-civil
// algorithms. Going through them instead of `jiff::Timestamp` keeps the
// whole range of the field available: a `Timestamp` stops about a day
// before the end of year 9999.

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for `second`, or `None` outside years 1 to 9999.
fn format_time(second: i64) -> Option<String> {
    if !(MIN_SECOND..=MAX_SECOND).contains(&second) {
        return None;
    }
    let (year, month, day) = civil_from_days(second.div_euclid(SECONDS_PER_DAY));
    let in_day = second.rem_euclid(SECONDS_PER_DAY);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    ))
}

/// The seconds a time field names. Only the canonical spelling counts, so
/// a day that does not exist or an hour 24 is refused.
fn parse_time(text: &[u8]) -> Option<i64> {
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let separators = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'Z'),
    ];
    if text.len() != 20 || separators.iter().any(|(at, byte)| text[*at] != *byte) {
        return None;
    }
    let second = days_from_civil(digits(0..4)?, digits(5..7)?, digits(8..10)?) * SECONDS_PER_DAY
        + digits(11..13)? * 3600
        + digits(14..16)? * 60
        + digits(17..19)?;
    (format_time(second)?.as_bytes() == text).then_some(second)
}

fn timestamp(second: i64) -> Option<Timestamp> {
    Timestamp::from_second(second).ok()
}

// =========================================================
// Escapes
// =========================================================

// Comment lines write paths and link targets with `\\`, `\n`, `\t` and
// `\r`, and never start with a backslash, since a line that does is no
// longer skipped as a comment. Checksum lines are the standard format of
// `shasum` and GNU `sha256sum`: a path holding a newline or a backslash
// gets a leading backslash on the line and writes those two as `\n` and
// `\\`. A carriage return and a tab stay raw there.

fn escape_comment(bytes: &[u8], out: &mut Vec<u8>) {
    for &byte in bytes {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            other => out.push(other),
        }
    }
}

/// A path as comment lines spell it.
pub fn escape_path(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    escape_comment(path, &mut out);
    out
}

fn unescape(bytes: &[u8], allowed: &[(u8, u8)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes.iter();
    while let Some(&byte) = rest.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&code) = rest.next() else {
            return Err("a backslash ends the path".into());
        };
        match allowed.iter().find(|(letter, _)| *letter == code) {
            Some((_, meaning)) => out.push(*meaning),
            None => return Err(format!("`\\{}` is not an escape", char::from(code))),
        }
    }
    Ok(out)
}

fn unescape_comment(bytes: &[u8]) -> Result<Vec<u8>, String> {
    unescape(
        bytes,
        &[(b'\\', b'\\'), (b'n', b'\n'), (b't', b'\t'), (b'r', b'\r')],
    )
}

// =========================================================
// Writing
// =========================================================

fn io_error(source: io::Error) -> ListingError {
    let limit = source
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<LimitExceeded>())
        .map(|exceeded| exceeded.limit);
    match limit {
        Some(limit) => ListingError::TooLarge { limit },
        None => ListingError::Io { source },
    }
}

/// Whether `text` is a SHA-256 in lower-case hex.
pub(crate) fn is_sha256_hex(text: &[u8]) -> bool {
    text.len() == 64 && text.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The widest size the 16 characters of the size field hold.
const MAX_SIZE: u64 = 9_999_999_999_999_999;

/// The rules that tie the fields of an entry to its type, which the entry
/// type itself leaves open. A reader that accepted a symlink without a
/// target or a directory with a checksum would hand the comparison with
/// the tree an entry it cannot make sense of. The hash is checked apart,
/// since a reader gets it from the line after the entry.
fn check_fields(entry: &ListingEntry) -> Result<(), String> {
    if entry.path.is_root() {
        return Err("the top-level directory has no line of its own".into());
    }
    let is_link = matches!(entry.kind, ListingType::Symlink | ListingType::Hardlink);
    if is_link != entry.link.is_some() {
        return Err("`link` belongs to symlinks and hard links, and only to them".into());
    }
    if entry.kind != ListingType::File && entry.size != 0 {
        return Err("only a file has a size".into());
    }
    if entry.size > MAX_SIZE {
        return Err("the size has more than 16 digits".into());
    }
    if entry.mode > 0o7777 {
        return Err("the mode has bits above the permission bits".into());
    }
    if format_time(entry.mtime).is_none() {
        return Err("the time cannot be written in years 1 to 9999".into());
    }
    Ok(())
}

fn check_hash(entry: &ListingEntry) -> Result<(), String> {
    let has_hash = matches!(entry.kind, ListingType::File | ListingType::Hardlink);
    match &entry.sha256 {
        Some(digest) if has_hash && is_sha256_hex(digest.as_bytes()) => Ok(()),
        Some(_) if has_hash => Err("`sha256` is not a hash in lower-case hex of 64 digits".into()),
        None if has_hash => Err("a file and a hard link need a `sha256` hash".into()),
        Some(_) => Err("`sha256` belongs to files and hard links, and only to them".into()),
        None => Ok(()),
    }
}

fn check_entry(entry: &ListingEntry) -> Result<(), String> {
    check_fields(entry)?;
    check_hash(entry)
}

fn header_error(reason: &str) -> ListingError {
    ListingError::Header {
        reason: reason.into(),
    }
}

fn header_line(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    out.extend_from_slice(b"# ");
    out.extend_from_slice(format!("{key:<KEY_WIDTH$}").as_bytes());
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn header_time(second: Timestamp) -> Result<String, ListingError> {
    format_time(second.as_second()).ok_or_else(|| header_error("a time is outside years 1 to 9999"))
}

fn head_bytes(header: &Header, files: u64, bytes: u64) -> Result<Vec<u8>, ListingError> {
    if header.archive.contains('\n') {
        return Err(header_error("the archive name holds a line break"));
    }
    let mut out = format!("# antiq listing {FORMAT}\n").into_bytes();
    header_line(&mut out, "piece", &escape_path(header.piece.as_bytes()));
    header_line(
        &mut out,
        "archived",
        header_time(header.archived)?.as_bytes(),
    );
    header_line(&mut out, "archive", header.archive.as_bytes());
    header_line(
        &mut out,
        "archive-bytes",
        header.archive_bytes.to_string().as_bytes(),
    );
    header_line(&mut out, "files", files.to_string().as_bytes());
    header_line(&mut out, "bytes", bytes.to_string().as_bytes());
    if let Some(time) = header.imported {
        header_line(&mut out, "imported", header_time(time)?.as_bytes());
    }
    if let Some(time) = header.rebuilt {
        header_line(&mut out, "rebuilt", header_time(time)?.as_bytes());
    }
    out.extend_from_slice(b"#\n");
    Ok(out)
}

/// The entry line and, for a file or hard link, the checksum line after it.
fn entry_bytes(entry: &ListingEntry) -> Vec<u8> {
    let name = match entry.kind {
        ListingType::File => "file",
        ListingType::Dir => "dir ",
        ListingType::Symlink => "link",
        ListingType::Hardlink => "hard",
    };
    let size = match entry.kind {
        ListingType::File => entry.size.to_string(),
        _ => "-".to_string(),
    };
    let time = format_time(entry.mtime).expect("check_entry accepted the time");
    let mut out = format!("# {name}  {:04o}  {time}  {size:>16}  ", entry.mode).into_bytes();
    debug_assert_eq!(out.len(), PATH_AT);
    escape_comment(entry.path.as_bytes(), &mut out);
    match (entry.kind, &entry.link) {
        (ListingType::Dir, _) => out.push(b'/'),
        (ListingType::Symlink, Some(target)) => {
            out.extend_from_slice(b"\t-> ");
            escape_comment(target.as_bytes(), &mut out);
        }
        (ListingType::Hardlink, Some(target)) => {
            out.extend_from_slice(b"\t=> ");
            escape_comment(target.as_bytes(), &mut out);
        }
        _ => {}
    }
    out.push(b'\n');
    if let Some(digest) = &entry.sha256 {
        let path = entry.path.as_bytes();
        let escaped = path.iter().any(|byte| matches!(byte, b'\n' | b'\\'));
        if escaped {
            out.push(b'\\');
        }
        out.extend_from_slice(digest.as_bytes());
        out.extend_from_slice(b"  ");
        for &byte in path {
            match byte {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                other => out.push(other),
            }
        }
        out.push(b'\n');
    }
    out
}

/// Writes a listing, compressed.
///
/// `out` is flushed but not synced; making a file durable is up to the
/// caller, who holds the file. `files` and `bytes` of the head come from
/// the entries.
pub fn write<W: Write>(
    header: &Header,
    entries: &[ListingEntry],
    out: W,
) -> Result<Written, ListingError> {
    let (files, bytes) = counts(entries)
        .ok_or_else(|| header_error("the sizes of the files add up beyond 64 bits"))?;
    let head = head_bytes(header, files, bytes)?;
    let mut encoder = Encoder::new(HashingWriter::new(out), LISTING_LEVEL).map_err(io_error)?;
    encoder.include_checksum(true).map_err(io_error)?;
    encoder.write_all(&head).map_err(io_error)?;
    for entry in entries {
        check_entry(entry).map_err(|reason| ListingError::Entry {
            path: entry.path.to_string(),
            reason,
        })?;
        encoder.write_all(&entry_bytes(entry)).map_err(io_error)?;
    }
    // The encoder's `finish` ends the frame but does not flush what it
    // wrote into the sink, so a buffering sink would keep the last bytes
    // that the figures below already count.
    let mut sink = encoder.finish().map_err(io_error)?;
    sink.flush().map_err(io_error)?;
    Ok(Written {
        size: sink.written(),
        sha256: sink.sha256(),
    })
}

// =========================================================
// Reading
// =========================================================

/// The most decoded bytes any reader of antiq takes from one listing.
/// It bounds the decoded text only: the entries a full read
/// builds from it take a multiple of that in memory.
///
/// A listing can be copied back from the target by hand, so a damaged or
/// hostile one must not make antiq allocate without bound. The limit is
/// 2^23 files (8.4 million) at 512 bytes each. One real tree measured
/// 306 decoded bytes per file, checksum lines included (the 119,680 files
/// of `~/.cargo/registry/src`, whose listing decodes to 36,668,911 bytes);
/// 512 is that figure with room for longer paths. The product, 4 GiB, is
/// about 117 times that listing. The length of a single entry is not limited
/// on its own: it counts here.
const MAX_DECODED_BYTES: u64 = 512 << 23;

/// The marker inside the `io::Error` of [`Bounded`], so that a listing that
/// is too large is told apart from a stream that fails.
#[derive(Debug)]
struct LimitExceeded {
    limit: u64,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "more than {} decoded bytes", self.limit)
    }
}

impl std::error::Error for LimitExceeded {}

/// The decoded stream of a listing, which fails once it holds more than
/// `limit` bytes. Every consumer reads through it, so the line reader and
/// `read_to_end` stop at the same point, also inside a line that has no end.
struct Bounded<R> {
    inner: R,
    limit: u64,
    left: u64,
}

impl<R: Read> Bounded<R> {
    fn new(inner: R, limit: u64) -> Bounded<R> {
        Bounded {
            inner,
            limit,
            left: limit,
        }
    }
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // One byte past what is left tells a stream that ends exactly at the
        // limit from one that goes on, and no read ever takes more.
        let room = usize::try_from(self.left.saturating_add(1)).unwrap_or(usize::MAX);
        let ask = buf.len().min(room);
        let read = self.inner.read(&mut buf[..ask])?;
        match self.left.checked_sub(read as u64) {
            Some(left) => {
                self.left = left;
                Ok(read)
            }
            None => Err(io::Error::other(LimitExceeded { limit: self.limit })),
        }
    }
}

/// The lines of a decoded listing, one at a time, without their newline.
struct Lines<R: Read> {
    inner: BufReader<Bounded<Decoder<'static, BufReader<R>>>>,
    number: usize,
}

impl<R: Read> Lines<R> {
    /// Opens the stream and checks the first line. The decoder refuses
    /// windows above the limit every reader of antiq has, which shows here
    /// or on a later read, depending on where the stream's first frame
    /// header ends.
    fn open(input: R, limit: u64) -> Result<Lines<R>, ListingError> {
        let mut lines = Lines {
            inner: BufReader::new(Bounded::new(decoder(input).map_err(io_error)?, limit)),
            number: 0,
        };
        let first = lines.next()?.ok_or(ListingError::InvalidHeader)?;
        check_first_line(&first)?;
        Ok(lines)
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, ListingError> {
        let mut line = Vec::new();
        let read = self.inner.read_until(b'\n', &mut line).map_err(io_error)?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        if line.pop() != Some(b'\n') {
            return Err(self.fail("the listing ends inside a line"));
        }
        Ok(Some(line))
    }

    fn fail(&self, reason: &str) -> ListingError {
        ListingError::Line {
            number: self.number,
            reason: reason.into(),
        }
    }

    /// The stream ended where the format wants more.
    fn ended(&self, reason: &str) -> ListingError {
        ListingError::End {
            reason: reason.into(),
        }
    }
}

/// `# antiq listing <number>`: 1 is this format, any other number above 1
/// (also one too large for 32 bits) a newer one.
fn check_first_line(line: &[u8]) -> Result<(), ListingError> {
    let digits = line
        .strip_prefix(b"# antiq listing ")
        .filter(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit))
        .filter(|digits| digits[0] != b'0')
        .ok_or(ListingError::InvalidHeader)?;
    let number = digits.iter().fold(0u64, |number, digit| {
        number
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'))
    });
    match u32::try_from(number).unwrap_or(u32::MAX) {
        FORMAT => Ok(()),
        found => Err(ListingError::FormatTooNew { found }),
    }
}

/// The key and value of a header line.
fn header_pair(line: &[u8]) -> Option<(String, &[u8])> {
    if line.len() < VALUE_AT || !line.starts_with(b"# ") {
        return None;
    }
    let key = std::str::from_utf8(&line[2..VALUE_AT])
        .ok()?
        .trim_end_matches(' ');
    (!key.is_empty()).then(|| (key.to_string(), &line[VALUE_AT..]))
}

/// A number as `to_string` writes it: digits, no leading zero.
fn parse_number(value: &[u8]) -> Option<u64> {
    if value.is_empty()
        || !value.iter().all(u8::is_ascii_digit)
        || (value.len() > 1 && value[0] == b'0')
    {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// Sets the field `key` names when `value` parses.
fn apply(head: &mut Head, key: &str, value: &[u8]) {
    let time = || parse_time(value).and_then(timestamp);
    match key {
        "piece" => {
            head.piece = unescape_comment(value)
                .ok()
                .and_then(|bytes| RelPath::from_bytes(bytes).ok());
        }
        "archived" => head.archived = time(),
        "archive" => head.archive = String::from_utf8(value.to_vec()).ok(),
        "archive-bytes" => head.archive_bytes = parse_number(value),
        "files" => head.files = parse_number(value),
        "bytes" => head.bytes = parse_number(value),
        "imported" => head.imported = time(),
        "rebuilt" => head.rebuilt = time(),
        _ => {}
    }
}

/// The head of a listing. Stops at the line holding only `#`, so the
/// stream is not read to its end and its content checksum is not checked.
///
/// A head that is cut short or lacks a line gives the fields that were
/// found; [`Head::damage`] says what is missing. Only a first line that is
/// not `# antiq listing <number>`, a newer format and an unreadable stream
/// are errors.
pub fn read_head<R: Read>(input: R) -> Result<Head, ListingError> {
    read_head_within(input, MAX_DECODED_BYTES)
}

fn read_head_within<R: Read>(input: R, limit: u64) -> Result<Head, ListingError> {
    let mut lines = Lines::open(input, limit)?;
    let mut head = Head::default();
    // A stream that fails inside the head leaves the fields found so far,
    // unless it fails by the size limit, which says why the head is cut.
    loop {
        let line = match lines.next() {
            Ok(Some(line)) => line,
            Err(error @ ListingError::TooLarge { .. }) => return Err(error),
            Ok(None) | Err(_) => break,
        };
        if line == b"#" {
            head.closed = true;
            break;
        }
        if let Some((key, value)) = header_pair(&line) {
            apply(&mut head, &key, value);
        }
    }
    Ok(head)
}

/// Reads the head of a listing in full: the six lines in order, the
/// optional two, the closing `#`.
fn read_strict_head<R: Read>(lines: &mut Lines<R>) -> Result<(Header, u64, u64), ListingError> {
    let mut head = Head::default();
    let mut next = lines.next()?;
    for expected in REQUIRED_KEYS {
        let Some(line) = next.as_deref() else {
            return Err(lines.ended(&format!("the head stops before the `{expected}` line")));
        };
        let found = header_pair(line);
        match found {
            Some((key, value)) if key == expected => {
                apply(&mut head, &key, value);
                let parsed = match expected {
                    "piece" => head.piece.is_some(),
                    "archived" => head.archived.is_some(),
                    "archive" => head.archive.is_some(),
                    "archive-bytes" => head.archive_bytes.is_some(),
                    "files" => head.files.is_some(),
                    _ => head.bytes.is_some(),
                };
                if !parsed {
                    return Err(lines.fail(&format!("the `{expected}` value does not parse")));
                }
            }
            _ => {
                return Err(lines.fail(&format!(
                    "the header lines are not in order: expected `{expected}`"
                )));
            }
        }
        next = lines.next()?;
    }
    for optional in OPTIONAL_KEYS {
        if let Some((key, value)) = next.as_deref().and_then(header_pair)
            && key == optional
        {
            apply(&mut head, &key, value);
            let parsed = if optional == "imported" {
                head.imported.is_some()
            } else {
                head.rebuilt.is_some()
            };
            if !parsed {
                return Err(lines.fail(&format!("the `{optional}` value does not parse")));
            }
            next = lines.next()?;
        }
    }
    match next.as_deref() {
        Some(b"#") => {}
        Some(_) => return Err(lines.fail("the closing `#` line of the head is missing")),
        None => return Err(lines.ended("the closing `#` line of the head is missing")),
    }
    let header = Header {
        piece: head.piece.expect("parsed above"),
        archived: head.archived.expect("parsed above"),
        archive: head.archive.expect("parsed above"),
        archive_bytes: head.archive_bytes.expect("parsed above"),
        imported: head.imported,
        rebuilt: head.rebuilt,
    };
    Ok((
        header,
        head.files.expect("parsed above"),
        head.bytes.expect("parsed above"),
    ))
}

/// An entry line, without the checksum that follows it, and whether such a
/// line has to follow.
fn parse_entry_line(line: &[u8]) -> Result<(ListingEntry, bool), String> {
    let gaps = [(6, 8), (12, 14), (34, 36), (52, 54)];
    if line.len() < PATH_AT
        || !line.starts_with(b"# ")
        || gaps.iter().any(|(from, to)| &line[*from..*to] != b"  ")
    {
        return Err("an entry line has four fixed-width fields before the path".into());
    }
    let kind = match &line[TYPE_AT..TYPE_AT + 4] {
        b"file" => ListingType::File,
        b"dir " => ListingType::Dir,
        b"link" => ListingType::Symlink,
        b"hard" => ListingType::Hardlink,
        other => return Err(format!("unknown type `{}`", String::from_utf8_lossy(other))),
    };
    let mode_digits = &line[MODE_AT..MODE_AT + 4];
    let mode = std::str::from_utf8(mode_digits)
        .ok()
        .filter(|digits| digits.bytes().all(|b| matches!(b, b'0'..=b'7')))
        .and_then(|digits| u32::from_str_radix(digits, 8).ok())
        .ok_or("the mode is not four octal digits")?;
    let mtime = parse_time(&line[TIME_AT..TIME_AT + 20]).ok_or("the time does not parse")?;
    // The size is right aligned, padded with spaces only.
    let size_field = &line[SIZE_AT..SIZE_AT + 16];
    let size_field = &size_field[size_field
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(size_field.len())..];
    let size = match (kind, size_field) {
        (ListingType::File, digits) => {
            parse_number(digits).ok_or("a file needs a size in digits")?
        }
        (_, b"-") => 0,
        _ => return Err("only a file has a size, the others show `-`".into()),
    };
    let rest = &line[PATH_AT..];
    let (raw_path, raw_target) = match kind {
        ListingType::Symlink | ListingType::Hardlink => {
            let marker: &[u8] = if kind == ListingType::Symlink {
                b"\t-> "
            } else {
                b"\t=> "
            };
            let at = rest
                .windows(marker.len())
                .position(|window| window == marker)
                .ok_or("a link line needs its target after a tab")?;
            (&rest[..at], Some(&rest[at + marker.len()..]))
        }
        _ => (rest, None),
    };
    if raw_path.contains(&b'\t') || raw_target.is_some_and(|target| target.contains(&b'\t')) {
        return Err("a tab in a path or target must be written `\\t`".into());
    }
    let mut path = unescape_comment(raw_path)?;
    if kind == ListingType::Dir && path.pop() != Some(b'/') {
        return Err("the path of a directory ends in `/`".into());
    }
    let path = RelPath::from_bytes(path).map_err(|error| error.to_string())?;
    let link = raw_target
        .map(|target| unescape_comment(target).map(LinkTarget::from_bytes))
        .transpose()?;
    let entry = ListingEntry {
        path,
        kind,
        size,
        mtime,
        mode,
        link,
        sha256: None,
    };
    check_fields(&entry)?;
    Ok((
        entry,
        matches!(kind, ListingType::File | ListingType::Hardlink),
    ))
}

/// The hash and the unescaped path of a checksum line.
fn parse_checksum_line(line: &[u8]) -> Result<(String, Vec<u8>), String> {
    let (escaped, rest) = match line.strip_prefix(b"\\") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    if rest.len() < 66 || !is_sha256_hex(&rest[..64]) || &rest[64..66] != b"  " {
        return Err(
            "the checksum line does not start with a SHA-256 hash in lower-case hex and two spaces"
                .into(),
        );
    }
    let hash = String::from_utf8(rest[..64].to_vec()).expect("hex digits are ASCII");
    let path = if escaped {
        unescape(&rest[66..], &[(b'\\', b'\\'), (b'n', b'\n')])?
    } else {
        rest[66..].to_vec()
    };
    Ok((hash, path))
}

/// All of a listing. The stream is read to its end, so its content
/// checksum is checked.
///
/// Anything that is not the format is an error, a newer format apart:
/// the header lines in order, every entry in the layout with its checksum
/// line right after it, no path twice, and `files` and `bytes` equal to
/// what the entries hold.
pub fn read_full<R: Read>(input: R) -> Result<Listing, ListingError> {
    read_full_within(input, MAX_DECODED_BYTES)
}

fn read_full_within<R: Read>(input: R, limit: u64) -> Result<Listing, ListingError> {
    let mut lines = Lines::open(input, limit)?;
    let (header, files, bytes) = read_strict_head(&mut lines)?;
    let mut entries: Vec<ListingEntry> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    while let Some(line) = lines.next()? {
        if !line.starts_with(b"#") {
            return Err(lines.fail("a checksum line has no file or hard line before it"));
        }
        let (mut entry, needs_checksum) =
            parse_entry_line(&line).map_err(|reason| lines.fail(&reason))?;
        if !seen.insert(entry.path.as_bytes().to_vec()) {
            return Err(lines.fail("the path occurs twice"));
        }
        if needs_checksum {
            let checksum = match lines.next()? {
                Some(checksum) if !checksum.starts_with(b"#") => checksum,
                Some(_) => return Err(lines.fail("the entry has no checksum line after it")),
                None => return Err(lines.ended("the last entry has no checksum line after it")),
            };
            let (hash, path) =
                parse_checksum_line(&checksum).map_err(|reason| lines.fail(&reason))?;
            if path != entry.path.as_bytes() {
                return Err(lines.fail("the checksum line names another path than the entry"));
            }
            entry.sha256 = Some(hash);
        }
        entries.push(entry);
    }
    let (found_files, found_bytes) = counts(&entries).ok_or_else(|| ListingError::End {
        reason: "the sizes of the files add up beyond 64 bits".into(),
    })?;
    if found_files != files {
        return Err(ListingError::End {
            reason: format!("the `files` line says {files}, the entries hold {found_files}"),
        });
    }
    if found_bytes != bytes {
        return Err(ListingError::End {
            reason: format!("the `bytes` line says {bytes}, the entries hold {found_bytes}"),
        });
    }
    Ok(Listing {
        header,
        files,
        bytes,
        entries,
    })
}

/// The decoded bytes of a whole listing, for a search that works on the
/// text. The first line is checked, and the stream is read to its end.
pub fn decode_all<R: Read>(input: R) -> Result<Vec<u8>, ListingError> {
    decode_all_within(input, MAX_DECODED_BYTES)
}

fn decode_all_within<R: Read>(input: R, limit: u64) -> Result<Vec<u8>, ListingError> {
    let mut reader = BufReader::new(Bounded::new(decoder(input).map_err(io_error)?, limit));
    // The first line is checked before the rest is held, so a stream that
    // is no listing is not decoded up to the limit first.
    let mut decoded = Vec::new();
    reader.read_until(b'\n', &mut decoded).map_err(io_error)?;
    check_first_line(decoded.strip_suffix(b"\n").unwrap_or(&decoded))?;
    reader.read_to_end(&mut decoded).map_err(io_error)?;
    Ok(decoded)
}

/// The entry lines of a decoded listing, without the checks on hashes.
pub fn scan_paths(decoded: &[u8]) -> impl Iterator<Item = Result<ScannedPath, ListingError>> + '_ {
    // A head that never closes has no entries to scan, so its error is the
    // only item.
    let unclosed = decoded
        .split(|byte| *byte == b'\n')
        .all(|line| line != b"#")
        .then(|| {
            Err(ListingError::End {
                reason: "the closing `#` line of the head is missing".into(),
            })
        });
    let mut in_head = true;
    unclosed
        .into_iter()
        .chain(
            decoded
                .split(|byte| *byte == b'\n')
                .enumerate()
                .filter_map(move |(index, line)| {
                    if in_head {
                        in_head = line != b"#";
                        return None;
                    }
                    if !line.starts_with(b"#") {
                        return None;
                    }
                    Some(
                        parse_entry_line(line)
                            .map(|(entry, _)| ScannedPath {
                                kind: entry.kind,
                                path: entry.path.as_bytes().to_vec(),
                            })
                            .map_err(|reason| ListingError::Line {
                                number: index + 1,
                                reason,
                            }),
                    )
                }),
        )
}

/// Errors of archive listings.
#[derive(Debug, thiserror::Error)]
pub enum ListingError {
    #[error("cannot write or read the listing: {source}")]
    Io { source: io::Error },
    #[error("the listing does not start with a line `# antiq listing <number>`")]
    InvalidHeader,
    #[error("the listing has format {found}, which this version of antiq cannot read")]
    FormatTooNew { found: u32 },
    #[error("line {number} of the listing is not valid: {reason}")]
    Line { number: usize, reason: String },
    #[error("the listing is not valid at its end: {reason}")]
    End { reason: String },
    #[error("the listing decodes to more than {limit} bytes, the most antiq reads")]
    TooLarge { limit: u64 },
    #[error("the head of the listing cannot be written: {reason}")]
    Header { reason: String },
    #[error("the entry {path} cannot be listed: {reason}")]
    Entry { path: String, reason: String },
}

impl HasKind for ListingError {
    fn kind(&self) -> ErrorKind {
        match self {
            ListingError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
            _ => ErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn at(time: &str) -> Timestamp {
        time.parse().expect("a valid UTC time")
    }

    fn seconds(time: &str) -> i64 {
        at(time).as_second()
    }

    fn sha(content: &[u8]) -> String {
        crate::archive::hashing::hex(&Sha256::digest(content))
    }

    fn entry(kind: ListingType, name: &str) -> ListingEntry {
        entry_at(kind, path(name))
    }

    fn entry_at(kind: ListingType, name: RelPath) -> ListingEntry {
        ListingEntry {
            path: name,
            kind,
            size: 0,
            mtime: seconds("2026-10-30T17:44:02Z"),
            mode: 0o644,
            link: None,
            sha256: None,
        }
    }

    fn file(name: &str, content: &[u8]) -> ListingEntry {
        file_at(path(name), content)
    }

    fn file_at(name: RelPath, content: &[u8]) -> ListingEntry {
        ListingEntry {
            size: content.len() as u64,
            sha256: Some(sha(content)),
            ..entry_at(ListingType::File, name)
        }
    }

    fn hard(name: &str, target: &str, content: &[u8]) -> ListingEntry {
        ListingEntry {
            link: Some(LinkTarget::from_bytes(target.as_bytes().to_vec())),
            sha256: Some(sha(content)),
            ..entry(ListingType::Hardlink, name)
        }
    }

    fn symlink(name: &str, target: &[u8]) -> ListingEntry {
        ListingEntry {
            mode: 0o755,
            link: Some(LinkTarget::from_bytes(target.to_vec())),
            ..entry(ListingType::Symlink, name)
        }
    }

    fn dir(name: &str) -> ListingEntry {
        ListingEntry {
            mode: 0o755,
            ..entry(ListingType::Dir, name)
        }
    }

    fn header() -> Header {
        Header {
            piece: path("projects/scanner"),
            archived: at("2026-11-04T09:02:11Z"),
            archive: "projects-scanner--2026-11-04T090211.tar.zst".into(),
            archive_bytes: 1_093_744_120,
            imported: None,
            rebuilt: None,
        }
    }

    fn encode(header: &Header, entries: &[ListingEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        write(header, entries, &mut out).expect("the listing writes");
        out
    }

    fn decoded(listing: &[u8]) -> Vec<u8> {
        ::zstd::stream::decode_all(listing).expect("the listing decodes")
    }

    fn text(entries: &[ListingEntry]) -> Vec<u8> {
        decoded(&encode(&header(), entries))
    }

    /// Compresses decoded listing text the way `write` does, for readers
    /// that must see hand-made content.
    fn compressed(content: &[u8]) -> Vec<u8> {
        let mut encoder =
            ::zstd::stream::write::Encoder::new(Vec::new(), LISTING_LEVEL).expect("an encoder");
        encoder.include_checksum(true).expect("a flag");
        encoder.write_all(content).expect("a vec accepts bytes");
        encoder.finish().expect("the frame ends")
    }

    /// The valid text of a small listing, split into lines for editing.
    fn lines_of(entries: &[ListingEntry]) -> Vec<String> {
        String::from_utf8(text(entries))
            .expect("UTF-8 names")
            .split_inclusive('\n')
            .map(str::to_string)
            .collect()
    }

    fn small() -> Vec<ListingEntry> {
        vec![
            dir("src"),
            file("src/main.rs", b"fn main() {}"),
            symlink("src/latest", b"main.rs"),
        ]
    }

    // =========================================================
    // Format
    // =========================================================

    #[test]
    fn the_spec_example_is_reproduced_byte_for_byte_and_reads_back() {
        let example = crate::testing::package::spec_listing_example();
        let content = "x".repeat(4211);
        let main = |name: &str| ListingEntry {
            mtime: seconds("2026-10-30T17:44:02Z"),
            ..file(name, content.as_bytes())
        };
        let entries = [
            ListingEntry {
                mtime: seconds("2026-10-30T17:40:00Z"),
                ..dir("src")
            },
            ListingEntry {
                mtime: seconds("2026-10-30T17:41:00Z"),
                ..symlink("src/latest", b"main.rs")
            },
            main("src/main-copy.rs"),
            ListingEntry {
                link: Some(LinkTarget::from_bytes(b"src/main-copy.rs".to_vec())),
                sha256: Some(sha(content.as_bytes())),
                ..entry(ListingType::Hardlink, "src/main.rs")
            },
        ];
        let header = Header {
            archive_bytes: 233,
            ..header()
        };
        let written = decoded(&encode(&header, &entries));
        assert_eq!(String::from_utf8(written).expect("UTF-8"), example);
        let listing = read_full(&compressed(example.as_bytes())[..]).expect("a valid listing");
        assert_eq!(listing.entries, entries);
        assert_eq!((listing.files, listing.bytes), (1, 4211));
    }

    #[test]
    fn entry_lines_have_fixed_width_fields_and_the_path_at_offset_54() {
        let big = ListingEntry {
            size: 1_234_567_890_123_456,
            mode: 0o4755,
            ..file("big", b"")
        };
        let entries = [dir("d"), big, symlink("l", b"t"), hard("h", "big", b"")];
        let decoded = text(&entries);
        let comments: Vec<&[u8]> = decoded
            .split(|byte| *byte == b'\n')
            .skip_while(|line| *line != b"#")
            .filter(|line| line.starts_with(b"# "))
            .collect();
        let paths: Vec<&[u8]> = comments.iter().map(|line| &line[54..]).collect();
        assert_eq!(
            paths,
            [&b"d/"[..], b"big", b"l\t-> t", b"h\t=> big"],
            "{comments:?}"
        );
        assert_eq!(
            &comments[1][..54],
            b"# file  4755  2026-10-30T17:44:02Z  1234567890123456  "
        );
    }

    #[test]
    fn a_hard_link_has_a_hard_line_and_a_checksum_line_equal_to_its_target() {
        let lines = lines_of(&[file("a", b"abc"), hard("b", "a", b"abc")]);
        let hash = sha(b"abc");
        assert!(lines.contains(&format!("{hash}  a\n")), "{lines:?}");
        assert!(lines.contains(&format!("{hash}  b\n")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.starts_with("# hard  0644")),
            "{lines:?}"
        );
    }

    #[test]
    fn files_and_bytes_count_a_hard_linked_file_once() {
        let entries = [file("a", b"abc"), hard("b", "a", b"abc"), file("c", b"xy")];
        assert_eq!(counts(&entries), Some((2, 5)));
        let lines = lines_of(&entries);
        assert!(
            lines.contains(&"# files         2\n".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"# bytes         5\n".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn imported_and_rebuilt_follow_bytes_in_the_key_column() {
        let header = Header {
            imported: Some(at("2026-11-05T10:00:00Z")),
            rebuilt: Some(at("2026-11-06T11:00:00Z")),
            ..header()
        };
        let lines = String::from_utf8(decoded(&encode(&header, &[]))).expect("UTF-8");
        assert!(
            lines.contains(
                "# bytes         0\n# imported      2026-11-05T10:00:00Z\n# rebuilt       2026-11-06T11:00:00Z\n#\n"
            ),
            "{lines}"
        );
    }

    #[test]
    fn checksum_lines_escape_only_newline_and_backslash() {
        let content = b"x";
        let hash = sha(content);
        let entries = [
            file("plain", content),
            file("new\nline", content),
            file("back\\slash", content),
            file("tab\there", content),
            file("cr\rmid", content),
        ];
        let decoded = String::from_utf8(text(&entries)).expect("UTF-8");
        for expected in [
            format!("{hash}  plain\n"),
            format!("\\{hash}  new\\nline\n"),
            format!("\\{hash}  back\\\\slash\n"),
            format!("{hash}  tab\there\n"),
            format!("{hash}  cr\rmid\n"),
        ] {
            assert!(decoded.contains(&expected), "{expected:?} in {decoded:?}");
        }
    }

    #[test]
    fn comment_lines_escape_backslash_newline_tab_and_carriage_return_and_never_start_with_a_backslash()
     {
        let entries = [
            file("a\\b\nc\td\re", b"x"),
            symlink("l\nink", b"t\\a\trget"),
        ];
        let header = Header {
            piece: path("pie\nce"),
            ..header()
        };
        let decoded = String::from_utf8(decoded(&encode(&header, &entries))).expect("UTF-8");
        assert!(
            decoded.contains("# piece         pie\\nce\n"),
            "{decoded:?}"
        );
        assert!(decoded.contains("  a\\\\b\\nc\\td\\re\n"), "{decoded:?}");
        assert!(
            decoded.contains("  l\\nink\t-> t\\\\a\\trget\n"),
            "{decoded:?}"
        );
        assert!(
            decoded.lines().all(|line| !line.starts_with("\\#")),
            "{decoded:?}"
        );
    }

    #[test]
    fn the_frame_carries_the_content_checksum() {
        let bytes = encode(&header(), &small());
        assert_ne!(bytes[4] & 0x04, 0, "the checksum flag is set");
    }

    // =========================================================
    // Round trip and readers
    // =========================================================

    #[test]
    fn a_listing_reads_back_as_written() {
        let non_utf8 = RelPath::from_bytes(b"bad\xffname".to_vec()).expect("valid");
        let old = ListingEntry {
            mtime: -5,
            ..file("old", b"o")
        };
        let ancient = ListingEntry {
            mtime: seconds("0001-01-01T00:00:00Z"),
            ..file("ancient", b"a")
        };
        let entries = vec![
            dir("d"),
            file("d/a\\b\nc\td\re", b"x"),
            file_at(non_utf8, b"y"),
            symlink("d/l", b"t\\a\nrget\xff"),
            hard("d/h", "d/a\\b\nc\td\re", b"x"),
            old,
            ancient,
        ];
        let header = Header {
            piece: path("pie\nce"),
            imported: Some(at("2026-11-05T10:00:00Z")),
            rebuilt: Some(at("2026-11-06T11:00:00Z")),
            ..header()
        };
        let bytes = encode(&header, &entries);
        let listing = read_full(&bytes[..]).expect("the listing reads");
        assert_eq!(listing.header, header);
        assert_eq!(listing.entries, entries);
        assert_eq!(Some((listing.files, listing.bytes)), counts(&entries));
    }

    #[test]
    fn a_dir_line_keeps_its_slash_and_a_dash_reads_as_size_zero() {
        let entries = vec![dir("d"), symlink("d/l", b"t"), hard("d/h", "d/x", b"")];
        let listing = read_full(&encode(&header(), &entries)[..]).expect("reads");
        assert_eq!(listing.entries, entries);
    }

    /// A frame whose first block ends after the head, then a body of more
    /// than one block, and the length of the bytes up to that block end.
    fn multi_block() -> (Vec<u8>, usize) {
        let entries: Vec<ListingEntry> = (0..4000)
            .map(|index| file(&format!("dir/file-{index:05}"), b"x"))
            .collect();
        let full = text(&entries);
        let head_end = full
            .windows(3)
            .position(|w| w == b"\n#\n")
            .expect("the head ends")
            + 3;
        let mut encoder =
            ::zstd::stream::write::Encoder::new(Vec::new(), LISTING_LEVEL).expect("an encoder");
        encoder.include_checksum(true).expect("a flag");
        encoder.write_all(&full[..head_end]).expect("writes");
        encoder.flush().expect("a block ends here");
        let boundary = encoder.get_ref().len();
        encoder.write_all(&full[head_end..]).expect("writes");
        (encoder.finish().expect("the frame ends"), boundary)
    }

    #[test]
    fn read_head_stops_at_the_hash_line_where_read_full_fails_on_a_cut_stream() {
        let (bytes, boundary) = multi_block();
        let cut = &bytes[..boundary];
        let head = read_head(cut).expect("the head is in the first block");
        assert!(head.closed);
        assert_eq!(head.piece, Some(path("projects/scanner")));
        assert_eq!(head.files, Some(4000));
        assert!(
            matches!(read_full(cut), Err(ListingError::Io { .. })),
            "the stream is cut inside its frame"
        );
        assert!(read_full(&bytes[..]).is_ok());
    }

    #[test]
    fn a_head_without_archive_bytes_still_says_what_it_holds() {
        let content = String::from_utf8(text(&small()))
            .expect("UTF-8")
            .replace("# archive-bytes 1093744120\n", "");
        let head = read_head(&compressed(content.as_bytes())[..]).expect("a head");
        assert_eq!(head.archive_bytes, None);
        assert_eq!(head.piece, Some(path("projects/scanner")));
        assert_eq!(head.files, Some(1));
        assert!(head.closed);
        assert_eq!(
            head.damage().as_deref(),
            Some("the `archive-bytes` line is missing or unparsable")
        );
    }

    #[test]
    fn a_complete_head_has_no_damage_and_an_unclosed_one_says_so() {
        let head = read_head(&encode(&header(), &small())[..]).expect("a head");
        assert_eq!(head.damage(), None);
        let content = "# antiq listing 1\n# piece         a\n";
        let head = read_head(&compressed(content.as_bytes())[..]).expect("a head");
        assert!(!head.closed);
        assert_eq!(
            head.damage().as_deref(),
            Some("the `archived` line is missing or unparsable")
        );
    }

    // =========================================================
    // The size limit
    // =========================================================

    fn assert_too_large(error: ListingError, expected: u64) {
        match error {
            ListingError::TooLarge { limit } => assert_eq!(limit, expected),
            other => panic!("expected the size limit to be met, got {other}"),
        }
    }

    /// A first line, then one line of `length` bytes that never ends, so a
    /// reader that waits for the newline holds all of it.
    fn endless_line(length: usize) -> Vec<u8> {
        let mut content = b"# antiq listing 1\n".to_vec();
        content.resize(content.len() + length, b'x');
        compressed(&content)
    }

    #[test]
    fn read_full_accepts_a_listing_of_exactly_the_limit_and_refuses_one_byte_less() {
        let bytes = encode(&header(), &small());
        let size = decoded(&bytes).len() as u64;
        assert!(read_full_within(&bytes[..], size).is_ok());
        assert_too_large(
            read_full_within(&bytes[..], size - 1).expect_err("one byte over"),
            size - 1,
        );
    }

    #[test]
    fn read_head_refuses_a_head_that_runs_past_the_limit() {
        let mut content = b"# antiq listing 1\n".to_vec();
        // More than the 8 KiB the line reader fills at a time, so the limit is
        // met inside the head and not while the first line is read.
        for _ in 0..1000 {
            content.extend_from_slice(b"# piece         a\n");
        }
        assert_too_large(
            read_head_within(&compressed(&content)[..], 10_000).expect_err("a head of 20 KB"),
            10_000,
        );
    }

    #[test]
    fn decode_all_refuses_a_listing_past_the_limit() {
        let bytes = encode(&header(), &small());
        let size = decoded(&bytes).len() as u64;
        assert_eq!(
            decode_all_within(&bytes[..], size).expect("exactly the limit"),
            decoded(&bytes)
        );
        assert_too_large(
            decode_all_within(&bytes[..], size - 1).expect_err("one byte over"),
            size - 1,
        );
    }

    #[test]
    fn a_line_without_a_newline_is_cut_off_at_the_limit() {
        let bytes = endless_line(1 << 20);
        assert_too_large(
            read_full_within(&bytes[..], 4096).expect_err("a line of 1 MiB"),
            4096,
        );
        assert_too_large(
            read_head_within(&bytes[..], 4096).expect_err("a line of 1 MiB"),
            4096,
        );
    }

    /// Counts the bytes its reader is asked for.
    struct Counting<R> {
        inner: R,
        asked: usize,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.asked += buf.len();
            self.inner.read(buf)
        }
    }

    #[test]
    fn bounded_never_asks_for_more_than_one_byte_past_the_limit() {
        let source = Counting {
            inner: io::repeat(b'a'),
            asked: 0,
        };
        let mut reader = BufReader::new(Bounded::new(source, 1000));
        let error = reader
            .read_until(b'\n', &mut Vec::new())
            .expect_err("a line that never ends");
        assert_eq!(error.to_string(), "more than 1000 decoded bytes");
        assert_too_large(io_error(error), 1000);
        assert_eq!(reader.get_ref().inner.asked, 1001);
    }

    #[test]
    fn decode_all_checks_the_first_line_before_it_holds_the_rest() {
        let mut content = b"not a listing\n".to_vec();
        content.resize(1 << 20, b'x');
        assert!(matches!(
            decode_all_within(&compressed(&content)[..], 1 << 16),
            Err(ListingError::InvalidHeader)
        ));
    }

    #[test]
    fn the_error_names_the_limit() {
        assert_eq!(
            ListingError::TooLarge { limit: 1 << 32 }.to_string(),
            "the listing decodes to more than 4294967296 bytes, the most antiq reads"
        );
    }

    #[test]
    fn a_flipped_bit_in_the_body_fails_read_full() {
        let mut bytes = encode(&header(), &small());
        // The last four bytes of the frame are its content checksum, so
        // only the check of that checksum can see this flip.
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(matches!(
            read_full(&bytes[..]),
            Err(ListingError::Io { .. })
        ));
    }

    /// Replaces the first occurrence of `from` in the valid text of
    /// [`small`] and reads the result.
    fn read_edited(from: &str, to: &str) -> ListingError {
        let content = String::from_utf8(text(&small())).expect("UTF-8");
        assert!(content.contains(from), "{from:?} in {content:?}");
        let edited = content.replacen(from, to, 1);
        read_full(&compressed(edited.as_bytes())[..]).expect_err("the edit breaks the listing")
    }

    fn assert_line_error(error: ListingError, needle: &str) {
        match error {
            ListingError::Line { reason, .. } => {
                assert!(reason.contains(needle), "{reason:?} lacks {needle:?}")
            }
            other => panic!("expected a line error about {needle:?}, got {other}"),
        }
    }

    #[test]
    fn read_full_refuses_a_header_out_of_order() {
        let error = read_edited(
            "# archived      2026-11-04T09:02:11Z\n# archive       projects-scanner--2026-11-04T090211.tar.zst\n",
            "# archive       projects-scanner--2026-11-04T090211.tar.zst\n# archived      2026-11-04T09:02:11Z\n",
        );
        assert_line_error(error, "order");
    }

    #[test]
    fn read_full_refuses_a_missing_header_field() {
        assert_line_error(read_edited("# bytes         12\n", ""), "bytes");
    }

    #[test]
    fn read_full_refuses_a_head_without_its_closing_hash_line() {
        assert_line_error(read_edited("#\n", ""), "closing");
    }

    fn assert_end_error(error: ListingError, needle: &str) {
        match error {
            ListingError::End { reason } => {
                assert!(reason.contains(needle), "{reason:?} lacks {needle:?}")
            }
            other => panic!("expected an error at the end about {needle:?}, got {other}"),
        }
    }

    #[test]
    fn read_full_refuses_a_dash_as_the_size_of_a_file() {
        assert_line_error(
            read_edited(
                "              12  src/main.rs",
                "               -  src/main.rs",
            ),
            "size",
        );
    }

    #[test]
    fn read_full_refuses_a_file_line_without_its_checksum_line() {
        let sum = format!("{}  src/main.rs\n", sha(b"fn main() {}"));
        assert_line_error(read_edited(&sum, ""), "checksum line");
    }

    #[test]
    fn read_full_refuses_an_orphan_checksum_line() {
        let orphan = format!(
            "# dir   0755  2026-10-30T17:44:02Z                 -  src/\n{}  x\n",
            sha(b"")
        );
        assert_line_error(
            read_edited(
                "# dir   0755  2026-10-30T17:44:02Z                 -  src/\n",
                &orphan,
            ),
            "checksum line",
        );
    }

    #[test]
    fn read_full_refuses_a_second_checksum_line() {
        let sum = format!("{}  src/main.rs\n", sha(b"fn main() {}"));
        assert_line_error(read_edited(&sum, &format!("{sum}{sum}")), "checksum line");
    }

    #[test]
    fn read_full_refuses_a_checksum_line_for_another_path() {
        assert_line_error(read_edited("  src/main.rs\n", "  src/other.rs\n"), "path");
    }

    #[test]
    fn read_full_refuses_a_hash_of_63_digits() {
        let sum = sha(b"fn main() {}");
        assert_line_error(read_edited(&sum, &sum[1..]), "hash");
    }

    #[test]
    fn read_full_refuses_a_time_that_does_not_parse() {
        assert_line_error(
            read_edited("2026-10-30T17:44:02Z", "2026-13-30T17:44:02Z"),
            "time",
        );
    }

    #[test]
    fn read_full_refuses_an_unknown_type() {
        assert_line_error(read_edited("# file  0644", "# fifo  0644"), "type");
    }

    #[test]
    fn read_full_refuses_a_path_that_occurs_twice() {
        let lines = "# dir   0755  2026-10-30T17:44:02Z                 -  src/\n";
        assert_line_error(read_edited(lines, &format!("{lines}{lines}")), "twice");
    }

    #[test]
    fn read_full_refuses_a_files_count_that_disagrees_with_the_entries() {
        assert_end_error(
            read_edited("# files         1\n", "# files         2\n"),
            "`files`",
        );
    }

    #[test]
    fn read_full_refuses_a_bytes_count_that_disagrees_with_the_entries() {
        assert_end_error(
            read_edited("# bytes         12\n", "# bytes         13\n"),
            "`bytes`",
        );
    }

    #[test]
    fn a_listing_that_stops_inside_its_head_is_refused_at_its_end() {
        let content = "# antiq listing 1\n# piece         a\n";
        let error = read_full(&compressed(content.as_bytes())[..]).expect_err("a short head");
        assert_end_error(error, "`archived`");
    }

    #[test]
    fn a_listing_that_stops_before_the_closing_hash_line_is_refused_at_its_end() {
        let content = String::from_utf8(text(&[]))
            .expect("UTF-8")
            .replace("#\n", "");
        let error = read_full(&compressed(content.as_bytes())[..]).expect_err("no closing line");
        assert_end_error(error, "closing");
    }

    #[test]
    fn a_listing_that_stops_before_the_last_checksum_line_is_refused_at_its_end() {
        let sum = format!("{}  f\n", sha(b"x"));
        let content = String::from_utf8(text(&[file("f", b"x")]))
            .expect("UTF-8")
            .replace(&sum, "");
        let error = read_full(&compressed(content.as_bytes())[..]).expect_err("no checksum");
        assert_end_error(error, "checksum line");
    }

    #[test]
    fn read_full_refuses_a_listing_that_ends_inside_a_line() {
        let content = String::from_utf8(text(&small())).expect("UTF-8");
        let cut = &content[..content.len() - 1];
        let error = read_full(&compressed(cut.as_bytes())[..]).expect_err("cut line");
        assert_line_error(error, "ends");
    }

    #[test]
    fn read_full_refuses_an_unknown_escape_and_a_trailing_backslash() {
        assert_line_error(
            read_edited(
                "              12  src/main.rs",
                "              12  src/ma\\xin.rs",
            ),
            "not an escape",
        );
        assert_line_error(
            read_edited("  src/main.rs\n", "  src/main.rs\\\n"),
            "backslash",
        );
    }

    #[test]
    fn read_full_refuses_an_entry_line_out_of_the_layout() {
        assert_line_error(read_edited("# file  0644", "# file 0644"), "fixed-width");
    }

    #[test]
    fn read_full_refuses_a_size_on_a_directory() {
        assert_line_error(
            read_edited("-  src/\n", "5  src/\n"),
            "only a file has a size",
        );
    }

    #[test]
    fn read_full_refuses_a_raw_tab_in_a_path() {
        assert_line_error(read_edited("  src/main.rs\n", "  src/ma\tin.rs\n"), "tab");
    }

    #[test]
    fn read_full_refuses_a_directory_path_without_its_slash() {
        assert_line_error(read_edited("  src/\n", "  src\n"), "ends in `/`");
    }

    #[test]
    fn read_full_refuses_the_top_level_directory() {
        assert_line_error(read_edited("  src/\n", "  /\n"), "top-level");
    }

    #[test]
    fn read_full_refuses_a_header_value_that_does_not_parse() {
        assert_line_error(
            read_edited("# files         1\n", "# files         x\n"),
            "`files` value",
        );
    }

    #[test]
    fn read_full_refuses_an_imported_time_that_does_not_parse() {
        let header = Header {
            imported: Some(at("2026-11-05T10:00:00Z")),
            ..header()
        };
        let content = String::from_utf8(decoded(&encode(&header, &[])))
            .expect("UTF-8")
            .replace("2026-11-05T10:00:00Z", "2026-11-05 10:00:00Z");
        let error = read_full(&compressed(content.as_bytes())[..]).expect_err("bad time");
        assert_line_error(error, "`imported` value");
    }

    #[test]
    fn read_head_ignores_a_line_it_does_not_know() {
        let content = String::from_utf8(text(&small()))
            .expect("UTF-8")
            .replace("#\n# dir", "# future        x\n#\n# dir");
        let head = read_head(&compressed(content.as_bytes())[..]).expect("a head");
        assert!(head.closed);
        assert_eq!(head.damage(), None);
    }

    #[test]
    fn times_that_are_not_the_canonical_spelling_do_not_parse() {
        for text in [
            "2026-10-30T17:44:02Z",
            "0001-01-01T00:00:00Z",
            "9999-12-31T23:59:59Z",
        ] {
            assert!(parse_time(text.as_bytes()).is_some(), "{text}");
        }
        for text in [
            "2026-10-3OT17:44:02Z",
            "2026/10/30T17:44:02Z",
            "2026-10-30 17:44:02Z",
            "2026-02-30T17:44:02Z",
            "2026-10-30T24:00:00Z",
            "0000-01-01T00:00:00Z",
            "2026-10-30T17:44:02",
        ] {
            assert_eq!(parse_time(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn scan_paths_reports_a_head_that_is_never_closed() {
        let content = "# antiq listing 1\n# piece         a\n";
        let error = scan_paths(content.as_bytes())
            .find_map(Result::err)
            .expect("an error");
        assert_end_error(error, "closing");
    }

    #[test]
    fn scan_paths_names_the_line_of_an_entry_it_cannot_read() {
        let decoded = String::from_utf8(text(&small()))
            .expect("UTF-8")
            .replace("# file  0644", "# fifo  0644");
        let error = scan_paths(decoded.as_bytes())
            .find_map(Result::err)
            .expect("a bad line");
        assert!(
            matches!(error, ListingError::Line { number: 10, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_newer_format_is_named_whatever_follows() {
        let newer =
            |first: &str| read_full(&compressed(format!("{first}\nnonsense").as_bytes())[..]);
        assert!(matches!(
            newer("# antiq listing 2"),
            Err(ListingError::FormatTooNew { found: 2 })
        ));
        assert!(matches!(
            newer("# antiq listing 99999999999999999999"),
            Err(ListingError::FormatTooNew { found: u32::MAX })
        ));
        for first in [
            "# antiq listing 0",
            "# antiq listing ",
            "# antiq listing +2",
            "# antiq listing 1x",
            "{\"format\":1}",
        ] {
            assert!(
                matches!(newer(first), Err(ListingError::InvalidHeader)),
                "{first}"
            );
        }
        assert!(matches!(
            read_head(&compressed(b"# antiq listing 3\n")[..]),
            Err(ListingError::FormatTooNew { found: 3 })
        ));
    }

    // =========================================================
    // Writer refusals
    // =========================================================

    fn refused(entry: ListingEntry) -> ListingError {
        write(&header(), &[entry], Vec::new()).expect_err("the entry cannot be listed")
    }

    fn assert_entry_error(error: ListingError, name: &str, needle: &str) {
        match error {
            ListingError::Entry { path, reason } => {
                assert_eq!(path, name);
                assert!(reason.contains(needle), "{reason:?} lacks {needle:?}");
            }
            other => panic!("expected an entry error, got {other}"),
        }
    }

    #[test]
    fn write_refuses_a_hard_link_without_its_hash() {
        let link = ListingEntry {
            sha256: None,
            ..hard("h", "a", b"")
        };
        assert_entry_error(refused(link), "h", "hash");
    }

    #[test]
    fn write_refuses_a_hash_on_a_directory() {
        let dir = ListingEntry {
            sha256: Some(sha(b"")),
            ..dir("d")
        };
        assert_entry_error(refused(dir), "d", "belongs to files");
    }

    #[test]
    fn write_refuses_a_size_the_field_cannot_hold() {
        let big = ListingEntry {
            size: 10_000_000_000_000_000,
            ..file("f", b"")
        };
        assert_entry_error(refused(big), "f", "16 digits");
        let widest = ListingEntry {
            size: 9_999_999_999_999_999,
            ..file("f", b"")
        };
        let listing =
            read_full(&encode(&header(), std::slice::from_ref(&widest))[..]).expect("reads");
        assert_eq!(listing.entries, [widest]);
    }

    #[test]
    fn the_sizes_of_the_entries_that_add_up_beyond_64_bits_are_refused() {
        let many: Vec<ListingEntry> = (0..2000)
            .map(|index| ListingEntry {
                size: MAX_SIZE,
                ..file(&format!("f{index}"), b"")
            })
            .collect();
        assert_eq!(counts(&many), None);
        let error = write(&header(), &many, Vec::new()).expect_err("the sum overflows");
        assert!(matches!(error, ListingError::Header { .. }), "{error}");
    }

    #[test]
    fn a_listing_whose_sizes_add_up_beyond_64_bits_is_refused_at_its_end() {
        let many: Vec<ListingEntry> = (0..2000)
            .map(|index| ListingEntry {
                size: MAX_SIZE,
                ..file(&format!("f{index}"), b"")
            })
            .collect();
        let mut content = head_bytes(&header(), 0, 0).expect("a head");
        for entry in &many {
            content.extend_from_slice(&entry_bytes(entry));
        }
        let error = read_full(&compressed(&content)[..]).expect_err("the sum overflows");
        assert_end_error(error, "add up");
    }

    /// A sink that keeps what it was given in a buffer until it is
    /// flushed, and does not flush on drop, as a buffered file would.
    struct Buffered {
        pending: Vec<u8>,
        flushed: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }

    impl Write for Buffered {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.pending.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed.borrow_mut().append(&mut self.pending);
            Ok(())
        }
    }

    #[test]
    fn written_reports_the_size_and_hash_of_the_flushed_bytes() {
        let flushed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = Buffered {
            pending: Vec::new(),
            flushed: flushed.clone(),
        };
        let written = write(&header(), &small(), sink).expect("writes");
        let bytes = flushed.borrow();
        assert_eq!(written.size, bytes.len() as u64);
        assert_eq!(written.sha256, sha(&bytes));
        assert_eq!(bytes.as_slice(), encode(&header(), &small()).as_slice());
    }

    #[test]
    fn read_full_refuses_a_size_with_a_leading_zero() {
        assert_line_error(
            read_edited(
                "              12  src/main.rs",
                "            0012  src/main.rs",
            ),
            "size",
        );
    }

    #[test]
    fn read_full_refuses_a_header_number_with_a_leading_zero() {
        assert_line_error(
            read_edited("# files         1\n", "# files         01\n"),
            "`files` value",
        );
    }

    #[test]
    fn read_full_refuses_a_type_field_not_padded_with_spaces() {
        assert_line_error(read_edited("# dir   0755", "# dir\t  0755"), "unknown type");
    }

    #[test]
    fn write_refuses_a_directory_with_a_size() {
        let dir = ListingEntry {
            size: 1,
            ..dir("d")
        };
        assert_entry_error(refused(dir), "d", "size");
    }

    #[test]
    fn write_refuses_a_mode_above_the_permission_bits() {
        let file = ListingEntry {
            mode: 0o10000,
            ..file("f", b"")
        };
        assert_entry_error(refused(file), "f", "mode");
    }

    #[test]
    fn write_refuses_the_root_a_missing_link_and_a_bad_hash() {
        assert_entry_error(refused(dir("")), "", "top-level");
        let link = ListingEntry {
            link: None,
            ..symlink("l", b"")
        };
        assert_entry_error(refused(link), "l", "link");
        let bad = ListingEntry {
            sha256: Some("AB".repeat(32)),
            ..file("f", b"")
        };
        assert_entry_error(refused(bad), "f", "hash");
    }

    #[test]
    fn write_refuses_a_time_the_field_cannot_hold() {
        for time in [253_402_300_800, seconds("0001-01-01T00:00:00Z") - 1] {
            let file = ListingEntry {
                mtime: time,
                ..file("f", b"")
            };
            assert_entry_error(refused(file), "f", "time");
        }
        let last = ListingEntry {
            mtime: 253_402_300_799,
            ..file("f", b"")
        };
        write(&header(), &[last], Vec::new()).expect("the last second of 9999 fits");
    }

    #[test]
    fn write_refuses_an_archive_name_with_a_newline() {
        let header = Header {
            archive: "a\nb".into(),
            ..header()
        };
        let error = write(&header, &[], Vec::new()).expect_err("a line break in the name");
        assert!(matches!(error, ListingError::Header { .. }), "{error}");
    }

    // =========================================================
    // Grep support
    // =========================================================

    #[test]
    fn scan_paths_yields_each_path_once_unescaped() {
        let name = "a\\b\nc\td";
        let entries = [dir("d"), file(name, b"x"), hard("h", name, b"x")];
        let decoded = text(&entries);
        let scanned: Vec<ScannedPath> = scan_paths(&decoded)
            .collect::<Result<_, _>>()
            .expect("the entries scan");
        let expected = |kind, path: &str| ScannedPath {
            kind,
            path: path.as_bytes().to_vec(),
        };
        assert_eq!(
            scanned,
            [
                expected(ListingType::Dir, "d"),
                expected(ListingType::File, name),
                expected(ListingType::Hardlink, "h"),
            ]
        );
        assert_eq!(escape_path(name.as_bytes()), b"a\\\\b\\nc\\td");
    }

    #[test]
    fn decode_all_returns_the_bytes_and_checks_the_first_line_and_the_frame() {
        let bytes = encode(&header(), &small());
        assert_eq!(decode_all(&bytes[..]).expect("decodes"), decoded(&bytes));
        let newer = compressed(b"# antiq listing 2\n");
        assert!(matches!(
            decode_all(&newer[..]),
            Err(ListingError::FormatTooNew { found: 2 })
        ));
        let mut damaged = bytes;
        let last = damaged.len() - 2;
        damaged[last] ^= 0xff;
        assert!(decode_all(&damaged[..]).is_err());
    }

    // =========================================================
    // shasum and GNU sha256sum
    // =========================================================

    /// Makes `names` files below `dir`, each holding `content`.
    fn make_files(dir: &Path, names: &[&[u8]], content: &[u8]) {
        use std::os::unix::ffi::OsStrExt;
        for name in names {
            let target = dir.join(std::ffi::OsStr::from_bytes(name));
            fs::write(&target, content).expect("the file is created");
        }
    }

    fn run(tool: &str, dir: &Path) -> Output {
        Command::new(tool)
            .args(if tool.contains("shasum") {
                vec!["-a", "256", "--strict", "-c", "listing.sha256"]
            } else {
                vec!["--strict", "-c", "listing.sha256"]
            })
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("cannot run {tool}: {error}"))
    }

    /// The GNU `sha256sum` of this machine: Apple's own `sha256sum`, where
    /// one is on the path, is not it.
    fn gnu_sha256sum() -> Option<String> {
        ["sha256sum", "gsha256sum"].into_iter().find_map(|tool| {
            let output = Command::new(tool).arg("--version").output().ok()?;
            String::from_utf8_lossy(&output.stdout)
                .contains("GNU coreutils")
                .then(|| tool.to_string())
        })
    }

    fn require_gnu_sha256sum() -> String {
        gnu_sha256sum()
            .expect("GNU coreutils sha256sum (or gsha256sum) is needed on the path for this test")
    }

    fn check_with_tools(names: &[&[u8]], tools: &[String]) {
        let content = b"listed content";
        let entries: Vec<ListingEntry> = names
            .iter()
            .map(|name| file_at(RelPath::from_bytes(name.to_vec()).expect("valid"), content))
            .collect();
        let dir = tempfile::tempdir().expect("a temp dir");
        make_files(dir.path(), names, content);
        fs::write(dir.path().join("listing.sha256"), text(&entries)).expect("writes");
        for tool in tools {
            let output = run(tool, dir.path());
            assert!(
                output.status.success(),
                "{tool}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn awkward_names_pass_shasum_and_gnu_sha256sum_strict() {
        let names: [&[u8]; 8] = [
            b"plain.txt",
            b"with space.txt",
            b" leading.txt",
            b"*star.txt",
            b"tab\there",
            b"back\\slash",
            b"new\nline",
            b"cr\rmid",
        ];
        check_with_tools(&names, &["shasum".to_string(), require_gnu_sha256sum()]);
    }

    #[test]
    fn a_name_ending_in_a_carriage_return_is_read_by_shasum_and_not_by_gnu() {
        let names: [&[u8]; 1] = [b"ab\r"];
        check_with_tools(&names, &["shasum".to_string()]);
        if let Some(gnu) = gnu_sha256sum() {
            let dir = tempfile::tempdir().expect("a temp dir");
            make_files(dir.path(), &names, b"listed content");
            let entry = file("ab\r", b"listed content");
            fs::write(dir.path().join("listing.sha256"), text(&[entry])).expect("writes");
            assert!(
                !run(&gnu, dir.path()).status.success(),
                "GNU strips the carriage return"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_passes_both_tools() {
        let names: [&[u8]; 1] = [b"bad\xffname"];
        check_with_tools(&names, &["shasum".to_string(), require_gnu_sha256sum()]);
    }
}
