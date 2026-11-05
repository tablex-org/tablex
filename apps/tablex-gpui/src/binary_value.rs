//! Binary cell values (BLOB, bytea, BINARY): the pure part of how the value
//! panel shows them. Hex dump rows, text decoding, image detection by magic
//! bytes and the dimensions read from an image header. No UI types, so it is
//! unit-testable; the panel (`app::results::binary_view`) draws what this
//! computes, one visible row at a time.

use std::fmt::Write as _;

/// Largest value the panel previews (hex rows, text, image). A bigger value
/// still shows its size and can be saved to a file, but is never decoded.
pub const PREVIEW_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Largest value that may be edited as `0x…` text in the panel's editor (the
/// text is twice the size and a text editor is slow on megabytes): a bigger
/// one is replaced from a file.
pub const TEXT_EDIT_CAP_BYTES: usize = 64 * 1024;

/// Bytes per hex dump row in a wide panel; a narrow one shows [`NARROW_ROW_BYTES`].
pub const WIDE_ROW_BYTES: usize = 16;
pub const NARROW_ROW_BYTES: usize = 8;

/// Panel width from which a dump row of [`WIDE_ROW_BYTES`] fits.
pub const WIDE_ROW_MIN_WIDTH: f32 = 540.0;

/// Bytes per dump row for a panel `width` pixels wide.
pub fn row_bytes_for_width(width: f32) -> usize {
    if width >= WIDE_ROW_MIN_WIDTH {
        WIDE_ROW_BYTES
    } else {
        NARROW_ROW_BYTES
    }
}

/// Text characters the Text view builds at most (the rest is "truncated").
pub const TEXT_PREVIEW_CHARS: usize = 64 * 1024;

/// `12 B`, `12.3 KB`, `4.0 MB`: the size of a value in 1024-based units.
pub fn format_size(len: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    if len < 1024 {
        return format!("{len} B");
    }
    let mut value = len as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Number of hex dump rows of `len` bytes at `per_row` bytes per row.
pub fn hex_row_count(len: usize, per_row: usize) -> usize {
    len.div_ceil(per_row.max(1))
}

/// One row of a hex dump: `offset | hex bytes | ASCII`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexRow {
    pub offset: String,
    /// Two hex digits per byte, a space between bytes and an extra one in the
    /// middle; short rows are padded so the ASCII column lines up.
    pub hex: String,
    pub ascii: String,
}

/// The dump row `row` of `bytes` at `per_row` bytes per row (`None` past the end).
pub fn hex_row(bytes: &[u8], row: usize, per_row: usize) -> Option<HexRow> {
    let start = row.checked_mul(per_row)?;
    let chunk = bytes.get(start..)?.get(..per_row.min(bytes.len() - start))?;
    if chunk.is_empty() {
        return None;
    }
    let mut hex = String::with_capacity(per_row * 3 + 1);
    let mut ascii = String::with_capacity(per_row);
    for index in 0..per_row {
        if index == per_row / 2 {
            hex.push(' ');
        }
        match chunk.get(index) {
            Some(byte) => {
                let _ = write!(hex, "{byte:02x}");
                ascii.push(if (0x20..0x7f).contains(byte) {
                    *byte as char
                } else {
                    '.'
                });
            }
            None => hex.push_str("  "),
        }
        if index + 1 < per_row {
            hex.push(' ');
        }
    }
    Some(HexRow {
        offset: format!("{start:08x}"),
        hex,
        ascii,
    })
}

#[cfg(test)]
/// `0x` + lower-case hex: the form the SQL drivers accept back for a binary
/// column, and what Copy puts on the clipboard.
pub fn to_hex_literal(bytes: &[u8]) -> String {
    to_hex_form(bytes, HexForm::ZeroX)
}

#[cfg(test)]
/// The bytes of `0x` + an even number of hex digits.
pub fn from_hex_literal(text: &str) -> Option<Vec<u8>> {
    from_hex_form(text, HexForm::ZeroX)
}

