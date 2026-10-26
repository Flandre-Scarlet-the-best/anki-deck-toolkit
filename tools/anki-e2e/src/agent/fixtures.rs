//! Синтетические данные сценариев и управляемые фикстуры баз Anki.
//!
//! Ожидания (поля, теги, байты media) зафиксированы здесь до прогона и не
//! вычисляются из результата проверяемой реализации.

use super::Fail;
use crate::util::sha256_hex;
use std::fs;
use std::path::Path;

pub const CONNECT: &str = "2055492159";
pub const CROWD: &str = "1788670778";
pub const MODEL: &str = "E2E Toolkit Model";
pub const DECK: &str = "E2E Toolkit Deck";
pub const MEDIA: &str = "e2e-toolkit-pixel.png";

pub struct NoteSpec {
    pub front: &'static str,
    pub back: &'static str,
    pub tags: &'static [&'static str],
}

pub const NOTES: &[NoteSpec] = &[
    NoteSpec {
        front: "e2e front one",
        back: "e2e back one",
        tags: &["e2e", "e2e::one"],
    },
    NoteSpec {
        front: "日本語の表 — unicode",
        back: "<b>bold</b> &amp; text",
        tags: &["e2e"],
    },
    NoteSpec {
        front: "image note",
        back: "<img src=\"e2e-toolkit-pixel.png\">",
        tags: &["e2e", "media"],
    },
];

/// Детерминированный PNG 8×8; байты известны до запуска Anki.
pub fn pixel_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 8, 8);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let data: Vec<u8> = (0..64u8)
            .flat_map(|i| [i * 4, 255 - i * 4, i.wrapping_mul(37)])
            .collect();
        writer.write_image_data(&data).expect("png data");
    }
    bytes
}

pub fn pixel_sha256() -> String {
    sha256_hex(&pixel_png())
}

pub fn copy_tree(from: &Path, to: &Path) -> Result<(), Fail> {
    let io = |e: std::io::Error| {
        Fail::error(
            "fixture_io",
            format!("{} → {}: {e}", from.display(), to.display()),
        )
    };
    fs::create_dir_all(to).map_err(io)?;
    for entry in fs::read_dir(from).map_err(io)? {
        let entry = entry.map_err(io)?;
        let kind = entry.file_type().map_err(io)?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).map_err(io)?;
        } else {
            return Err(Fail::error(
                "fixture_io",
                format!("Неожиданный тип файла {}", entry.path().display()),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_png_is_stable_and_decodable() {
        assert_eq!(pixel_png(), pixel_png());
        let decoder = png::Decoder::new(std::io::Cursor::new(pixel_png()));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (8, 8));
    }
}
