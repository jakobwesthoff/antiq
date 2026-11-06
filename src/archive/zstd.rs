// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! zstd settings for archives and for tests, and the decoder every reader
//! of antiq uses.

use crate::progress::Counter;
use ::zstd::stream::raw::{CParameter, InBuffer, Operation, OutBuffer, WriteBuf};
use ::zstd::stream::read::Decoder;
use ::zstd::stream::zio::Writer;
use ::zstd::zstd_safe::{self, CCtx};
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

/// The compression parameters of a preset, in the order they are set.
fn parameters(settings: &ZstdSettings) -> Vec<CParameter> {
    let level = match settings.preset {
        Preset::Archive => ARCHIVE_LEVEL,
        Preset::Fast => FAST_LEVEL,
    };
    let mut parameters = vec![
        CParameter::CompressionLevel(level),
        CParameter::ChecksumFlag(true),
        CParameter::NbWorkers(settings.workers.max(1)),
    ];
    if settings.preset == Preset::Archive {
        parameters.push(CParameter::EnableLongDistanceMatching(true));
        // Long-distance matching already defaults the window to 2^27;
        // setting it states the size the archives are meant to have.
        parameters.push(CParameter::WindowLog(ARCHIVE_WINDOW_LOG));
    }
    parameters
}

/// An encoder that writes a checksummed zstd stream into `out`.
pub fn encoder<W: Write>(out: W, settings: &ZstdSettings) -> io::Result<ProgressEncoder<W>> {
    encoder_counted(out, settings, None)
}

/// An encoder that also raises `counter` to the number of input bytes the
/// compressor has consumed, while it writes and while it finishes.
pub fn encoder_counted<W: Write>(
    out: W,
    settings: &ZstdSettings,
    counter: Option<&Counter>,
) -> io::Result<ProgressEncoder<W>> {
    encoder_with(out, &parameters(settings), counter)
}

fn encoder_with<W: Write>(
    out: W,
    parameters: &[CParameter],
    counter: Option<&Counter>,
) -> io::Result<ProgressEncoder<W>> {
    let mut context = CCtx::create();
    for parameter in parameters {
        context
            .set_parameter(*parameter)
            .map_err(compressor_error)?;
    }
    let operation = Counting {
        context,
        counter: counter.cloned(),
        fed: 0,
        #[cfg(test)]
        reads: 0,
    };
    Ok(ProgressEncoder {
        writer: Writer::new(out, operation),
    })
}

fn compressor_error(code: usize) -> io::Error {
    io::Error::other(zstd_safe::get_error_name(code))
}

/// How many input bytes the compressor takes in between two reads of its
/// progress. The read locks the compressor's job table, so it is not made
/// per write.
const POLL_EVERY_BYTES: u64 = 256 * 1024;

/// A zstd stream encoder that reports how much input the compressor has
/// consumed.
///
/// With several workers, `write` returns as soon as the input sits in a job
/// buffer, so the bytes handed to the encoder run far ahead of the bytes
/// compressed, and the end of the frame can take longer than all writes
/// together. The consumed bytes are the ones that move steadily through
/// both phases.
pub struct ProgressEncoder<W: Write> {
    writer: Writer<W, Counting>,
}

impl<W: Write> ProgressEncoder<W> {
    /// Ends the frame and returns the writer under the encoder. The
    /// counter ends at the input length.
    pub fn finish(mut self) -> io::Result<W> {
        self.writer.finish()?;
        Ok(self.writer.into_inner().0)
    }
}

impl<W: Write> Write for ProgressEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// The compressor as an [`Operation`] of `zstd`'s stream writer, which
/// keeps the compression context where the progress can be read.
struct Counting {
    context: CCtx<'static>,
    counter: Option<Counter>,
    /// Input bytes taken since the progress was last read.
    fed: u64,
    #[cfg(test)]
    reads: u64,
}

impl Counting {
    /// Raises the counter to the bytes the compressor has consumed.
    fn report(&mut self) {
        self.fed = 0;
        let Some(counter) = &self.counter else {
            return;
        };
        #[cfg(test)]
        {
            self.reads += 1;
        }
        counter.raise_to(self.context.get_frame_progression().consumed);
    }
}

impl Operation for Counting {
    fn run<C: WriteBuf + ?Sized>(
        &mut self,
        input: &mut InBuffer<'_>,
        output: &mut OutBuffer<'_, C>,
    ) -> io::Result<usize> {
        let before = input.pos();
        let hint = self
            .context
            .compress_stream(output, input)
            .map_err(compressor_error)?;
        self.fed += (input.pos() - before) as u64;
        if self.fed >= POLL_EVERY_BYTES {
            self.report();
        }
        Ok(hint)
    }

    fn flush<C: WriteBuf + ?Sized>(&mut self, output: &mut OutBuffer<'_, C>) -> io::Result<usize> {
        let remaining = self
            .context
            .flush_stream(output)
            .map_err(compressor_error)?;
        self.report();
        Ok(remaining)
    }

