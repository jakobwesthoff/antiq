// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Readers and writers that count and hash the bytes passing through them.
//!
//! Packing hashes every file in the read that feeds the tar stream and the
//! compressed stream in the write that stores it, so nothing is read twice.

use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

/// Lower-case hex of a digest, the form listings and metadata use.
pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A writer that counts and hashes everything written through it.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> HashingWriter<W> {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }

    /// Bytes the inner writer accepted so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The SHA-256 of the bytes the inner writer accepted so far, as
    /// lower-case hex.
    pub fn sha256(&self) -> String {
        hex(&self.hasher.clone().finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Only the bytes the inner writer took count, since the caller
        // offers the rest again.
        let taken = self.inner.write(buf)?;
        self.hasher.update(&buf[..taken]);
        self.written += taken as u64;
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A reader that counts and hashes everything read through it.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    read: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> HashingReader<R> {
        HashingReader {
            inner,
            hasher: Sha256::new(),
            read: 0,
        }
    }

    /// Bytes that came out of the reader so far.
    pub fn bytes_read(&self) -> u64 {
        self.read
    }

    /// The SHA-256 of the bytes read so far.
    pub fn digest(&self) -> [u8; 32] {
        self.hasher.clone().finalize().into()
    }

    /// [`digest`](Self::digest) as lower-case hex.
    pub fn sha256(&self) -> String {
        hex(&self.digest())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.hasher.update(&buf[..count]);
        self.read += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn a_writer_counts_and_hashes_what_passes() {
        let mut writer = HashingWriter::new(Vec::new());
        writer.write_all(b"a").expect("a vec accepts bytes");
        writer.write_all(b"bc").expect("a vec accepts bytes");
        assert_eq!(writer.written(), 3);
        assert_eq!(writer.sha256(), ABC);
        assert_eq!(writer.inner, b"abc");
    }

    #[test]
    fn a_writer_counts_only_the_bytes_the_inner_writer_took() {
        // A writer that takes one byte per call.
        struct Slow(Vec<u8>);
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.push(buf[0]);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = HashingWriter::new(Slow(Vec::new()));
        writer.write_all(b"abc").expect("the slow writer accepts");
        writer.flush().expect("the slow writer flushes");
        assert_eq!(writer.written(), 3);
        assert_eq!(writer.sha256(), ABC);
    }

    #[test]
    fn flushing_reaches_the_inner_writer() {
        struct Flagged(bool);
        impl Write for Flagged {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0 = true;
                Ok(())
            }
        }
        let mut writer = HashingWriter::new(Flagged(false));
        writer.write_all(b"x").expect("the flag writer accepts");
        writer.flush().expect("the flag writer flushes");
        assert!(writer.inner.0);
    }

    #[test]
    fn a_reader_counts_and_hashes_what_it_yields() {
        let mut reader = HashingReader::new(&b"abc"[..]);
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).expect("a slice reads");
        assert_eq!(sink, b"abc");
        assert_eq!(reader.bytes_read(), 3);
        assert_eq!(reader.sha256(), ABC);
    }

    #[test]
    fn hex_writes_two_lower_case_digits_per_byte() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }
}
