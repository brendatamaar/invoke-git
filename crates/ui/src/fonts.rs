//! The two bundled typefaces (docs/design/README.md, "Typography"): Inter for
//! the interface and JetBrains Mono for code, both under the SIL Open Font
//! License (the licence files sit next to the font files in
//! `assets/fonts/`). Only the weights the tokens use are embedded: Inter 400,
//! 500 and 600, JetBrains Mono 400.

use std::borrow::Cow;

use gpui::App;

const FILES: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
];

/// Registers the bundled fonts with GPUI. Call before the families are
/// resolved. A failure is not fatal: the stacks fall back to Segoe UI and
/// Cascadia Mono, so the error is returned for the caller to log.
pub fn register(cx: &App) -> Result<(), String> {
    cx.text_system()
        .add_fonts(FILES.iter().map(|bytes| Cow::Borrowed(*bytes)).collect())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TrueType file starts with the 4-byte version `0x00010000`, or
    /// `OTTO` for CFF outlines.
    #[test]
    fn every_embedded_file_is_a_font() {
        for (index, bytes) in FILES.iter().enumerate() {
            let tag = &bytes[..4];
            assert!(
                tag == [0, 1, 0, 0] || tag == *b"OTTO" || tag == *b"true",
                "file {index} does not start like a font: {tag:?}"
            );
            assert!(bytes.len() > 100_000, "file {index} is suspiciously small");
        }
    }
}
