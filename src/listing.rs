// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archive listings: one JSON line per entry of an archive, compressed.

use crate::error::{ErrorKind, HasKind};
use crate::meta::FileName;
use crate::pack::hashing::HashingWriter;
use crate::pack::zstd::decoder;
use crate::relpath::{RelPath, escape, unescape};
use ::zstd::stream::read::Decoder;
use ::zstd::stream::write::Encoder;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::io::{self, BufRead, BufReader, Read, Write};

/// The format of the listings this version reads and writes.
pub const FORMAT: u32 = 1;

/// Listings compress at level 3 with the content checksum. They are small
/// next to the archive they describe, and writing one takes no worker of
/// the compression pool.
pub const LISTING_LEVEL: i32 = 3;

/// The type of a listed entry. Special files are not archived, so they
/// have no type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListingType {
    File,
    Dir,
    Symlink,
    Hardlink,
}

/// What a link points to: any bytes, in the escaped form as text. A
/// symlink target is not a path inside the piece, so it is not a
/// [`RelPath`].
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

impl Serialize for LinkTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&escape(&self.0))
    }
}

impl<'de> Deserialize<'de> for LinkTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        unescape(&text)
            .map(LinkTarget)
            .map_err(serde::de::Error::custom)
    }
}

/// One entry line of a listing, keys in this order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingEntry {
    /// Relative to the archive's top-level directory.
    pub path: RelPath,
    #[serde(rename = "type")]
    pub kind: ListingType,
    pub size: u64,
    /// Modification time in whole seconds since the epoch.
    pub mtime: i64,
    /// Permission bits.
    pub mode: u32,
    /// The target, for symlinks and hard links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkTarget>,
    /// SHA-256 of the content as lower-case hex, for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// What a written listing came to: the figures of the compressed bytes,
/// which are the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub size: u64,
    /// SHA-256 as lower-case hex.
    pub sha256: String,
}

fn io_error(source: io::Error) -> ListingError {
    ListingError::Io { source }
}

/// Whether `text` is a SHA-256 in lower-case hex.
fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The rules of spec "Listing" that tie the keys of an entry to its type,
/// which the entry type itself leaves open. A reader that accepted a
/// symlink without a target or a directory with a checksum would hand the
/// comparison with the tree an entry it cannot make sense of.
fn check_entry(entry: &ListingEntry) -> Result<(), String> {
    if entry.path.is_root() {
        return Err("the top-level directory has no line of its own".into());
    }
    let is_link = matches!(entry.kind, ListingType::Symlink | ListingType::Hardlink);
    if is_link != entry.link.is_some() {
        return Err("`link` belongs to symlinks and hard links, and only to them".into());
    }
    let is_file = entry.kind == ListingType::File;
    match &entry.sha256 {
        Some(digest) if is_file && is_sha256_hex(digest) => Ok(()),
        Some(_) if is_file => Err("`sha256` is not lower-case hex of 64 digits".into()),
        None if is_file => Err("a file needs a `sha256`".into()),
        Some(_) => Err("`sha256` belongs to files, and only to them".into()),
        None => Ok(()),
    }
}