/// How a binary value is written as text: SQL drivers that deliver real bytes
/// take `0x` + hex back; PostgreSQL delivers `bytea` as the server's own text,
/// `\x` + hex, and takes that back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HexForm {
    #[default]
    ZeroX,
    PgBytea,
}

impl HexForm {
    fn prefix(self) -> &'static str {
        match self {
            Self::ZeroX => "0x",
            Self::PgBytea => "\\x",
        }
    }
}

/// `bytes` in `form`.
pub fn to_hex_form(bytes: &[u8], form: HexForm) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str(form.prefix());
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The bytes of `text` written in `form` (an even number of hex digits).
pub fn from_hex_form(text: &str, form: HexForm) -> Option<Vec<u8>> {
    let digits = text.trim().strip_prefix(form.prefix())?;
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

/// How the Text view reads the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Latin1,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Latin1 => "Latin-1",
        }
    }
}

/// A control character other than a line break or tab reads as a middle dot:
/// the text shaper never sees NUL, ESC or the like.
fn printable(ch: char) -> char {
    if ch.is_control() && !matches!(ch, '\n' | '\t' | '\r') {
        '\u{b7}'
    } else {
        ch
    }
}

/// The text of the first bytes of `bytes` (at most [`TEXT_PREVIEW_CHARS`]
/// characters) and whether more followed. UTF-8 replaces what is not valid
/// with U+FFFD; Latin-1 maps every byte to the code point of the same value.
/// Control characters read as `\u{b7}`.
pub fn decode_text(bytes: &[u8], encoding: TextEncoding) -> (String, bool) {
    match encoding {
        TextEncoding::Latin1 => {
            let truncated = bytes.len() > TEXT_PREVIEW_CHARS;
            let text = bytes
                .iter()
                .take(TEXT_PREVIEW_CHARS)
                .map(|byte| printable(char::from(*byte)))
                .collect();
            (text, truncated)
        }
        TextEncoding::Utf8 => {
            // A multi-byte character is at most 4 bytes: reading a little past
            // the character limit bounds the work for a huge value.
            let window = &bytes[..bytes.len().min(TEXT_PREVIEW_CHARS * 4)];
            let lossy = String::from_utf8_lossy(window);
            let mut chars = lossy.chars();
            let text: String = chars.by_ref().take(TEXT_PREVIEW_CHARS).map(printable).collect();
            let cut = chars.next().is_some();
            (text, cut || window.len() < bytes.len())
        }
    }
}

/// Image formats the panel previews, found by their magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl ImageKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Bmp => "BMP",
        }
    }
}

/// The image format `bytes` start with, if any.
pub fn detect_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else if bytes.starts_with(b"BM") && bytes.len() >= 26 {
        Some(ImageKind::Bmp)
    } else {
        None
    }
}

/// Width and height from the image header (no pixel decoding), `None` when the
/// header is cut or unusual.
pub fn image_dimensions(bytes: &[u8], kind: ImageKind) -> Option<(u32, u32)> {
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let le16 = |at: usize| Some(u32::from(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?)));
    let le32 = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let (width, height) = match kind {
        // 8 byte signature, then the IHDR chunk: length, "IHDR", width, height.
        ImageKind::Png => (be32(16)?, be32(20)?),
        ImageKind::Gif => (le16(6)?, le16(8)?),
        // BITMAPINFOHEADER: signed 32 bit width and height (negative = top down).
        ImageKind::Bmp => (le32(18)?, (le32(22)? as i32).unsigned_abs()),
        ImageKind::Webp => webp_dimensions(bytes)?,
        ImageKind::Jpeg => jpeg_dimensions(bytes)?,
    };
    (width > 0 && height > 0).then_some((width, height))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let u24 = |at: usize| {
        let b = bytes.get(at..at + 3)?;
        Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };
    match bytes.get(12..16)? {
        // Extended: 24 bit width - 1 and height - 1.
        b"VP8X" => Some((u24(24)? + 1, u24(27)? + 1)),
        // Lossless: signature 0x2f, then 14 bit width - 1 and height - 1.
        b"VP8L" => {
            let b = bytes.get(21..25)?;
            let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        // Lossy: key frame header after the 3 byte frame tag and start code.
        b"VP8 " => {
            let b = bytes.get(26..30)?;
            Some((
                u32::from(u16::from_le_bytes([b[0], b[1]]) & 0x3fff),
                u32::from(u16::from_le_bytes([b[2], b[3]]) & 0x3fff),
            ))
        }
        _ => None,
    }
}

