//! The text encodings an import file can be in, decoded as a stream into UTF-8.
//!
//! The parsers read UTF-8 only. A file in another encoding goes through a [`Transcoder`] that turns
//! one buffer at a time into UTF-8, so a file is never held in memory whatever it is encoded in.
//! A byte order mark picks the encoding when the user left it on Auto (and is dropped in every
//! case: it is not part of the first value); without one, Auto still recognises UTF-16 text by the zero bytes
//! of its first block (every other byte of ASCII text is zero).

use std::io::{self, BufRead, BufReader, Read};

/// How an import file's bytes are read as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// A byte order mark decides (UTF-8, UTF-16 LE or BE); a file without one is UTF-8.
    #[default]
    Auto,
    Utf8,
    /// ISO-8859-1: every byte is the code point of the same number.
    Latin1,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub const ALL: [Self; 5] = [Self::Auto, Self::Utf8, Self::Latin1, Self::Utf16Le, Self::Utf16Be];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (BOM, else UTF-8)",
            Self::Utf8 => "UTF-8",
            Self::Latin1 => "ISO-8859-1 (Latin-1)",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }

    /// The encoding the byte order mark at the start of `head` names, and the mark's length.
    fn from_mark(head: &[u8]) -> Option<(Self, usize)> {
        match head {
            [0xEF, 0xBB, 0xBF, ..] => Some((Self::Utf8, 3)),
            [0xFF, 0xFE, ..] => Some((Self::Utf16Le, 2)),
            [0xFE, 0xFF, ..] => Some((Self::Utf16Be, 2)),
            _ => None,
        }
    }

    /// UTF-16 without a mark, told by its zero bytes: text in the Latin range has one in every other byte, at the
    /// odd positions (little endian) or the even ones (big endian), and none at the others.
    fn sniff_utf16(block: &[u8]) -> Option<Self> {
        if block.len() < 4 {
            return None;
        }
        let pairs = block.len() / 2;
        let zeros = |parity: usize| block.iter().enumerate().filter(|(index, byte)| index % 2 == parity && **byte == 0).count();
        let (even, odd) = (zeros(0), zeros(1));
        if even == 0 && odd * 2 >= pairs {
            Some(Self::Utf16Le)
        } else if odd == 0 && even * 2 >= pairs {
            Some(Self::Utf16Be)
        } else {
            None
        }
    }

    /// `reader` as UTF-8 text. The mark of the file is consumed; on Auto it also picks the encoding.
    pub fn decode<R: Read + 'static>(self, mut reader: R) -> io::Result<Box<dyn BufRead>> {
        // A reader may hand out fewer bytes than asked for: read until the longest mark fits or the file ends.
        let mut head = [0u8; SNIFF_BYTES];
        let mut length = 0;
        while length < head.len() {
            match reader.read(&mut head[length..])? {
                0 => break,
                count => length += count,
            }
        }
        let mark = Self::from_mark(&head[..length]);
        let resolved = match (self, mark) {
            (Self::Auto, Some((found, _))) => found,
            (Self::Auto, None) => Self::sniff_utf16(&head[..length]).unwrap_or(Self::Utf8),
            (explicit, _) => explicit,
        };
        // Only the mark of the encoding being read is dropped: a Latin-1 file starting with "ï»¿" has that text.
        let skipped = mark.filter(|(found, _)| *found == resolved).map_or(0, |(_, length)| length);
        let rest = io::Cursor::new(head[skipped..length].to_vec()).chain(reader);
        let reader = BufReader::with_capacity(128 * 1024, rest);
        Ok(match resolved {
            Self::Auto | Self::Utf8 => Box::new(reader),
            Self::Latin1 => Box::new(Transcoder::new(reader, Source::Latin1)),
            Self::Utf16Le => Box::new(Transcoder::new(reader, Source::Utf16 { big_endian: false })),
            Self::Utf16Be => Box::new(Transcoder::new(reader, Source::Utf16 { big_endian: true })),
        })
    }
}

