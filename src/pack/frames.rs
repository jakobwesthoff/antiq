// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inspecting zstd frames.

use super::hashing::HashingReader;
use std::fmt;
use std::io::{self, BufReader, Read};

/// The compression format a file starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Zstd,
    Gzip,
    Xz,
    Bzip2,
    Lz4,
    Lzip,
    Tar,
    Unknown,
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Codec::Zstd => "zstd",
            Codec::Gzip => "gzip",
            Codec::Xz => "xz",
            Codec::Bzip2 => "bzip2",
            Codec::Lz4 => "lz4",
            Codec::Lzip => "lzip",
            Codec::Tar => "an uncompressed tar",
            Codec::Unknown => "of a kind antiq does not know",
        })
    }
}

/// The first bytes of a zstd data frame.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Skippable frames start with `0x184D2A50` to `0x184D2A5F`.
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;

/// How many bytes of the start of a file tell the codecs apart: a tar
/// header carries its `ustar` magic at offset 257.
const PREFIX_LEN: usize = 262;

/// The largest window antiq's decoders accept, 128 MiB (ADR 0051).
pub const MAX_WINDOW: u64 = 1 << 27;

/// A block holds at most 128 KiB, or the window if that is smaller.
const BLOCK_SIZE_LIMIT: u64 = 128 * 1024;

/// The codec whose magic bytes `prefix` starts with.
pub fn codec_of(prefix: &[u8]) -> Codec {
    let starts = |magic: &[u8]| prefix.starts_with(magic);
    if starts(&ZSTD_MAGIC) || is_skippable(prefix) {
        Codec::Zstd
    } else if starts(&[0x1F, 0x8B]) {
        Codec::Gzip
    } else if starts(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Codec::Xz
    } else if starts(b"BZh") {
        Codec::Bzip2
    } else if starts(&[0x04, 0x22, 0x4D, 0x18]) {
        Codec::Lz4
    } else if starts(b"LZIP") {
        Codec::Lzip
    } else if prefix.get(257..262) == Some(b"ustar") {
        Codec::Tar
    } else {
        Codec::Unknown
    }
}

fn is_skippable(prefix: &[u8]) -> bool {
    prefix
        .first_chunk::<4>()
        .is_some_and(|magic| u32::from_le_bytes(*magic) & SKIPPABLE_MASK == SKIPPABLE_MAGIC)
}

/// What walking a file's frames found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    /// Bytes in the file.
    pub size: u64,
    /// SHA-256 of the file as lower-case hex.
    pub sha256: String,
    /// Zstd data frames; skippable frames are not counted.
    pub frames: usize,
    /// The largest window any frame declares, in bytes.
    pub largest_window: u64,
}