    fn finish<C: WriteBuf + ?Sized>(
        &mut self,
        output: &mut OutBuffer<'_, C>,
        _finished_frame: bool,
    ) -> io::Result<usize> {
        let remaining = self.context.end_stream(output).map_err(compressor_error)?;
        self.report();
        Ok(remaining)
    }
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
    /// The encoder every archive used before the progress encoder: the
    /// stock stream encoder, configured call by call, plus `extras`.
    fn stock_frame(settings: &ZstdSettings, extras: &[CParameter], input: &[u8]) -> Vec<u8> {
        let level = match settings.preset {
            Preset::Archive => ARCHIVE_LEVEL,
            Preset::Fast => FAST_LEVEL,
        };
        let mut encoder =
            ::zstd::stream::write::Encoder::new(Vec::new(), level).expect("an encoder starts");
        encoder.include_checksum(true).expect("valid");
        encoder.multithread(settings.workers.max(1)).expect("valid");
        if settings.preset == Preset::Archive {
            encoder.long_distance_matching(true).expect("valid");
            encoder.window_log(ARCHIVE_WINDOW_LOG).expect("valid");
        }
        for extra in extras {
            encoder.set_parameter(*extra).expect("valid");
        }
        encoder.write_all(input).expect("a vec accepts the stream");
        encoder.finish().expect("the frame ends")
    }

    fn progress_frame(settings: &ZstdSettings, extras: &[CParameter], input: &[u8]) -> Vec<u8> {
        let mut parameters = parameters(settings);
        parameters.extend_from_slice(extras);
        let mut encoder = encoder_with(Vec::new(), &parameters, None).expect("an encoder starts");
        encoder.write_all(input).expect("a vec accepts the stream");
        encoder.finish().expect("the frame ends")
    }

    #[test]
    fn the_progress_encoder_writes_the_bytes_of_the_stock_encoder() {
        // One level-19 encoder at a time: it takes about 300 MB, so the
        // archive cases run one after the other in this test. 0 workers
        // pin the clamp to one, 4 the multithreaded production path.
        for workers in [0, 1, 4] {
            for len in [0, 1, 3000] {
                let settings = ZstdSettings::archive(workers);
                let input = sample(len);
                assert_eq!(
                    progress_frame(&settings, &[], &input),
                    stock_frame(&settings, &[], &input),
                    "archive preset, {workers} workers, {len} bytes"
                );
            }
        }
        for len in [0, 1, 3000, 200 * 1024, 6 << 20] {
            let input = sample(len);
            assert_eq!(
                progress_frame(&ZstdSettings::fast(), &[], &input),
                stock_frame(&ZstdSettings::fast(), &[], &input),
                "fast preset, {len} bytes"
            );
        }
        // Several jobs: three workers on jobs of 1 MiB over 6 MiB.
        let jobs = ZstdSettings {
            preset: Preset::Fast,
            workers: 3,
        };
        let extras = [CParameter::JobSize(1 << 20), CParameter::WindowLog(20)];
        let input = sample(6 << 20);
        assert_eq!(
            progress_frame(&jobs, &extras, &input),
            stock_frame(&jobs, &extras, &input),
            "three workers, jobs of 1 MiB"
        );
    }

    /// Records what the counter says each time compressed output arrives.
    struct Recording {
        counter: Counter,
        seen: Vec<u64>,
    }

    impl Write for Recording {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.seen.push(self.counter.done());
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_counter_follows_the_consumed_bytes_and_ends_at_the_input_length() {
        let jobs = ZstdSettings {
            preset: Preset::Fast,
            workers: 3,
        };
        let mut parameters = parameters(&jobs);
        parameters.push(CParameter::JobSize(1 << 20));
        parameters.push(CParameter::WindowLog(20));
        let input = sample(6 << 20);
        let counter = Counter::new();
        let sink = Recording {
            counter: counter.clone(),
            seen: Vec::new(),
        };
        let mut encoder = encoder_with(sink, &parameters, Some(&counter)).expect("an encoder");
        for chunk in input.chunks(64 * 1024) {
            encoder.write_all(chunk).expect("the sink accepts");
        }
        let during_writes = counter.done();
        let sink = encoder.finish().expect("the frame ends");

        assert!(during_writes <= input.len() as u64);
        assert!(
            sink.seen.windows(2).all(|pair| pair[0] <= pair[1]),
            "{:?}",
            sink.seen
        );
        assert!(sink.seen.iter().all(|&done| done <= input.len() as u64));
        assert_eq!(counter.done(), input.len() as u64);
    }

    #[test]
    fn the_progression_is_read_every_256_kib_fed() {
        let counter = Counter::new();
        let mut encoder =
            encoder_counted(Vec::new(), &ZstdSettings::fast(), Some(&counter)).expect("an encoder");
        let block = sample(8 * 1024);
        let reads = |encoder: &ProgressEncoder<Vec<u8>>| encoder.writer.operation().reads;

        for _ in 0..31 {
            encoder.write_all(&block).expect("a vec accepts the stream");
        }
        assert_eq!(reads(&encoder), 0, "248 KiB are below the threshold");
        for _ in 31..64 {
            encoder.write_all(&block).expect("a vec accepts the stream");
        }
        assert_eq!(reads(&encoder), 2, "512 KiB make two reads");
        encoder.finish().expect("the frame ends");
        assert_eq!(counter.done(), 64 * 8 * 1024);
    }
}
