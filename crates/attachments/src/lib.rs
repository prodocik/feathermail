//! Bounded-memory attachment cache and file export (T-043).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const STREAM_CHUNK: usize = 64 * 1024;

/// MIME transfer encoding of the literal fetched from an IMAP body section.
/// The decoder is deliberately here, beside the file writer, so a large
/// base64 attachment is never expanded into a whole-message `Vec` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferEncoding {
    Base64,
    QuotedPrintable,
    Identity,
}

/// Streams a provider response into an atomic cache file. A partial download
/// is never exposed under `destination`; restart replaces the `.part` sibling.
pub fn stream_to_file(
    mut source: impl Read,
    destination: &Path,
    max_bytes: Option<u64>,
) -> io::Result<u64> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = partial_path(destination);
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&part)?;
    let result = copy_bounded(&mut source, &mut output, max_bytes);
    match result {
        Ok(bytes) => {
            output.sync_all()?;
            drop(output);
            fs::rename(&part, destination)?;
            Ok(bytes)
        }
        Err(err) => {
            drop(output);
            let _ = fs::remove_file(&part);
            Err(err)
        }
    }
}

/// Streams a cached attachment to Save As / portal output without loading it
/// into a `Vec<u8>` first.
pub fn stream_file(source: &Path, mut destination: impl Write) -> io::Result<u64> {
    let mut input = File::open(source)?;
    copy_bounded(&mut input, &mut destination, None)
}

/// Transfer-decodes an IMAP MIME section directly into an atomic cache file.
/// On malformed base64 or quoted-printable input this follows the existing
/// message parser's best-effort policy: incomplete escape tails are dropped,
/// while non-encoding bytes are preserved where possible.
pub fn decode_to_file(
    source: impl Read,
    destination: &Path,
    encoding: TransferEncoding,
    max_bytes: Option<u64>,
) -> io::Result<u64> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let part = partial_path(destination);
    let output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&part)?;
    let mut output = BufWriter::new(output);
    let result = decode_bounded(source, &mut output, encoding, max_bytes);
    match result {
        Ok(bytes) => {
            output.flush()?;
            let output = output.into_inner().map_err(io::Error::other)?;
            output.sync_all()?;
            fs::rename(&part, destination)?;
            Ok(bytes)
        }
        Err(err) => {
            drop(output);
            let _ = fs::remove_file(&part);
            Err(err)
        }
    }
}

/// Decodes one stream with a fixed input and output buffer. The return value
/// is the decoded file size, not the transfer-encoded wire size.
pub fn decode_bounded(
    mut source: impl Read,
    destination: &mut impl Write,
    encoding: TransferEncoding,
    max_bytes: Option<u64>,
) -> io::Result<u64> {
    if encoding == TransferEncoding::Identity {
        return copy_bounded(&mut source, destination, max_bytes);
    }

    let mut input = [0_u8; STREAM_CHUNK];
    let mut pending = Vec::with_capacity(STREAM_CHUNK);
    let mut total = 0_u64;
    let mut base64 = Base64State::default();
    let mut quoted_printable = QuotedPrintableState::Normal;

    loop {
        let read = source.read(&mut input)?;
        if read == 0 {
            break;
        }
        for &byte in &input[..read] {
            match encoding {
                TransferEncoding::Base64 => {
                    if let Some(decoded) = base64.push(byte) {
                        push_decoded(&mut pending, decoded, &mut total, max_bytes, destination)?;
                    }
                }
                TransferEncoding::QuotedPrintable => {
                    quoted_printable.push(
                        byte,
                        &mut pending,
                        &mut total,
                        max_bytes,
                        destination,
                    )?;
                }
                TransferEncoding::Identity => unreachable!("handled above"),
            }
        }
    }
    quoted_printable.finish(&mut pending, &mut total, max_bytes, destination)?;
    flush_pending(&mut pending, destination)?;
    destination.flush()?;
    Ok(total)
}

fn push_decoded(
    pending: &mut Vec<u8>,
    byte: u8,
    total: &mut u64,
    max_bytes: Option<u64>,
    destination: &mut impl Write,
) -> io::Result<()> {
    *total = total
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::FileTooLarge, "attachment too large"))?;
    if max_bytes.is_some_and(|limit| *total > limit) {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "attachment exceeds configured limit",
        ));
    }
    pending.push(byte);
    if pending.len() == STREAM_CHUNK {
        flush_pending(pending, destination)?;
    }
    Ok(())
}

fn flush_pending(pending: &mut Vec<u8>, destination: &mut impl Write) -> io::Result<()> {
    if !pending.is_empty() {
        destination.write_all(pending)?;
        pending.clear();
    }
    Ok(())
}

#[derive(Default)]
struct Base64State {
    acc: u32,
    bits: u32,
}

impl Base64State {
    fn push(&mut self, byte: u8) -> Option<u8> {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        self.acc = (self.acc << 6) | u32::from(value);
        self.bits += 6;
        if self.bits >= 8 {
            self.bits -= 8;
            Some((self.acc >> self.bits) as u8)
        } else {
            None
        }
    }
}

