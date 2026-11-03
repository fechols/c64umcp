//! Text-only screen snapshots, independent of the VIC video stream.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

const WIDTH: usize = 40;
const HEIGHT: usize = 25;
const CELLS: usize = WIDTH * HEIGHT;

fn rows(bytes: &[u8], decode: fn(u8) -> char) -> Vec<String> {
    bytes
        .chunks_exact(WIDTH)
        .map(|row| row.iter().map(|&byte| decode(byte)).collect())
        .collect()
}

fn menu_char(byte: u8) -> char {
    match byte & 0x7f {
        printable @ 32..=126 => printable as char,
        _ => '\u{fffd}',
    }
}

fn screen_char(byte: u8) -> char {
    match byte & 0x7f {
        0 => '@',
        code @ 1..=26 => (b'A' + code - 1) as char,
        27 => '[',
        28 => '£',
        29 => ']',
        30 => '↑',
        31 => '←',
        printable @ 32..=63 => printable as char,
        // Shifted space: blank in both character sets.
        96 => ' ',
        _ => '\u{fffd}',
    }
}

/// Whether the CPU's memory map shows I/O (and so color RAM) at $D000, from
/// the processor port's direction ($00) and data ($01) registers. Port lines
/// set as inputs read high. With LORAM and HIRAM both low, or CHAREN low,
/// $D800 reads plain RAM or character ROM instead of color RAM.
pub fn io_visible(direction: u8, data: u8) -> bool {
    let lines = ((data & direction) | !direction) & 0x07;
    lines & 0x03 != 0 && lines & 0x04 != 0
}

pub fn decode_menu(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() == CELLS * 2,
        "menu screen must contain exactly 2000 bytes, received {}",
        bytes.len()
    );
    let (chars, attrs) = bytes.split_at(CELLS);
    let text_rows = rows(chars, menu_char);
    Ok(json!({
        "kind": "firmware_menu",
        "width": WIDTH, "height": HEIGHT,
        "text": text_rows.join("\n"), "rows": text_rows,
        "characters": chars, "attributes": attrs,
        "reverse": chars.iter().map(|c| c & 0x80 != 0).collect::<Vec<_>>(),
        "foreground": attrs.iter().map(|c| c & 0x0f).collect::<Vec<_>>(),
        "background": attrs.iter().map(|c| c >> 4).collect::<Vec<_>>(),
        "decoding_note": "Firmware UI glyphs without a printable character use U+FFFD; raw characters are preserved."
    }))
}

pub fn decode_text(bytes: &[u8], colors: Option<&[u8]>) -> Result<Value> {
    ensure!(
        bytes.len() == CELLS,
        "text screen must contain exactly 1000 screen codes, received {}",
        bytes.len()
    );
    if let Some(colors) = colors {
        ensure!(
            colors.len() == CELLS,
            "color RAM snapshot must contain exactly 1000 bytes, received {}",
            colors.len()
        );
    }
    let text_rows = rows(bytes, screen_char);
    Ok(json!({
        "kind": "c64_text_memory",
        "width": WIDTH, "height": HEIGHT,
        "text": text_rows.join("\n"), "rows": text_rows,
        "screen_codes": bytes,
        "reverse": bytes.iter().map(|c| c & 0x80 != 0).collect::<Vec<_>>(),
        "colors": colors.map(|values| values.iter().map(|c| c & 0x0f).collect::<Vec<_>>()),
        "decoding_note": "Assumes the default uppercase/graphics C64 character set. Unmapped graphics use U+FFFD. This is a memory snapshot, not a rendering of bitmap graphics, sprites, custom glyphs, or raster changes."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_decodes_ascii_reverse_and_packed_attributes() {
        let mut bytes = vec![b' '; CELLS * 2];
        bytes[0] = b'A';
        bytes[1] = b'B' | 0x80;
        bytes[2] = 1;
        bytes[CELLS] = 0x25;
        let result = decode_menu(&bytes).unwrap();
        assert!(result["rows"][0].as_str().unwrap().starts_with("AB�"));
        assert_eq!(result["reverse"][1], true);
        assert_eq!(result["foreground"][0], 5);
        assert_eq!(result["background"][0], 2);
        assert_eq!(result["characters"][1], b'B' | 0x80);
        assert_eq!(result["rows"].as_array().unwrap().len(), 25);
    }

    #[test]
    fn c64_codes_are_not_ascii_and_colors_are_nibble_values() {
        let mut bytes = vec![32; CELLS];
        bytes[..8].copy_from_slice(&[0, 1, 26, 0x81, 28, 30, 31, 64]);
        let colors = vec![0xf6; CELLS];
        let result = decode_text(&bytes, Some(&colors)).unwrap();
        assert!(result["rows"][0].as_str().unwrap().starts_with("@AZA£↑←�"));
        assert_eq!(result["reverse"][3], true);
        assert_eq!(result["colors"][0], 6);
        assert!(decode_text(&bytes, None).unwrap()["colors"].is_null());
    }

    #[test]
    fn shifted_space_decodes_as_space_in_both_polarities() {
        let mut bytes = vec![32; CELLS];
        bytes[..3].copy_from_slice(&[96, 224, 1]);
        let result = decode_text(&bytes, None).unwrap();
        assert!(result["rows"][0].as_str().unwrap().starts_with("  A"));
        assert_eq!(result["reverse"][1], true);
    }

    #[test]
    fn io_visibility_follows_the_processor_port() {
        // KERNAL default: $01 = $37 with direction $2F.
        assert!(io_visible(0x2f, 0x37));
        // All RAM, and RAM with character ROM: no I/O.
        assert!(!io_visible(0x2f, 0x34));
        assert!(!io_visible(0x2f, 0x30));
        assert!(!io_visible(0x2f, 0x33));
        // Lines set as inputs read high whatever was written.
        assert!(io_visible(0x00, 0x00));
        assert!(io_visible(0x28, 0x30));
    }

    #[test]
    fn rejects_truncated_and_oversized_matrices() {
        for len in [0, 999, 1001] {
            assert!(decode_text(&vec![0; len], None).is_err());
        }
        for len in [0, 1999, 2001] {
            assert!(decode_menu(&vec![0; len]).is_err());
        }
        assert!(decode_text(&vec![32; CELLS], Some(&[0; 999])).is_err());
    }
}