/// Why a file is not a stream of acceptable zstd frames. Frames are
/// numbered from 1 in file order, skippable frames included.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("the file is empty")]
    Empty,
    #[error("the file is not zstd but {0}")]
    NotZstd(Codec),
    #[error("the file holds skippable frames only and no zstd frame")]
    NoFrames,
    #[error(
        "frame {frame} has a window of {window} bytes, above the 128 MiB antiq reads; recompress it with `zstd -dc <file> | zstd -19 --long=27 -o <new>.tar.zst`"
    )]
    Window { frame: usize, window: u64 },
    #[error(
        "frame {frame} has no content checksum (the macOS `tar --zstd` writes none); recompress it with `zstd -dc <file> | zstd -19 --long=27 -o <new>.tar.zst`"
    )]
    NoChecksum { frame: usize },
    #[error("frame {frame} sets the reserved bit of its header")]
    ReservedBit { frame: usize },
    #[error("frame {frame} needs a dictionary, which antiq does not have")]
    Dictionary { frame: usize },
    #[error("frame {frame} has a block of the reserved type")]
    ReservedBlockType { frame: usize },
    #[error("frame {frame} has a block of {size} bytes, above the {max} its window allows")]
    BlockTooLarge {
        frame: usize,
        size: usize,
        max: usize,
    },
    #[error("frame {frame} ends inside its {what}")]
    Truncated { frame: usize, what: &'static str },
    #[error("the bytes at offset {offset} after the last frame are not a frame")]
    TrailingData { offset: u64 },
    #[error("cannot read the file")]
    Io(#[from] io::Error),
}

/// Reads up to `buf.len()` bytes, fewer only at the end of the input.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Reads exactly `buf.len()` bytes of `frame`'s `what`.
fn exact(
    reader: &mut impl Read,
    buf: &mut [u8],
    frame: usize,
    what: &'static str,
) -> Result<(), FrameError> {
    if fill(reader, buf)? < buf.len() {
        return Err(FrameError::Truncated { frame, what });
    }
    Ok(())
}

/// Reads and drops `count` bytes of `frame`'s `what`, through a fixed
/// buffer, so a frame of any size takes no more memory than a small one.
fn skip(
    reader: &mut impl Read,
    count: u64,
    frame: usize,
    what: &'static str,
) -> Result<(), FrameError> {
    let copied = io::copy(&mut reader.by_ref().take(count), &mut io::sink())?;
    if copied < count {
        return Err(FrameError::Truncated { frame, what });
    }
    Ok(())
}

/// Walks the frames of `input` over their headers and block headers
/// (RFC 8878, sections 3.1.1 and 3.1.1.2), reading each byte once and
/// holding none of them.
///
/// Every data frame must declare a window of at most [`MAX_WINDOW`] and
/// carry the content checksum. The checksum value and the compressed
/// blocks are not examined; decoding the stream checks them.
pub fn walk<R: Read>(input: R) -> Result<Walked, FrameError> {
    let mut reader = HashingReader::new(BufReader::with_capacity(256 * 1024, input));
    let mut seen = 0;
    let mut frames = 0;
    let mut largest_window = 0;
    loop {
        let mut magic = [0u8; 4];
        let got = fill(&mut reader, &mut magic)?;
        if got == 0 && seen > 0 {
            break;
        }
        if got == 0 {
            return Err(FrameError::Empty);
        }
        let offset = reader.bytes_read() - got as u64;
        let is_zstd = got == 4 && magic == ZSTD_MAGIC;
        let is_skippable = got == 4 && is_skippable(&magic);
        if !is_zstd && !is_skippable {
            if seen > 0 {
                return Err(FrameError::TrailingData { offset });
            }
            let mut prefix = [0u8; PREFIX_LEN];
            prefix[..got].copy_from_slice(&magic[..got]);
            let more = if got == 4 {
                fill(&mut reader, &mut prefix[4..])?
            } else {
                0
            };
            return Err(FrameError::NotZstd(codec_of(&prefix[..got + more])));
        }
        seen += 1;
        if is_skippable {
            let mut size = [0u8; 4];
            exact(&mut reader, &mut size, seen, "header")?;
            skip(
                &mut reader,
                u64::from(u32::from_le_bytes(size)),
                seen,
                "content",
            )?;
            continue;
        }
        frames += 1;
        largest_window = largest_window.max(walk_data_frame(&mut reader, seen)?);
    }
    if frames == 0 {
        return Err(FrameError::NoFrames);
    }
    Ok(Walked {
        size: reader.bytes_read(),
        sha256: reader.sha256(),
        frames,
        largest_window,
    })
}

/// Walks one data frame after its magic and returns its window.
fn walk_data_frame(reader: &mut impl Read, frame: usize) -> Result<u64, FrameError> {
    let mut descriptor = [0u8; 1];
    exact(reader, &mut descriptor, frame, "header")?;
    let descriptor = descriptor[0];
    let content_size_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    let reserved = descriptor & 0x08 != 0;
    let checksum = descriptor & 0x04 != 0;
    let dictionary_width = [0, 1, 2, 4][usize::from(descriptor & 0x03)];
    if reserved {
        return Err(FrameError::ReservedBit { frame });
    }

    let mut window_descriptor = [0u8; 1];
    if !single_segment {
        exact(reader, &mut window_descriptor, frame, "header")?;
    }
    let mut dictionary = [0u8; 4];
    exact(reader, &mut dictionary[..dictionary_width], frame, "header")?;
    let content_size_width = match content_size_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let mut content_size = [0u8; 8];
    exact(
        reader,
        &mut content_size[..content_size_width],
        frame,
        "header",
    )?;

    // A single segment has no window of its own: the content is the window.
    let window = if single_segment {
        let size = u64::from_le_bytes(content_size);
        if content_size_flag == 1 {
            size + 256
        } else {
            size
        }
    } else {
        let exponent = u32::from(window_descriptor[0] >> 3);
        let mantissa = u64::from(window_descriptor[0] & 0x07);
        let base = 1u64 << (10 + exponent);
        base + base / 8 * mantissa
    };
    if window > MAX_WINDOW {
        return Err(FrameError::Window { frame, window });
    }
    if dictionary.iter().any(|byte| *byte != 0) {
        return Err(FrameError::Dictionary { frame });
    }
    if !checksum {
        return Err(FrameError::NoChecksum { frame });
    }

    let block_max = window.min(BLOCK_SIZE_LIMIT) as usize;
    loop {
        let mut header = [0u8; 4];
        exact(reader, &mut header[..3], frame, "block header")?;
        let word = u32::from_le_bytes(header);
        let last = word & 1 != 0;
        let kind = (word >> 1) & 3;
        let size = (word >> 3) as usize;
        if kind == 3 {
            return Err(FrameError::ReservedBlockType { frame });
        }
        if size > block_max {
            return Err(FrameError::BlockTooLarge {
                frame,
                size,
                max: block_max,
            });
        }
        // An RLE block stores its byte once, whatever size it stands for.
        let stored = if kind == 1 { 1 } else { size as u64 };
        skip(reader, stored, frame, "block")?;
        if last {
            break;
        }
    }
    let mut checksum_bytes = [0u8; 4];
    exact(reader, &mut checksum_bytes, frame, "content checksum")?;
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::zstd::{ZstdSettings, encoder};
    use crate::testing::package::{RawMember, compress, raw_tar};
    use std::io::Write;

    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    const CHECKSUM: u8 = 0x04;
    const SINGLE_SEGMENT: u8 = 0x20;
    const MIB: u64 = 1 << 20;

    /// A raw block of `size` zero bytes.
    fn raw(size: usize, last: bool) -> Vec<u8> {
        let mut block = block_header(size, 0, last);
        block.resize(3 + size, 0);
        block
    }

    /// An RLE block that stands for `size` bytes.
    fn rle(size: usize, last: bool) -> Vec<u8> {
        let mut block = block_header(size, 1, last);
        block.push(0xAA);
        block
    }

    fn block_header(size: usize, kind: u32, last: bool) -> Vec<u8> {
        let word = ((size as u32) << 3) | (kind << 1) | u32::from(last);
        word.to_le_bytes()[..3].to_vec()
    }

    /// A frame with the checksum flag, a window descriptor of `window`
    /// and `blocks`, ending in four checksum bytes the walker does not
    /// compute.
    fn frame_with_window(window: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(CHECKSUM);
        bytes.push(window);
        for block in blocks {
            bytes.extend_from_slice(block);
        }
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        bytes
    }

    /// The window descriptor byte for `2^log` plus `mantissa` eighths.
    fn window_byte(log: u32, mantissa: u8) -> u8 {
        (((log - 10) as u8) << 3) | mantissa
    }

    fn small_frame() -> Vec<u8> {
        frame_with_window(window_byte(17, 0), &[raw(10, true)])
    }

    /// A frame with a custom descriptor, then `fields` (window, dictionary
    /// ID and content size as that descriptor needs), then the blocks.
    fn frame_with(descriptor: u8, fields: &[u8], blocks: &[Vec<u8>], checksum: bool) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(descriptor);
        bytes.extend_from_slice(fields);
        for block in blocks {
            bytes.extend_from_slice(block);
        }
        if checksum {
            bytes.extend_from_slice(&[1, 2, 3, 4]);
        }
        bytes
    }

    fn refused(bytes: &[u8]) -> FrameError {
        walk(bytes).expect_err("the walk refuses the stream")
    }

    /// A frame the zstd crate wrote for `content`.
    fn crate_frame(settings: &ZstdSettings, content: &[u8]) -> Vec<u8> {
        let mut encoder = encoder(Vec::new(), settings).expect("an encoder");
        encoder.write_all(content).expect("a vec accepts bytes");
        encoder.finish().expect("the frame ends")
    }

    fn sha256_of(bytes: &[u8]) -> String {
        let mut reader = crate::pack::hashing::HashingReader::new(bytes);
        io::copy(&mut reader, &mut io::sink()).expect("hashes");
        reader.sha256()
    }

    // ---------------------------------------------------------
    // Frames the crate writes
    // ---------------------------------------------------------

    #[test]
    fn a_crate_made_package_walks_to_its_size_digest_and_one_frame() {
        let package = compress(&raw_tar(&[RawMember::dir(b"proj/")]));
        let walked = walk(&package[..]).expect("the package walks");
        assert_eq!(walked.size, package.len() as u64);
        assert_eq!(walked.sha256, sha256_of(&package));
        assert_eq!(walked.frames, 1);
        assert!((1024..=1 << 27).contains(&walked.largest_window));
    }

    #[test]
    fn concatenated_frames_are_counted_and_their_sizes_add_up() {
        let first = compress(&raw_tar(&[RawMember::dir(b"proj/")]));
        let second = compress(&raw_tar(&[RawMember::dir(b"other/")]));
        let both = [first.clone(), second.clone()].concat();
        let walked = walk(&both[..]).expect("both frames walk");
        assert_eq!(walked.frames, 2);
        assert_eq!(walked.size, (first.len() + second.len()) as u64);
    }

    #[test]
    fn each_frame_size_agrees_with_libzsts_find_frame_compressed_size() {
        let content: Vec<u8> = (0..3_000_000u32)
            .flat_map(|n| (n / 7).to_le_bytes())
            .collect();
        for settings in [
            ZstdSettings::live(1),
            ZstdSettings::live(3),
            ZstdSettings::archive(1),
        ] {
            let frame = crate_frame(&settings, &content[..200_000]);
            let expected = ::zstd::zstd_safe::find_frame_compressed_size(&frame)
                .expect("libzstd sizes its own frame");
            let walked = walk(&frame[..]).expect("the frame walks");
            assert_eq!(walked.size, expected as u64, "{settings:?}");
            assert_eq!(walked.frames, 1);
        }
    }

    #[test]
    fn a_frame_with_a_pledged_size_is_single_segment_and_walks() {
        // Multithreaded compression never writes a single-segment frame.
        let mut encoder = ::zstd::stream::write::Encoder::new(Vec::new(), 1).expect("an encoder");
        encoder.include_checksum(true).expect("checksums can be on");
        encoder
            .set_pledged_src_size(Some(100_000))
            .expect("a size can be pledged");
        encoder
            .write_all(&vec![0u8; 100_000])
            .expect("a vec accepts bytes");
        let frame = encoder.finish().expect("the frame ends");
        assert_ne!(frame[4] & SINGLE_SEGMENT, 0, "a single-segment frame");
        let walked = walk(&frame[..]).expect("the frame walks");
        assert_eq!(walked.largest_window, 100_000);
    }

    // ---------------------------------------------------------
    // Magic bytes
    // ---------------------------------------------------------

    fn codec_error(prefix: &[u8]) -> Codec {
        match refused(prefix) {
            FrameError::NotZstd(codec) => codec,
            other => panic!("expected NotZstd, got {other:?}"),
        }
    }

    #[test]
    fn gzip_xz_bzip2_lz4_lzip_and_plain_tar_are_named_by_their_magic() {
        assert_eq!(codec_error(&[0x1F, 0x8B, 8, 0, 0, 0]), Codec::Gzip);
        assert_eq!(
            codec_error(&[0xFD, b'7', b'z', b'X', b'Z', 0, 0, 4]),
            Codec::Xz
        );
        assert_eq!(codec_error(b"BZh91AY&SY"), Codec::Bzip2);
        assert_eq!(codec_error(&[0x04, 0x22, 0x4D, 0x18, 0x64]), Codec::Lz4);
        assert_eq!(codec_error(b"LZIP\x01\x0c"), Codec::Lzip);
        let tar = raw_tar(&[RawMember::dir(b"proj/")]);
        assert_eq!(codec_error(&tar), Codec::Tar);
    }

    #[test]
    fn an_unknown_prefix_is_not_zstd() {
        assert_eq!(codec_error(b"this is plain text"), Codec::Unknown);
    }

    #[test]
    fn a_stream_shorter_than_a_magic_is_not_zstd() {
        assert_eq!(codec_error(&[0x28, 0xB5]), Codec::Unknown);
    }

    #[test]
    fn an_empty_input_is_empty() {
        assert!(matches!(refused(b""), FrameError::Empty));
    }

    #[test]
    fn codec_of_reads_only_the_magic() {
        assert_eq!(codec_of(&MAGIC), Codec::Zstd);
        assert_eq!(codec_of(&[0x50, 0x2A, 0x4D, 0x18]), Codec::Zstd);
    }

    // ---------------------------------------------------------
    // Windows
    // ---------------------------------------------------------

    fn walks_with_window(window: u8) -> Result<Walked, FrameError> {
        walk(&frame_with_window(window, &[raw(10, true)])[..])
    }

    #[test]
    fn a_window_of_exactly_128_mib_passes() {
        let walked = walks_with_window(window_byte(27, 0)).expect("2^27 is allowed");
        assert_eq!(walked.largest_window, 1 << 27);
    }

    #[test]
    fn a_window_log_of_28_is_refused() {
        match walks_with_window(window_byte(28, 0)) {
            Err(FrameError::Window { frame: 1, window }) => assert_eq!(window, 1 << 28),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_of_128_mib_plus_one_mantissa_step_is_refused() {
        match walks_with_window(window_byte(27, 1)) {
            Err(FrameError::Window { window, .. }) => assert_eq!(window, (1 << 27) + (1 << 24)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_window_log_of_26_with_the_largest_mantissa_passes() {
        let walked = walks_with_window(window_byte(26, 7)).expect("below 2^27");
        assert_eq!(walked.largest_window, (1 << 26) + 7 * (1 << 23));
    }

    /// A single-segment frame of content size `size` in a four-byte field.
    fn single_segment(size: u32) -> Vec<u8> {
        frame_with(
            CHECKSUM | SINGLE_SEGMENT | (2 << 6),
            &size.to_le_bytes(),
            &[raw(10, true)],
            true,
        )
    }

    #[test]
    fn a_single_segment_frame_of_exactly_128_mib_passes() {
        let walked = walk(&single_segment(1 << 27)[..]).expect("2^27 is allowed");
        assert_eq!(walked.largest_window, 1 << 27);
    }

    #[test]
    fn a_single_segment_frame_one_byte_over_is_refused() {
        match refused(&single_segment((1 << 27) + 1)) {
            FrameError::Window { window, .. } => assert_eq!(window, (1 << 27) + 1),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_single_segment_size_field_of_two_bytes_counts_from_256() {
        let frame = |size: u16, block: usize| {
            frame_with(
                CHECKSUM | SINGLE_SEGMENT | (1 << 6),
                &size.to_le_bytes(),
                &[raw(block, true)],
                true,
            )
        };
        let walked = walk(&frame(0, 256)[..]).expect("a window of 256 holds a block of 256");
        assert_eq!(walked.largest_window, 256);
        assert!(matches!(
            refused(&frame(0, 257)),
            FrameError::BlockTooLarge { max: 256, .. }
        ));
    }

    #[test]
    fn the_second_frames_window_is_judged_too() {
        let bytes = [
            small_frame(),
            frame_with_window(window_byte(28, 0), &[raw(10, true)]),
        ]
        .concat();
        assert!(matches!(
            refused(&bytes),
            FrameError::Window { frame: 2, .. }
        ));
    }

    #[test]
    fn the_window_message_names_the_repair_command() {
        let message = refused(&frame_with_window(window_byte(28, 0), &[raw(10, true)])).to_string();
        assert!(
            message.contains("zstd -dc <file> | zstd -19 --long=27 -o <new>.tar.zst"),
            "{message}"
        );
    }

    // ---------------------------------------------------------
    // Header fields
    // ---------------------------------------------------------

    #[test]
    fn each_content_size_flag_is_read_with_its_width() {
        let window = window_byte(17, 0);
        let sized = |flag: u8, field: &[u8]| {
            let mut fields = vec![window];
            fields.extend_from_slice(field);
            frame_with(CHECKSUM | (flag << 6), &fields, &[raw(10, true)], true)
        };
        for (flag, field) in [(1, vec![0; 2]), (2, vec![0; 4]), (3, vec![0; 8])] {
            walk(&sized(flag, &field)[..]).unwrap_or_else(|error| panic!("flag {flag}: {error}"));
        }
        // A single-segment frame with flag 0 has a one-byte size field.
        let one_byte = frame_with(CHECKSUM | SINGLE_SEGMENT, &[10], &[raw(10, true)], true);
        walk(&one_byte[..]).expect("flag 0 with a single segment has one byte");
    }

    #[test]
    fn each_dictionary_id_width_is_read_and_a_non_zero_id_is_refused() {
        let window = window_byte(17, 0);
        for (flag, width) in [(1u8, 1usize), (2, 2), (3, 4)] {
            let mut fields = vec![window];
            let mut id = vec![0u8; width];
            id[0] = 7;
            fields.extend_from_slice(&id);
            let bytes = frame_with(CHECKSUM | flag, &fields, &[raw(10, true)], true);
            assert!(
                matches!(refused(&bytes), FrameError::Dictionary { frame: 1 }),
                "width {width}"
            );
        }
    }

    #[test]
    fn a_zero_dictionary_id_passes() {
        let window = window_byte(17, 0);
        for (flag, width) in [(1u8, 1usize), (2, 2), (3, 4)] {
            let mut fields = vec![window];
            fields.extend(vec![0u8; width]);
            let bytes = frame_with(CHECKSUM | flag, &fields, &[raw(10, true)], true);
            walk(&bytes[..]).unwrap_or_else(|error| panic!("width {width}: {error}"));
        }
    }

    #[test]
    fn a_set_reserved_bit_is_refused() {
        let bytes = frame_with(
            CHECKSUM | 0x08,
            &[window_byte(17, 0)],
            &[raw(10, true)],
            true,
        );
        assert!(matches!(
            refused(&bytes),
            FrameError::ReservedBit { frame: 1 }
        ));
    }

    #[test]
    fn a_set_unused_bit_is_ignored() {
        let bytes = frame_with(
            CHECKSUM | 0x10,
            &[window_byte(17, 0)],
            &[raw(10, true)],
            true,
        );
        walk(&bytes[..]).expect("bit 4 is not interpreted");
    }

    // ---------------------------------------------------------
    // The content checksum
    // ---------------------------------------------------------

    #[test]
    fn a_frame_without_the_checksum_flag_is_refused() {
        let bytes = frame_with(0, &[window_byte(17, 0)], &[raw(10, true)], false);
        assert!(matches!(
            refused(&bytes),
            FrameError::NoChecksum { frame: 1 }
        ));
    }

    #[test]
    fn a_second_frame_without_the_flag_is_refused() {
        let unchecked = frame_with(0, &[window_byte(17, 0)], &[raw(10, true)], false);
        let bytes = [small_frame(), unchecked].concat();
        assert!(matches!(
            refused(&bytes),
            FrameError::NoChecksum { frame: 2 }
        ));
    }

    #[test]
    fn a_window_and_a_missing_checksum_name_the_window() {
        let bytes = frame_with(0, &[window_byte(28, 0)], &[raw(10, true)], false);
        assert!(matches!(refused(&bytes), FrameError::Window { .. }));
    }

    #[test]
    fn the_checksum_message_names_the_repair_command() {
        let bytes = frame_with(0, &[window_byte(17, 0)], &[raw(10, true)], false);
        let message = refused(&bytes).to_string();
        assert!(
            message.contains("zstd -dc <file> | zstd -19 --long=27 -o <new>.tar.zst"),
            "{message}"
        );
    }

    // ---------------------------------------------------------
    // Blocks
    // ---------------------------------------------------------

    #[test]
    fn a_reserved_block_type_is_refused() {
        let mut block = block_header(0, 3, true);
        block.extend_from_slice(&[]);
        let bytes = frame_with_window(window_byte(17, 0), &[block]);
        assert!(matches!(
            refused(&bytes),
            FrameError::ReservedBlockType { frame: 1 }
        ));
    }

    #[test]
    fn a_raw_block_larger_than_the_block_maximum_is_refused() {
        let bytes = frame_with_window(window_byte(20, 0), &[raw(128 * 1024 + 1, true)]);
        match refused(&bytes) {
            FrameError::BlockTooLarge { size, max, .. } => {
                assert_eq!((size, max), (128 * 1024 + 1, 128 * 1024));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_raw_block_of_exactly_the_block_maximum_passes() {
        let bytes = frame_with_window(window_byte(20, 0), &[raw(128 * 1024, true)]);
        walk(&bytes[..]).expect("the maximum itself is allowed");
    }

    #[test]
    fn the_block_maximum_is_the_window_when_it_is_smaller_than_128_kib() {
        let at = frame_with_window(window_byte(10, 0), &[raw(1024, true)]);
        walk(&at[..]).expect("a block as large as the window passes");
        let over = frame_with_window(window_byte(10, 0), &[raw(1025, true)]);
        assert!(matches!(
            refused(&over),
            FrameError::BlockTooLarge { max: 1024, .. }
        ));
    }

    #[test]
    fn an_rle_block_carries_one_byte_whatever_its_size() {
        let bytes = frame_with_window(window_byte(17, 0), &[rle(100_000, false), raw(3, true)]);
        let walked = walk(&bytes[..]).expect("the RLE block holds one byte");
        assert_eq!(walked.size, bytes.len() as u64);
    }

    #[test]
    fn an_rle_block_over_the_maximum_is_refused() {
        let bytes = frame_with_window(window_byte(17, 0), &[rle(128 * 1024 + 1, true)]);
        assert!(matches!(
            refused(&bytes),
            FrameError::BlockTooLarge { size, .. } if size == 128 * 1024 + 1
        ));
    }

    // ---------------------------------------------------------
    // Cut and trailing streams
    // ---------------------------------------------------------

    #[test]
    fn a_stream_cut_at_any_offset_is_refused() {
        let bytes = frame_with_window(
            window_byte(17, 0),
            &[raw(100, false), rle(50, false), raw(7, true)],
        );
        walk(&bytes[..]).expect("the whole stream walks");
        for length in 0..bytes.len() {
            assert!(
                walk(&bytes[..length]).is_err(),
                "a stream cut at {length} of {} was accepted",
                bytes.len()
            );
        }
    }

    #[test]
    fn a_header_cut_inside_its_optional_fields_is_truncated() {
        let bytes = frame_with(
            CHECKSUM | (3 << 6) | 3,
            &[window_byte(17, 0), 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0],
            &[raw(10, true)],
            true,
        );
        walk(&bytes[..]).expect("the whole frame walks");
        for length in 4..4 + 14 {
            assert!(
                matches!(
                    walk(&bytes[..length]),
                    Err(FrameError::Truncated { what: "header", .. })
                ),
                "cut at {length}"
            );
        }
    }

    /// Gives its bytes one at a time, failing as `failure` says after the
    /// first.
    struct Flaky {
        bytes: Vec<u8>,
        position: usize,
        failure: Option<io::ErrorKind>,
    }

    impl Read for Flaky {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.position == 1
                && let Some(kind) = self.failure.take()
            {
                return Err(kind.into());
            }
            let Some(byte) = self.bytes.get(self.position) else {
                return Ok(0);
            };
            buf[0] = *byte;
            self.position += 1;
            Ok(1)
        }
    }

    #[test]
    fn an_interrupted_read_is_tried_again() {
        let flaky = Flaky {
            bytes: small_frame(),
            position: 0,
            failure: Some(io::ErrorKind::Interrupted),
        };
        assert_eq!(walk(flaky).expect("the walk goes on").frames, 1);
    }

    #[test]
    fn a_failing_read_is_an_io_error() {
        let flaky = Flaky {
            bytes: small_frame(),
            position: 0,
            failure: Some(io::ErrorKind::PermissionDenied),
        };
        match walk(flaky) {
            Err(FrameError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_refusal_says_what_the_file_is() {
        let message = |bytes: &[u8]| refused(bytes).to_string();
        assert_eq!(message(&[0x1F, 0x8B, 8]), "the file is not zstd but gzip");
        assert_eq!(
            message(&[0xFD, b'7', b'z', b'X', b'Z', 0]),
            "the file is not zstd but xz"
        );
        assert_eq!(message(b"BZh9"), "the file is not zstd but bzip2");
        assert_eq!(
            message(&[4, 0x22, 0x4D, 0x18]),
            "the file is not zstd but lz4"
        );
        assert_eq!(message(b"LZIP"), "the file is not zstd but lzip");
        assert_eq!(
            message(&raw_tar(&[RawMember::dir(b"proj/")])),
            "the file is not zstd but an uncompressed tar"
        );
        assert_eq!(
            message(b"plain text"),
            "the file is not zstd but of a kind antiq does not know"
        );
        assert_eq!(Codec::Zstd.to_string(), "zstd");
    }

    #[test]
    fn empty_blocks_that_never_end_the_frame_end_with_the_input() {
        // Every block is a header of its own and none is the last, so only
        // the end of the input stops the walk.
        let mut bytes = MAGIC.to_vec();
        bytes.push(CHECKSUM);
        bytes.push(window_byte(17, 0));
        bytes.extend(std::iter::repeat_n(0u8, 3 * 100_000));
        assert!(matches!(
            refused(&bytes),
            FrameError::Truncated {
                what: "block header",
                ..
            }
        ));
    }

    #[test]
    fn a_frame_cut_before_its_checksum_is_truncated() {
        let bytes = small_frame();
        match refused(&bytes[..bytes.len() - 1]) {
            FrameError::Truncated { frame: 1, .. } => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_to_three_bytes_after_a_frame_are_trailing_data() {
        let frame = small_frame();
        for extra in 1..=3usize {
            let mut bytes = frame.clone();
            bytes.extend(vec![0x28; extra]);
            match refused(&bytes) {
                FrameError::TrailingData { offset } => assert_eq!(offset, frame.len() as u64),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn zeros_after_a_frame_are_trailing_data() {
        let mut bytes = small_frame();
        bytes.extend_from_slice(&[0; 16]);
        assert!(matches!(refused(&bytes), FrameError::TrailingData { .. }));
    }

    #[test]
    fn a_stream_of_garbage_after_a_frame_is_trailing_data() {
        let mut bytes = small_frame();
        bytes.extend_from_slice(b"garbage after the frame");
        assert!(matches!(refused(&bytes), FrameError::TrailingData { .. }));
    }

    // ---------------------------------------------------------
    // Skippable frames
    // ---------------------------------------------------------

    fn skippable(variant: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = (0x184D2A50u32 + u32::from(variant)).to_le_bytes().to_vec();
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn a_skippable_frame_between_frames_is_walked_over() {
        for variant in [0, 15] {
            let bytes = [
                skippable(variant, b"size hints"),
                small_frame(),
                skippable(variant, b""),
                small_frame(),
            ]
            .concat();
            let walked = walk(&bytes[..]).expect("skippable frames are walked over");
            assert_eq!(walked.frames, 2);
            assert_eq!(walked.size, bytes.len() as u64);
        }
    }

    #[test]
    fn a_cut_skippable_frame_is_truncated() {
        let mut bytes = small_frame();
        let mut cut = skippable(0, b"0123456789");
        cut.truncate(cut.len() - 1);
        bytes.extend_from_slice(&cut);
        assert!(matches!(refused(&bytes), FrameError::Truncated { .. }));
    }

    #[test]
    fn skippable_frames_alone_are_no_frames() {
        assert!(matches!(
            refused(&skippable(0, b"hint")),
            FrameError::NoFrames
        ));
    }

    // ---------------------------------------------------------
    // Memory
    // ---------------------------------------------------------

    /// A frame of 512 raw blocks of 128 KiB that is generated while it is
    /// read, recording the largest buffer it was asked to fill.
    struct Blocks {
        pending: Vec<u8>,
        index: u32,
        left_in_block: usize,
        largest_request: usize,
    }

    impl Read for Blocks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.largest_request = self.largest_request.max(buf.len());
            if self.pending.is_empty() && self.left_in_block == 0 {
                if self.index == 512 {
                    self.pending = vec![1, 2, 3, 4];
                    self.index += 1;
                } else if self.index == 513 {
                    return Ok(0);
                } else {
                    self.pending = block_header(128 * 1024, 0, self.index == 511);
                    self.index += 1;
                    self.left_in_block = 128 * 1024;
                }
            }
            if !self.pending.is_empty() {
                let count = self.pending.len().min(buf.len());
                buf[..count].copy_from_slice(&self.pending[..count]);
                self.pending.drain(..count);
                return Ok(count);
            }
            let count = self.left_in_block.min(buf.len());
            buf[..count].fill(0);
            self.left_in_block -= count;
            Ok(count)
        }
    }

    #[test]
    fn a_frame_of_64_mib_is_read_in_small_pieces() {
        let mut pending = MAGIC.to_vec();
        pending.push(CHECKSUM);
        pending.push(window_byte(27, 0));
        let mut reader = Blocks {
            pending,
            index: 0,
            left_in_block: 0,
            largest_request: 0,
        };
        let walked = walk(&mut reader).expect("a 64 MiB frame walks");
        assert_eq!(walked.frames, 1);
        assert!(walked.size > 64 * MIB);
        assert!(
            reader.largest_request <= MIB as usize,
            "a read asked for {} bytes",
            reader.largest_request
        );
    }

    // ---------------------------------------------------------
    // Agreement with the decoder
    // ---------------------------------------------------------

    fn decodes(bytes: &[u8]) -> bool {
        let Ok(mut decoder) = crate::pack::zstd::decoder(bytes) else {
            return false;
        };
        io::copy(&mut decoder, &mut io::sink()).is_ok()
    }

    #[test]
    fn frames_the_walker_accepts_decode_with_the_antiq_decoder() {
        let content = b"hello";
        let base = crate_frame(&ZstdSettings::live(1), content);
        assert_eq!(
            base[4], CHECKSUM,
            "the base frame has only the checksum flag"
        );
        let window = base[5];
        let rest = &base[6..];

        let mut variants: Vec<(&str, Vec<u8>)> = vec![("as written", base.clone())];
        let build = |descriptor: u8, fields: &[u8]| {
            let mut bytes = MAGIC.to_vec();
            bytes.push(descriptor);
            bytes.extend_from_slice(fields);
            bytes.extend_from_slice(rest);
            bytes
        };
        variants.push(("unused bit", build(CHECKSUM | 0x10, &[window])));
        variants.push(("window mantissa", build(CHECKSUM, &[window | 7])));
        variants.push((
            "dictionary id of one byte",
            build(CHECKSUM | 1, &[window, 0]),
        ));
        variants.push((
            "dictionary id of four bytes",
            build(CHECKSUM | 3, &[window, 0, 0, 0, 0]),
        ));
        variants.push((
            "content size in four bytes",
            build(CHECKSUM | (2 << 6), &[window, 5, 0, 0, 0]),
        ));
        variants.push((
            "content size in eight bytes",
            build(CHECKSUM | (3 << 6), &[window, 5, 0, 0, 0, 0, 0, 0, 0]),
        ));
        variants.push((
            "skippable frames around it",
            [skippable(3, b"hint"), base.clone(), skippable(9, b"")].concat(),
        ));
        for (name, bytes) in variants {
            assert!(decodes(&bytes), "{name}: the antiq decoder refuses it");
            walk(&bytes[..]).unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }
}