enum QuotedPrintableState {
    Normal,
    Equals,
    EqualsCr,
    EqualsHex { value: u8, raw: u8 },
}

impl QuotedPrintableState {
    fn push(
        &mut self,
        byte: u8,
        pending: &mut Vec<u8>,
        total: &mut u64,
        max_bytes: Option<u64>,
        destination: &mut impl Write,
    ) -> io::Result<()> {
        let previous = std::mem::replace(self, Self::Normal);
        match previous {
            Self::Normal if byte == b'=' => *self = Self::Equals,
            Self::Normal => push_decoded(pending, byte, total, max_bytes, destination)?,
            Self::Equals if byte == b'\n' => {}
            Self::Equals if byte == b'\r' => *self = Self::EqualsCr,
            Self::Equals if let Some(value) = hex_value(byte) => {
                *self = Self::EqualsHex { value, raw: byte }
            }
            Self::Equals => push_decoded(pending, byte, total, max_bytes, destination)?,
            Self::EqualsCr if byte == b'\n' => {}
            Self::EqualsCr => {
                push_decoded(pending, b'\r', total, max_bytes, destination)?;
                self.push(byte, pending, total, max_bytes, destination)?;
            }
            Self::EqualsHex { value, .. } if let Some(low) = hex_value(byte) => {
                push_decoded(pending, (value << 4) | low, total, max_bytes, destination)?;
            }
            Self::EqualsHex { raw, .. } => {
                push_decoded(pending, raw, total, max_bytes, destination)?;
                self.push(byte, pending, total, max_bytes, destination)?;
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        pending: &mut Vec<u8>,
        total: &mut u64,
        max_bytes: Option<u64>,
        destination: &mut impl Write,
    ) -> io::Result<()> {
        match std::mem::replace(self, Self::Normal) {
            Self::EqualsCr => push_decoded(pending, b'\r', total, max_bytes, destination),
            Self::EqualsHex { raw, .. } => {
                push_decoded(pending, raw, total, max_bytes, destination)
            }
            Self::Normal | Self::Equals => Ok(()),
        }
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn copy_bounded(
    source: &mut impl Read,
    destination: &mut impl Write,
    max_bytes: Option<u64>,
) -> io::Result<u64> {
    let mut buffer = [0_u8; STREAM_CHUNK];
    let mut total = 0_u64;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::FileTooLarge, "attachment too large"))?;
        if max_bytes.is_some_and(|limit| total > limit) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "attachment exceeds configured limit",
            ));
        }
        destination.write_all(&buffer[..read])?;
    }
    destination.flush()?;
    Ok(total)
}

fn partial_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    destination.with_file_name(name)
}

pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ChunkGuard {
        left: u64,
        largest_request: usize,
    }

    impl Read for ChunkGuard {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest_request = self.largest_request.max(buffer.len());
            if self.left == 0 {
                return Ok(0);
            }
            let n = usize::try_from(self.left.min(buffer.len() as u64)).unwrap();
            buffer[..n].fill(7);
            self.left -= n as u64;
            Ok(n)
        }
    }

    #[test]
    fn one_hundred_mb_uses_only_the_fixed_chunk() {
        let mut source = ChunkGuard {
            left: 100 * 1024 * 1024,
            largest_request: 0,
        };
        let mut sink = io::sink();
        assert_eq!(
            copy_bounded(&mut source, &mut sink, None).unwrap(),
            100 * 1024 * 1024
        );
        assert_eq!(source.largest_request, STREAM_CHUNK);
    }

    #[test]
    fn failed_limit_leaves_no_visible_or_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("large.bin");
        let source = io::repeat(1).take(1024);
        assert!(stream_to_file(source, &target, Some(100)).is_err());
        assert!(!target.exists());
        assert!(!partial_path(&target).exists());
    }

    #[test]
    fn base64_decodes_across_small_read_boundaries_into_an_atomic_file() {
        struct TinyReader(std::io::Cursor<Vec<u8>>);
        impl Read for TinyReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let chunk = buffer.len().min(2);
                self.0.read(&mut buffer[..chunk])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("decoded.bin");
        let bytes = decode_to_file(
            TinyReader(io::Cursor::new(b"aGVs\r\nbG8=".to_vec())),
            &target,
            TransferEncoding::Base64,
            None,
        )
        .unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(fs::read(target).unwrap(), b"hello");
    }

    #[test]
    fn quoted_printable_keeps_split_escapes_and_soft_breaks_streaming() {
        let mut decoded = Vec::new();
        let bytes = decode_bounded(
            io::Cursor::new(b"caf=C3=A9=\r\nnext"),
            &mut decoded,
            TransferEncoding::QuotedPrintable,
            None,
        )
        .unwrap();
        assert_eq!(bytes, 9);
        assert_eq!(decoded, "cafénext".as_bytes());
    }
}