/// The first block read to find the encoding.
const SNIFF_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy)]
enum Source {
    Latin1,
    Utf16 { big_endian: bool },
}

/// A buffered reader that yields the UTF-8 of an encoded stream.
struct Transcoder<R> {
    inner: R,
    source: Source,
    /// Decoded text not yet consumed: `out[position..]`.
    out: Vec<u8>,
    position: usize,
    /// The first byte of a UTF-16 unit whose second byte is in the next buffer.
    odd: Option<u8>,
    /// A high surrogate waiting for its low one.
    high: Option<u16>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn push_char(out: &mut Vec<u8>, character: char) {
    let mut encoded = [0u8; 4];
    out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

/// Adds the UTF-16 unit `unit` to `out`; `high` holds a high surrogate waiting for its low one.
fn push_unit(out: &mut Vec<u8>, high: &mut Option<u16>, unit: u16) -> io::Result<()> {
    match (high.take(), unit) {
        (Some(first), 0xDC00..=0xDFFF) => {
            let code = 0x10000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
            push_char(out, char::from_u32(code).ok_or_else(|| invalid("invalid UTF-16 pair"))?);
        }
        (Some(_), _) | (None, 0xDC00..=0xDFFF) => return Err(invalid("unpaired surrogate in UTF-16 text")),
        (None, 0xD800..=0xDBFF) => *high = Some(unit),
        (None, unit) => push_char(out, char::from_u32(u32::from(unit)).ok_or_else(|| invalid("invalid UTF-16 unit"))?),
    }
    Ok(())
}

impl<R: BufRead> Transcoder<R> {
    fn new(inner: R, source: Source) -> Self {
        Self { inner, source, out: Vec::with_capacity(256 * 1024), position: 0, odd: None, high: None }
    }

    /// Decodes the next buffer of the input into `out`, in place (the input's buffer is read where it is); `false` at
    /// the end of the input.
    fn refill(&mut self) -> io::Result<bool> {
        self.out.clear();
        self.position = 0;
        while self.out.is_empty() {
            let buffer = self.inner.fill_buf()?;
            if buffer.is_empty() {
                if self.odd.is_some() || self.high.is_some() {
                    return Err(invalid("the UTF-16 text ends in the middle of a character"));
                }
                return Ok(false);
            }
            let length = buffer.len();
            match self.source {
                Source::Latin1 => buffer.iter().for_each(|&byte| push_char(&mut self.out, char::from(byte))),
                Source::Utf16 { big_endian } => {
                    for &byte in buffer {
                        match self.odd.take() {
                            None => self.odd = Some(byte),
                            Some(first) => {
                                let pair = [first, byte];
                                let unit = if big_endian { u16::from_be_bytes(pair) } else { u16::from_le_bytes(pair) };
                                push_unit(&mut self.out, &mut self.high, unit)?;
                            }
                        }
                    }
                }
            }
            self.inner.consume(length);
        }
        Ok(true)
    }
}

impl<R: BufRead> Read for Transcoder<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(target.len());
        target[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl<R: BufRead> BufRead for Transcoder<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position >= self.out.len() && !self.refill()? {
            return Ok(&[]);
        }
        Ok(&self.out[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        self.position = (self.position + amount).min(self.out.len());
    }
}

/// Counts the bytes read through it, for the progress of an import.
pub struct CountingReader<R> {
    inner: R,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R> CountingReader<R> {
    pub fn new(inner: R, count: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self { inner, count }
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(target)?;
        self.count.fetch_add(read as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(encoding: Encoding, bytes: &[u8]) -> io::Result<String> {
        let mut text = String::new();
        encoding.decode(io::Cursor::new(bytes.to_vec()))?.read_to_string(&mut text)?;
        Ok(text)
    }

    fn utf16(text: &str, big_endian: bool, mark: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut units: Vec<u16> = text.encode_utf16().collect();
        if mark {
            units.insert(0, 0xFEFF);
        }
        for unit in units {
            bytes.extend(if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() });
        }
        bytes
    }

    #[test]
    fn auto_reads_plain_utf8_and_drops_its_mark() {
        assert_eq!(decode(Encoding::Auto, "a,é\n".as_bytes()).unwrap(), "a,é\n");
        let mut marked = vec![0xEF, 0xBB, 0xBF];
        marked.extend("id,名前\n".as_bytes());
        assert_eq!(decode(Encoding::Auto, &marked).unwrap(), "id,名前\n");
        assert_eq!(decode(Encoding::Utf8, &marked).unwrap(), "id,名前\n");
    }

    #[test]
    fn auto_follows_a_utf16_byte_order_mark() {
        let text = "id,name\n1,Zoë 😀\n";
        assert_eq!(decode(Encoding::Auto, &utf16(text, false, true)).unwrap(), text);
        assert_eq!(decode(Encoding::Auto, &utf16(text, true, true)).unwrap(), text);
    }

    #[test]
    fn auto_recognises_utf16_without_a_mark_by_its_zero_bytes() {
        let text = "id,name\n1,Ann\n2,Bob\n";
        assert_eq!(decode(Encoding::Auto, &utf16(text, false, false)).unwrap(), text);
        assert_eq!(decode(Encoding::Auto, &utf16(text, true, false)).unwrap(), text);
        // Plain UTF-8, text with a stray zero byte and very short input stay UTF-8.
        assert_eq!(decode(Encoding::Auto, b"hello\0world and more text").unwrap(), "hello\0world and more text");
        assert_eq!(decode(Encoding::Auto, b"ab").unwrap(), "ab");
        assert_eq!(decode(Encoding::Auto, "caf\u{e9}".as_bytes()).unwrap(), "caf\u{e9}");
    }

    #[test]
    fn explicit_utf16_needs_no_mark_and_reads_astral_characters() {
        let text = "a😀b";
        assert_eq!(decode(Encoding::Utf16Le, &utf16(text, false, false)).unwrap(), text);
        assert_eq!(decode(Encoding::Utf16Be, &utf16(text, true, false)).unwrap(), text);
        // A mark of the encoding being read is dropped there too.
        assert_eq!(decode(Encoding::Utf16Le, &utf16(text, false, true)).unwrap(), text);
    }

    #[test]
    fn latin1_maps_each_byte_to_its_code_point() {
        assert_eq!(decode(Encoding::Latin1, &[b'c', b'a', b'f', 0xE9, b' ', 0xFC]).unwrap(), "café ü");
        // Its first bytes are text, not a UTF-8 mark.
        assert_eq!(decode(Encoding::Latin1, &[0xEF, 0xBB, 0xBF]).unwrap(), "\u{ef}\u{bb}\u{bf}");
    }

    #[test]
    fn broken_utf16_is_an_error_not_replacement_text() {
        assert!(decode(Encoding::Utf16Le, &[0x61]).is_err(), "half a unit");
        assert!(decode(Encoding::Utf16Le, &[0x00, 0xD8]).is_err(), "a high surrogate alone");
        assert!(decode(Encoding::Utf16Le, &[0x00, 0xDC]).is_err(), "a low surrogate alone");
    }

    #[test]
    fn a_unit_split_across_buffers_is_decoded_whole() {
        // A reader that hands out one byte at a time splits every unit and pair.
        struct OneByte(io::Cursor<Vec<u8>>);
        impl Read for OneByte {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                let limit = target.len().min(1);
                self.0.read(&mut target[..limit])
            }
        }
        let text = "x😀y,z\n";
        let mut decoded = String::new();
        let bytes = utf16(text, false, true);
        let mut reader = Encoding::Auto.decode(OneByte(io::Cursor::new(bytes))).unwrap();
        reader.read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, text);
    }
}