/// Walks the JPEG segments to the first start-of-frame marker.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        match marker {
            0xff => at += 1,
            // Standalone markers carry no length.
            0x01 | 0xd0..=0xd8 => at += 2,
            // SOF0..SOF15 except DHT (c4), JPG (c8) and DAC (cc).
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                let height = u16::from_be_bytes([*bytes.get(at + 5)?, *bytes.get(at + 6)?]);
                let width = u16::from_be_bytes([*bytes.get(at + 7)?, *bytes.get(at + 8)?]);
                return Some((u32::from(width), u32::from(height)));
            }
            _ => {
                let length = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
                at += 2 + length.max(2);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(12_595), "12.3 KB");
        assert_eq!(format_size(4 * 1024 * 1024), "4.0 MB");
    }

    #[test]
    fn a_hex_row_is_offset_hex_and_ascii() {
        let bytes: Vec<u8> = (0x41..0x41 + 20).collect();
        let first = hex_row(&bytes, 0, 16).unwrap();
        assert_eq!(first.offset, "00000000");
        assert_eq!(
            first.hex,
            "41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50"
        );
        assert_eq!(first.ascii, "ABCDEFGHIJKLMNOP");
        let second = hex_row(&bytes, 1, 16).unwrap();
        assert_eq!(second.offset, "00000010");
        assert!(second.hex.starts_with("51 52 53 54 "));
        assert_eq!(second.hex.len(), first.hex.len(), "short rows are padded");
        assert_eq!(second.ascii, "QRST");
        assert_eq!(hex_row(&bytes, 2, 16), None);
        assert_eq!(hex_row(&[], 0, 16), None);
        assert_eq!(hex_row(&[0, 0x7f, 0xff], 0, 16).unwrap().ascii, "...");
        assert_eq!(hex_row_count(0, 16), 0);
        assert_eq!(hex_row_count(16, 16), 1);
        assert_eq!(hex_row_count(17, 16), 2);
        let narrow = hex_row(&bytes, 1, 8).unwrap();
        assert_eq!(narrow.offset, "00000008");
        assert_eq!(narrow.hex, "49 4a 4b 4c  4d 4e 4f 50");
        assert_eq!(hex_row_count(17, 8), 3);
        assert_eq!(row_bytes_for_width(320.0), 8);
        assert_eq!(row_bytes_for_width(560.0), 16);
    }

    #[test]
    fn hex_literals_round_trip() {
        assert_eq!(to_hex_literal(&[0, 1, 0xab]), "0x0001ab");
        assert_eq!(from_hex_literal("0x0001AB"), Some(vec![0, 1, 0xab]));
        assert_eq!(from_hex_literal(" 0x "), Some(vec![]));
        assert_eq!(from_hex_literal("0x0"), None);
        assert_eq!(from_hex_literal("0xzz"), None);
        assert_eq!(from_hex_literal("ab"), None);
    }

    #[test]
    fn postgres_bytea_text_is_backslash_x_hex() {
        assert_eq!(to_hex_form(&[0xde, 0xad], HexForm::PgBytea), "\\xdead");
        assert_eq!(from_hex_form("\\xDEAD", HexForm::PgBytea), Some(vec![0xde, 0xad]));
        assert_eq!(from_hex_form("\\x", HexForm::PgBytea), Some(vec![]));
        assert_eq!(from_hex_form("0xdead", HexForm::PgBytea), None);
        assert_eq!(from_hex_form("\\xd", HexForm::PgBytea), None);
    }

    #[test]
    fn text_decodes_as_utf8_or_latin1() {
        let bytes = "héllo".as_bytes();
        assert_eq!(decode_text(bytes, TextEncoding::Utf8), ("héllo".into(), false));
        assert_eq!(
            decode_text(bytes, TextEncoding::Latin1),
            ("hÃ©llo".into(), false)
        );
        assert_eq!(
            decode_text(b"a\xffb", TextEncoding::Utf8).0,
            "a\u{fffd}b",
            "invalid UTF-8 becomes the replacement character"
        );
        assert_eq!(decode_text(b"a\xffb", TextEncoding::Latin1).0, "aÿb");
        assert_eq!(decode_text(b"a\0b\x1b\n\x85", TextEncoding::Latin1).0, "a\u{b7}b\u{b7}\n\u{b7}");
        assert_eq!(decode_text(b"a\0b\tc", TextEncoding::Utf8).0, "a\u{b7}b\tc");
        let long = vec![b'x'; TEXT_PREVIEW_CHARS + 10];
        for encoding in [TextEncoding::Utf8, TextEncoding::Latin1] {
            let (text, truncated) = decode_text(&long, encoding);
            assert_eq!(text.len(), TEXT_PREVIEW_CHARS);
            assert!(truncated);
        }
    }

    #[test]
    fn images_are_found_by_their_magic_bytes() {
        assert_eq!(detect_image(b"\x89PNG\r\n\x1a\nrest"), Some(ImageKind::Png));
        assert_eq!(detect_image(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageKind::Jpeg));
        assert_eq!(detect_image(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(detect_image(b"RIFF\0\0\0\0WEBPVP8 "), Some(ImageKind::Webp));
        let mut bmp = b"BM".to_vec();
        bmp.resize(30, 0);
        assert_eq!(detect_image(&bmp), Some(ImageKind::Bmp));
        assert_eq!(detect_image(b"BM"), None, "a bare 'BM' is text");
        assert_eq!(detect_image(b"plain text"), None);
        assert_eq!(detect_image(&[]), None);
    }

    #[test]
    fn dimensions_come_from_the_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(image_dimensions(&png, ImageKind::Png), Some((640, 480)));
        assert_eq!(image_dimensions(&png[..20], ImageKind::Png), None, "cut header");

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[3, 0, 5, 0]);
        assert_eq!(image_dimensions(&gif, ImageKind::Gif), Some((3, 5)));

        let mut bmp = b"BM".to_vec();
        bmp.resize(18, 0);
        bmp.extend_from_slice(&7i32.to_le_bytes());
        bmp.extend_from_slice(&(-9i32).to_le_bytes());
        assert_eq!(image_dimensions(&bmp, ImageKind::Bmp), Some((7, 9)));

        // SOI, an APP0 segment, then SOF0 (precision 8, height 300, width 200).
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8, 1, 44, 0, 200, 1, 0, 0, 0,
        ];
        assert_eq!(image_dimensions(&jpeg, ImageKind::Jpeg), Some((200, 300)));
        assert_eq!(image_dimensions(&jpeg[..10], ImageKind::Jpeg), None);

        let mut webp_x = b"RIFF\0\0\0\0WEBPVP8X\n\0\0\0".to_vec();
        webp_x.extend_from_slice(&[0, 0, 0, 0, 99, 0, 0, 49, 0, 0]);
        assert_eq!(image_dimensions(&webp_x, ImageKind::Webp), Some((100, 50)));
        let mut webp_l = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0".to_vec();
        webp_l.push(0x2f);
        webp_l.extend_from_slice(&((31u32) | (15 << 14)).to_le_bytes());
        assert_eq!(image_dimensions(&webp_l, ImageKind::Webp), Some((32, 16)));
        let mut webp_y = b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0".to_vec();
        webp_y.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a]);
        webp_y.extend_from_slice(&[64, 0, 32, 0]);
        assert_eq!(image_dimensions(&webp_y, ImageKind::Webp), Some((64, 32)));
    }
}
