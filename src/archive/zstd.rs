// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! zstd settings for archives and for tests, and the decoder every reader
//! of antiq uses.

use ::zstd::stream::read::Decoder;
use ::zstd::stream::write::Encoder;
use std::io::{self, BufReader, Read, Write};

/// Archives compress at level 19 (ADR 0051).
pub const ARCHIVE_LEVEL: i32 = 19;

/// The window of an archive is 2^27 bytes, 128 MiB (ADR 0051).
pub const ARCHIVE_WINDOW_LOG: u32 = 27;

/// The fast preset compresses at level 1. It exists for tests and
/// fixtures, which would otherwise start the level-19 encoder and its
/// memory cost for every small package.
pub const FAST_LEVEL: i32 = 1;

/// The largest window any decoder of antiq accepts, 128 MiB. zstd's own
/// default is the same value; setting it states the limit in antiq's code.
pub const DECODER_WINDOW_LOG_MAX: u32 = 27;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    Archive,
    Fast,
}

/// How a stream is compressed: the preset and the number of worker
/// threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZstdSettings {
    preset: Preset,
    workers: u32,
}

impl ZstdSettings {
    /// Level 19 with long-distance matching and a 128 MiB window, at
    /// zstd's default job size.
    pub fn archive(workers: u32) -> ZstdSettings {
        ZstdSettings {
            preset: Preset::Archive,
            workers,
        }
    }

    /// Level 1 with one worker and no long-distance matching.
    pub fn fast() -> ZstdSettings {
        ZstdSettings {
            preset: Preset::Fast,
            workers: 1,
        }
    }
}

/// An encoder that writes a checksummed zstd stream into `out`.
pub fn encoder<W: Write>(out: W, settings: &ZstdSettings) -> io::Result<Encoder<'static, W>> {
    let level = match settings.preset {
        Preset::Archive => ARCHIVE_LEVEL,
        Preset::Fast => FAST_LEVEL,
    };
    let mut encoder = Encoder::new(out, level)?;
    encoder.include_checksum(true)?;
    encoder.multithread(settings.workers.max(1))?;
    match settings.preset {
        Preset::Archive => {
            encoder.long_distance_matching(true)?;
            // Long-distance matching already defaults the window to 2^27;
            // setting it states the size the archives are meant to have.
            encoder.window_log(ARCHIVE_WINDOW_LOG)?;
        }
        Preset::Fast => {}
    }
    Ok(encoder)
}

/// A decoder that refuses windows above 128 MiB.
pub fn decoder<R: Read>(input: R) -> io::Result<Decoder<'static, BufReader<R>>> {
    let mut decoder = Decoder::new(input)?;
    decoder.window_log_max(DECODER_WINDOW_LOG_MAX)?;
    Ok(decoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compressible stream of `len` bytes with enough variety that jobs
    /// differ from each other.
    fn sample(len: usize) -> Vec<u8> {
        let mut state: u32 = 1;
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let word = format!("{:08x} ", state >> 12);
            bytes.extend_from_slice(word.as_bytes());
        }
        bytes.truncate(len);
        bytes
    }

    fn compress(settings: &ZstdSettings, input: &[u8]) -> Vec<u8> {
        let mut encoder = encoder(Vec::new(), settings).expect("an encoder starts");
        encoder.write_all(input).expect("a vec accepts the stream");
        encoder.finish().expect("the frame ends")
    }

    fn decompress(frame: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        decoder(frame)
            .expect("a decoder starts")
            .read_to_end(&mut output)
            .expect("the frame decodes");
        output
    }

    /// The frame header descriptor: bit 2 is the content checksum flag,
    /// bit 5 the single segment flag.
    const CHECKSUM_FLAG: u8 = 0x04;
    const SINGLE_SEGMENT_FLAG: u8 = 0x20;

    #[test]
    fn the_archive_preset_writes_a_checksummed_128_mib_window_that_the_decoder_reads_back() {
        // One level-19 encoder at a time: it takes about 300 MB.
        let input = sample(3000);
        let frame = compress(&ZstdSettings::archive(1), &input);
        let descriptor = frame[4];
        assert_ne!(descriptor & CHECKSUM_FLAG, 0, "the checksum flag is set");
        assert_eq!(
            descriptor & SINGLE_SEGMENT_FLAG,
            0,
            "a stream of unknown size has a window descriptor"
        );
        // Bits 3 to 7 of the window descriptor hold the exponent, which
        // is the window log minus 10.
        assert_eq!(u32::from(frame[5] >> 3) + 10, ARCHIVE_WINDOW_LOG);
        assert_eq!(decompress(&frame), input);
    }

    #[test]
    fn the_decoder_refuses_a_window_above_128_mib() {
        // An empty frame with a window of 2^28 bytes: magic, descriptor
        // without checksum, window descriptor with exponent 18, and one
        // empty last block.
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x90, 0x01, 0x00, 0x00];
        let mut output = Vec::new();
        let error = decoder(&frame[..])
            .expect("a decoder starts")
            .read_to_end(&mut output)
            .expect_err("the window is above the limit");
        assert!(error.to_string().contains("memory"), "{error}");

        // The same frame decodes where the limit allows its window.
        let mut permissive = ::zstd::stream::read::Decoder::new(&frame[..]).expect("starts");
        permissive.window_log_max(28).expect("the limit is valid");
        permissive
            .read_to_end(&mut output)
            .expect("the frame decodes with a limit of 28");
        assert!(output.is_empty());
    }

    #[test]
    fn fast_frames_carry_the_checksum_and_round_trip() {
        let input = sample(100_000);
        let frame = compress(&ZstdSettings::fast(), &input);
        assert_ne!(frame[4] & CHECKSUM_FLAG, 0, "the checksum flag is set");
        assert_eq!(decompress(&frame), input);
    }

    #[test]
    fn fast_is_not_the_archive_preset() {
        let frame = compress(&ZstdSettings::fast(), &sample(3000));
        assert_eq!(
            frame[4] & SINGLE_SEGMENT_FLAG,
            0,
            "a stream of unknown size has a window descriptor"
        );
        let window_log = u32::from(frame[5] >> 3) + 10;
        assert_ne!(
            window_log, ARCHIVE_WINDOW_LOG,
            "the fast preset has no 128 MiB window"
        );
    }
}