/// Writes a listing: the header line, then one line per entry, compressed.
///
/// `out` is flushed but not synced; making a file durable is up to the
/// caller, who holds the file.
pub fn write<'a, W: Write>(
    entries: impl IntoIterator<Item = &'a ListingEntry>,
    out: &mut W,
) -> Result<Written, ListingError> {
    let mut encoder =
        Encoder::new(HashingWriter::new(&mut *out), LISTING_LEVEL).map_err(io_error)?;
    encoder.include_checksum(true).map_err(io_error)?;
    encoder
        .write_all(format!("{{\"format\":{FORMAT}}}\n").as_bytes())
        .map_err(io_error)?;
    for entry in entries {
        check_entry(entry).map_err(|reason| ListingError::Entry {
            path: entry.path.to_string(),
            reason,
        })?;
        serde_json::to_writer(&mut encoder, entry).map_err(|error| io_error(error.into()))?;
        encoder.write_all(b"\n").map_err(io_error)?;
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

/// The entries of a listing, one at a time.
///
/// The content checksum of the stream is checked when its last byte has
/// been read, so entries are verified only once the iterator has returned
/// `None`. A caller that acts on entries as they arrive can act on data
/// that a later error shows to be corrupt; [`read_all`] returns nothing
/// unless the whole stream is intact.
pub struct Reader<R: Read> {
    lines: BufReader<Decoder<'static, BufReader<R>>>,
    number: usize,
    done: bool,
}

impl<R: Read> std::fmt::Debug for Reader<R> {
    // The decoder holds a zstd context, which has no `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("line", &self.number)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<R: Read> Reader<R> {
    /// Opens the stream and reads its header line. The decoder refuses
    /// windows above the limit every reader of antiq has, which shows
    /// here or on a later read, depending on where the stream's first
    /// frame header ends.
    pub fn new(input: R) -> Result<Reader<R>, ListingError> {
        let mut lines = BufReader::new(decoder(input).map_err(io_error)?);
        let mut header = Vec::new();
        lines.read_until(b'\n', &mut header).map_err(io_error)?;
        check_header(&header)?;
        Ok(Reader {
            lines,
            number: 1,
            done: false,
        })
    }
}

/// Only `format` is read from the header, so a later format whose header
/// has more keys is still recognised as later.
fn check_header(line: &[u8]) -> Result<(), ListingError> {
    #[derive(Deserialize)]
    struct Header {
        format: u32,
    }
    let Header { format } =
        serde_json::from_slice(line).map_err(|_| ListingError::InvalidHeader)?;
    match format {
        0 => Err(ListingError::InvalidHeader),
        FORMAT => Ok(()),
        found => Err(ListingError::FormatTooNew { found }),
    }
}

fn parse_line(line: &[u8], number: usize) -> Result<ListingEntry, ListingError> {
    let fail = |reason: String| ListingError::Line { number, reason };
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.is_empty() {
        return Err(fail("the line is empty".into()));
    }
    let entry: ListingEntry =
        serde_json::from_slice(line).map_err(|error| fail(error.to_string()))?;
    check_entry(&entry).map_err(fail)?;
    Ok(entry)
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<ListingEntry, ListingError>;

    /// An error is the last item: after it the stream cannot be trusted.
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut line = Vec::new();
        let read = self.lines.read_until(b'\n', &mut line);
        self.number += 1;
        let item = match read {
            Ok(0) => {
                self.done = true;
                return None;
            }
            Ok(_) => parse_line(&line, self.number),
            Err(source) => Err(io_error(source)),
        };
        self.done = item.is_err();
        Some(item)
    }
}

/// All entries of a listing.
pub fn read_all<R: Read>(input: R) -> Result<Vec<ListingEntry>, ListingError> {
    Reader::new(input)?.collect()
}

/// The name of the listing of the payload with this SHA-256: its first 16
/// digits, so the name follows from the payload.
pub fn file_name(payload_sha256: &str) -> Result<FileName, ListingError> {
    if !is_sha256_hex(payload_sha256) {
        return Err(ListingError::InvalidDigest {
            digest: payload_sha256.to_string(),
        });
    }
    Ok(
        FileName::new(format!("{}.listing.zst", &payload_sha256[..16]))
            .expect("16 hex digits and a suffix are a plain name"),
    )
}

/// Errors of archive listings.
#[derive(Debug, thiserror::Error)]
pub enum ListingError {
    #[error("cannot write or read the listing: {source}")]
    Io { source: io::Error },
    #[error("the listing does not start with a header line of format 1 or later")]
    InvalidHeader,
    #[error("the listing has format {found}, which this version of antiq cannot read")]
    FormatTooNew { found: u32 },
    #[error("line {number} of the listing is not valid: {reason}")]
    Line { number: usize, reason: String },
    #[error("the entry {path} cannot be listed: {reason}")]
    Entry { path: String, reason: String },
    #[error("`{digest}` is not a SHA-256 in lower-case hex")]
    InvalidDigest { digest: String },
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
    use crate::relpath::RelPath;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn line(entry: &ListingEntry) -> String {
        serde_json::to_string(entry).expect("serialises")
    }

    #[test]
    fn a_file_entry_has_its_keys_in_order_and_a_checksum() {
        let entry = ListingEntry {
            path: path("src/main.rs"),
            kind: ListingType::File,
            size: 120,
            mtime: 1_600_000_000,
            mode: 0o644,
            link: None,
            sha256: Some("ab".repeat(32)),
        };
        assert_eq!(
            line(&entry),
            format!(
                r#"{{"path":"src/main.rs","type":"file","size":120,"mtime":1600000000,"mode":420,"sha256":"{}"}}"#,
                "ab".repeat(32)
            )
        );
    }

    #[test]
    fn a_directory_entry_has_neither_link_nor_checksum() {
        let entry = ListingEntry {
            path: path("src"),
            kind: ListingType::Dir,
            size: 0,
            mtime: 5,
            mode: 0o755,
            link: None,
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"src","type":"dir","size":0,"mtime":5,"mode":493}"#
        );
    }

    #[test]
    fn a_symlink_entry_carries_its_target_in_the_escaped_form() {
        let entry = ListingEntry {
            path: path("link"),
            kind: ListingType::Symlink,
            size: 0,
            mtime: 5,
            mode: 0o777,
            link: Some(LinkTarget::from_bytes(b"../a%\xff".to_vec())),
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"link","type":"symlink","size":0,"mtime":5,"mode":511,"link":"../a%25%FF"}"#
        );
    }

    #[test]
    fn a_hard_link_entry_names_the_path_it_links_to() {
        let entry = ListingEntry {
            path: path("copy"),
            kind: ListingType::Hardlink,
            size: 0,
            mtime: 5,
            mode: 0o644,
            link: Some(LinkTarget::from_bytes(b"original".to_vec())),
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"copy","type":"hardlink","size":0,"mtime":5,"mode":420,"link":"original"}"#
        );
    }

    #[test]
    fn every_line_reads_back_as_the_entry_it_was_written_from() {
        let lines = [
            r#"{"path":"a/%FF","type":"file","size":1,"mtime":-3,"mode":420,"sha256":"00"}"#,
            r#"{"path":"d","type":"dir","size":0,"mtime":0,"mode":493}"#,
            r#"{"path":"l","type":"symlink","size":0,"mtime":0,"mode":511,"link":"x%FF"}"#,
            r#"{"path":"h","type":"hardlink","size":0,"mtime":0,"mode":420,"link":"a/b"}"#,
        ];
        for text in lines {
            let entry: ListingEntry = serde_json::from_str(text).expect("a valid line");
            assert_eq!(line(&entry), text);
        }
    }

    #[test]
    fn a_link_target_is_any_bytes() {
        let target = LinkTarget::from_bytes(b"/abs/../x\xc3".to_vec());
        assert_eq!(target.as_bytes(), b"/abs/../x\xc3");
        let text = serde_json::to_string(&target).expect("serialises");
        assert_eq!(text, r#""/abs/../x%C3""#);
        let back: LinkTarget = serde_json::from_str(&text).expect("deserialises");
        assert_eq!(back, target);
        assert!(serde_json::from_str::<LinkTarget>(r#""%41""#).is_err());
    }

    // =====================================================
    // Writing and reading the compressed listing
    // =====================================================

    use crate::pack::hashing::hex;
    use sha2::{Digest, Sha256};

    fn file_entry() -> ListingEntry {
        ListingEntry {
            path: path("a/%FF"),
            kind: ListingType::File,
            size: 1,
            mtime: -3,
            mode: 0o644,
            link: None,
            sha256: Some("ab".repeat(32)),
        }
    }

    fn dir_entry() -> ListingEntry {
        ListingEntry {
            path: path("d"),
            kind: ListingType::Dir,
            size: 0,
            mtime: 0,
            mode: 0o755,
            link: None,
            sha256: None,
        }
    }

    fn symlink_entry() -> ListingEntry {
        ListingEntry {
            path: path("l"),
            kind: ListingType::Symlink,
            size: 0,
            mtime: 0,
            mode: 0o777,
            link: Some(LinkTarget::from_bytes(b"x%\xff".to_vec())),
            sha256: None,
        }
    }

    fn hardlink_entry() -> ListingEntry {
        ListingEntry {
            path: path("h"),
            kind: ListingType::Hardlink,
            size: 0,
            mtime: 0,
            mode: 0o644,
            link: Some(LinkTarget::from_bytes(b"a/%FF".to_vec())),
            sha256: None,
        }
    }

    fn all_entries() -> Vec<ListingEntry> {
        vec![dir_entry(), file_entry(), symlink_entry(), hardlink_entry()]
    }

    fn compressed(entries: &[ListingEntry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        write(entries, &mut bytes).expect("a vec accepts the listing");
        bytes
    }

    fn decompressed(bytes: &[u8]) -> String {
        let mut text = String::new();
        decoder(bytes)
            .expect("a decoder starts")
            .read_to_string(&mut text)
            .expect("the frame decodes");
        text
    }

    /// A listing frame around `text`, made the way the writer makes one.
    fn frame(text: &str) -> Vec<u8> {
        let mut encoder = Encoder::new(Vec::new(), LISTING_LEVEL).expect("an encoder starts");
        encoder.include_checksum(true).expect("a flag");
        encoder
            .write_all(text.as_bytes())
            .expect("a vec accepts it");
        encoder.finish().expect("the frame ends")
    }

    #[test]
    fn a_listing_reads_back_as_the_entries_it_was_written_from() {
        let entries = all_entries();
        assert_eq!(read_all(&compressed(&entries)[..]).expect("reads"), entries);
    }

    #[test]
    fn a_listing_without_entries_reads_as_empty() {
        let bytes = compressed(&[]);
        assert_eq!(decompressed(&bytes), "{\"format\":1}\n");
        assert_eq!(read_all(&bytes[..]).expect("reads"), vec![]);
    }

    #[test]
    fn the_text_is_a_header_line_and_one_line_per_entry() {
        let text = decompressed(&compressed(&[dir_entry(), file_entry()]));
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        assert_eq!(lines.len(), 3, "{text:?}");
        assert_eq!(lines[0], "{\"format\":1}\n");
        assert_eq!(lines[1], format!("{}\n", line(&dir_entry())));
        assert_eq!(lines[2], format!("{}\n", line(&file_entry())));
    }

    #[test]
    fn the_frame_is_level_3_with_the_content_checksum_and_a_window_the_reader_accepts() {
        let entries = all_entries();
        let written = compressed(&entries);
        assert_eq!(written, frame(&decompressed(&written)));
        let descriptor = written[4];
        assert_ne!(descriptor & 0x04, 0, "the content checksum flag is set");
        assert_eq!(
            descriptor & 0x20,
            0,
            "a stream of unknown size has a window"
        );
        let window_log = u32::from(written[5] >> 3) + 10;
        assert!(window_log <= crate::pack::zstd::DECODER_WINDOW_LOG_MAX);
    }

    #[test]
    fn the_written_figures_are_those_of_the_bytes_the_sink_received() {
        let mut sink = Vec::new();
        let figures = write(&all_entries(), &mut sink).expect("writes");
        assert_eq!(figures.size, sink.len() as u64);
        assert_eq!(figures.sha256, hex(&Sha256::digest(&sink)));
    }

    /// Every result of reading `bytes`, up to and including the first
    /// error.
    ///
    /// The decoder reports a bad stream when it has read all of it, which
    /// for a short listing is while the header is read.
    fn results(bytes: &[u8]) -> Vec<Result<ListingEntry, ListingError>> {
        match Reader::new(bytes) {
            Ok(reader) => reader.collect(),
            Err(error) => vec![Err(error)],
        }
    }

    #[test]
    fn a_flipped_checksum_is_an_error_after_which_nothing_follows() {
        let mut bytes = compressed(&all_entries());
        *bytes.last_mut().expect("not empty") ^= 0xff;
        let results = results(&bytes);
        assert!(results.last().expect("a result").is_err(), "{results:?}");
    }

    #[test]
    fn in_a_long_listing_entries_arrive_before_the_checksum_failure_shows() {
        let entries: Vec<ListingEntry> = (0..4000)
            .map(|number| ListingEntry {
                path: path(&format!("dir/file-{number:05}-with-a-longish-name.txt")),
                ..file_entry()
            })
            .collect();
        let mut bytes = compressed(&entries);
        *bytes.last_mut().expect("not empty") ^= 0xff;
        let results = results(&bytes);
        let arrived = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(arrived, entries.len());
        assert!(results.last().expect("a result").is_err());
    }

    #[test]
    fn a_truncated_stream_is_an_error() {
        let bytes = compressed(&all_entries());
        let results = results(&bytes[..bytes.len() - 6]);
        assert!(results.last().expect("a result").is_err(), "{results:?}");
    }

    #[test]
    fn a_header_of_a_later_format_is_refused_before_any_entry() {
        let error = Reader::new(&frame("{\"format\":2}\n")[..]).expect_err("format 2");
        assert!(matches!(error, ListingError::FormatTooNew { found: 2 }));
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
    }

    #[test]
    fn a_reader_shows_where_it_is_when_debugged() {
        let bytes = compressed(&[]);
        let reader = Reader::new(&bytes[..]).expect("a header");
        assert_eq!(format!("{reader:?}"), "Reader { line: 1, done: false, .. }");
    }

    #[test]
    fn only_a_later_format_has_a_kind_of_its_own() {
        let error = Reader::new(&frame("")[..]).expect_err("no header");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_header_of_format_zero_is_invalid() {
        let error = Reader::new(&frame("{\"format\":0}\n")[..]).expect_err("format 0");
        assert!(matches!(error, ListingError::InvalidHeader));
    }

    #[test]
    fn an_empty_stream_has_no_header() {
        let error = Reader::new(&frame("")[..]).expect_err("no header");
        assert!(matches!(error, ListingError::InvalidHeader));
    }

    #[test]
    fn a_header_that_is_not_json_is_invalid() {
        let error = Reader::new(&frame("{\"format\":\n")[..]).expect_err("not json");
        assert!(matches!(error, ListingError::InvalidHeader));
    }

    #[test]
    fn a_window_above_the_decoder_limit_is_refused() {
        let mut encoder = Encoder::new(Vec::new(), LISTING_LEVEL).expect("an encoder starts");
        encoder.window_log(28).expect("a window");
        encoder
            .write_all(b"{\"format\":1}\n")
            .expect("a vec accepts it");
        let bytes = encoder.finish().expect("the frame ends");
        let error = Reader::new(&bytes[..]).expect_err("2^28 is above the limit");
        assert!(matches!(error, ListingError::Io { .. }), "{error:?}");
    }

    /// A listing with the header, one good entry and `third` as line 3.
    fn listing_with_third_line(third: &str) -> Vec<u8> {
        frame(&format!(
            "{{\"format\":1}}\n{}\n{third}\n",
            line(&dir_entry())
        ))
    }

    #[test]
    fn a_line_that_is_not_json_is_reported_with_its_number() {
        let results = results(&listing_with_third_line("oops"));
        assert!(results[0].is_ok());
        assert!(
            matches!(results[1], Err(ListingError::Line { number: 3, .. })),
            "{results:?}"
        );
        assert_eq!(results.len(), 2, "nothing follows an error");
    }

    #[test]
    fn an_empty_line_is_reported_with_its_number() {
        let results = results(&listing_with_third_line(""));
        assert!(
            matches!(results[1], Err(ListingError::Line { number: 3, .. })),
            "{results:?}"
        );
    }

    /// The reader's verdict on a listing whose only entry is `entry`.
    fn read_one(entry: &ListingEntry) -> Result<Vec<ListingEntry>, ListingError> {
        let text = format!("{{\"format\":1}}\n{}\n", line(entry));
        read_all(&frame(&text)[..])
    }

    fn dir_with_a_checksum() -> ListingEntry {
        ListingEntry {
            sha256: Some("ab".repeat(32)),
            ..dir_entry()
        }
    }

    fn file_without_a_checksum() -> ListingEntry {
        ListingEntry {
            sha256: None,
            ..file_entry()
        }
    }

    fn symlink_without_a_target() -> ListingEntry {
        ListingEntry {
            link: None,
            ..symlink_entry()
        }
    }

    fn file_with_an_upper_case_checksum() -> ListingEntry {
        ListingEntry {
            sha256: Some("AB".repeat(32)),
            ..file_entry()
        }
    }

    fn the_container_itself() -> ListingEntry {
        ListingEntry {
            path: RelPath::root(),
            ..dir_entry()
        }
    }

    fn assert_refused_on_read(entry: &ListingEntry) {
        let result = read_one(entry);
        assert!(
            matches!(result, Err(ListingError::Line { number: 2, .. })),
            "{result:?}"
        );
    }

    fn assert_refused_on_write(entry: &ListingEntry) {
        let result = write([entry], &mut Vec::new());
        assert!(
            matches!(result, Err(ListingError::Entry { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn the_reader_refuses_a_directory_with_a_checksum() {
        assert_refused_on_read(&dir_with_a_checksum());
    }

    #[test]
    fn the_reader_refuses_a_file_without_a_checksum() {
        assert_refused_on_read(&file_without_a_checksum());
    }

    #[test]
    fn the_reader_refuses_a_symlink_without_a_target() {
        assert_refused_on_read(&symlink_without_a_target());
    }

    #[test]
    fn the_reader_refuses_an_upper_case_checksum() {
        assert_refused_on_read(&file_with_an_upper_case_checksum());
    }

    #[test]
    fn the_reader_refuses_the_container_itself() {
        assert_refused_on_read(&the_container_itself());
    }

    #[test]
    fn the_writer_refuses_a_directory_with_a_checksum() {
        assert_refused_on_write(&dir_with_a_checksum());
    }

    #[test]
    fn the_writer_refuses_a_file_without_a_checksum() {
        assert_refused_on_write(&file_without_a_checksum());
    }

    #[test]
    fn the_writer_refuses_a_symlink_without_a_target() {
        assert_refused_on_write(&symlink_without_a_target());
    }

    #[test]
    fn the_writer_refuses_an_upper_case_checksum() {
        assert_refused_on_write(&file_with_an_upper_case_checksum());
    }

    #[test]
    fn the_writer_refuses_the_container_itself() {
        assert_refused_on_write(&the_container_itself());
    }

    #[test]
    fn the_writer_refuses_a_file_with_a_link() {
        let entry = ListingEntry {
            link: Some(LinkTarget::from_bytes(b"x".to_vec())),
            ..file_entry()
        };
        assert_refused_on_write(&entry);
    }

    /// A sink that fails where it is told to.
    struct Failing {
        on_write: bool,
    }

    impl Write for Failing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.on_write {
                return Err(io::Error::other("the disk is full"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("the flush failed"))
        }
    }

    #[test]
    fn a_sink_that_fails_to_write_is_an_error() {
        let mut sink = Failing { on_write: true };
        let result = write(&all_entries(), &mut sink);
        assert!(matches!(result, Err(ListingError::Io { .. })), "{result:?}");
    }

    #[test]
    fn a_sink_that_fails_to_flush_is_an_error() {
        let mut sink = Failing { on_write: false };
        let result = write(&all_entries(), &mut sink);
        assert!(matches!(result, Err(ListingError::Io { .. })), "{result:?}");
    }

    const PAYLOAD_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_listing_is_named_after_the_first_16_digits_of_its_payload() {
        let name = file_name(PAYLOAD_SHA256).expect("a digest");
        assert_eq!(name.as_str(), "0123456789abcdef.listing.zst");
        assert!(crate::meta::is_listing_file_name(name.as_str()));
    }

    #[test]
    fn a_short_digest_names_no_listing() {
        assert!(matches!(
            file_name("0123456789abcdef"),
            Err(ListingError::InvalidDigest { .. })
        ));
    }

    #[test]
    fn an_upper_case_digest_names_no_listing() {
        assert!(matches!(
            file_name(&PAYLOAD_SHA256.to_uppercase()),
            Err(ListingError::InvalidDigest { .. })
        ));
    }

    #[test]
    fn a_digest_with_a_multi_byte_character_names_no_listing() {
        let digest = format!("{}é", &PAYLOAD_SHA256[..62]);
        assert_eq!(digest.len(), 64);
        assert!(matches!(
            file_name(&digest),
            Err(ListingError::InvalidDigest { .. })
        ));
    }

    #[test]
    fn a_digest_of_65_digits_names_no_listing() {
        assert!(matches!(
            file_name(&format!("{PAYLOAD_SHA256}0")),
            Err(ListingError::InvalidDigest { .. })
        ));
    }
}
