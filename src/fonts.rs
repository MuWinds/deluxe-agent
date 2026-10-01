//! Font installation.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily};

/// The name the CJK face is registered under inside egui.
const CJK_FACE: &str = "cjk";

/// The system faces to try, in order of preference.
#[cfg(target_os = "windows")]
const CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\msyh.ttc",   // Microsoft YaHei
    r"C:\Windows\Fonts\simsun.ttc", // SimSun
    r"C:\Windows\Fonts\simhei.ttf", // SimHei
    r"C:\Windows\Fonts\msjh.ttc",   // Microsoft JhengHei (Traditional)
];

#[cfg(target_os = "macos")]
const CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
];

#[cfg(target_os = "linux")]
const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
];

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const CANDIDATES: &[&str] = &[];

/// Installs the icon font and the host's CJK fallback.
pub fn install(ctx: &eframe::egui::Context) {
    ctx.set_fonts(definitions(load_cjk()));
}

/// Builds the `FontDefinitions` [`install`] hands to egui.
///
/// Split from [`install`] so a test can build the definitions without an egui
/// `Context` and without depending on the host having a CJK font.
fn definitions(cjk: Option<Vec<u8>>) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    // Everything goes into this one struct on purpose. `set_fonts` replaces the
    // whole definition, so installing a face in a second call would silently
    // discard the rest.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    if let Some(bytes) = cjk {
        // `from_owned` uses face index 0, which is the regular cut in the
        // `.ttc` collections listed above (`skrifa::FontRef::from_index`).
        fonts
            .font_data
            .insert(CJK_FACE.to_owned(), Arc::new(FontData::from_owned(bytes)));

        // Proportional: the CJK face leads, so the fullwidth marks `Ubuntu-Light`
        // carries (`！？（），Ａ１` …) resolve to the same typeface as the Hanzi.
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, CJK_FACE.to_owned());

        // Monospace: `Hack` leads so code Latin stays monospaced; the CJK face
        // goes right after it, still ahead of `Ubuntu-Light`'s fullwidth marks.
        let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
        let after_first = if monospace.is_empty() { 0 } else { 1 };
        monospace.insert(after_first, CJK_FACE.to_owned());
    }

    fonts
}

/// Reads the first face in [`CANDIDATES`] that can be read.
fn load_cjk() -> Option<Vec<u8>> {
    first_readable(CANDIDATES)
}

/// Returns the bytes of the first path in `candidates` that can be read.
///
/// Unreadable candidates — the common case on a machine with no CJK font — are
/// skipped rather than reported, so a partial list still finds a later face.
fn first_readable(candidates: &[&str]) -> Option<Vec<u8>> {
    candidates.iter().find_map(|path| std::fs::read(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader must skip a candidate it cannot read and take the first one
    /// it can, so a missing preferred face falls through instead of aborting.
    #[test]
    fn first_readable_skips_missing_and_takes_the_next() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let present = dir.path().join("present.otf");
        std::fs::write(&present, b"face bytes").expect("the temp file is writable");

        let missing = dir.path().join("missing.otf");
        let missing = missing.to_str().expect("the path is valid UTF-8");
        let present = present.to_str().expect("the path is valid UTF-8");

        assert_eq!(
            first_readable(&[missing, present]),
            Some(b"face bytes".to_vec()),
            "the loader did not fall through to the readable candidate"
        );
    }

    /// No readable candidate is a normal outcome, not an error.
    #[test]
    fn first_readable_returns_none_when_nothing_is_readable() {
        assert_eq!(first_readable(&["/no/such/font/on/this/machine.otf"]), None);
    }

    /// With no host face, egui's defaults are left alone — no `cjk` entry and
    /// no dangling family reference.
    #[test]
    fn definitions_without_a_face_registers_no_cjk() {
        let fonts = definitions(None);
        assert!(
            !fonts.font_data.contains_key(CJK_FACE),
            "a `cjk` face was registered with no bytes behind it"
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let chain = fonts
                .families
                .get(&family)
                .expect("egui binds both families");
            assert!(
                !chain.iter().any(|name| name == CJK_FACE),
                "the {family:?} chain references a face that was never inserted"
            );
        }
    }

    /// A host face must outrank `Ubuntu-Light`, which carries a few fullwidth
    /// marks: first in the proportional family, and right after `Hack` in the
    /// monospace family so code Latin keeps its monospaced advance.
    #[test]
    fn definitions_with_a_face_outranks_ubuntu_light() {
        let fonts = definitions(Some(vec![0u8; 4]));
        assert!(fonts.font_data.contains_key(CJK_FACE));

        let proportional = fonts
            .families
            .get(&FontFamily::Proportional)
            .expect("egui binds both families");
        assert_eq!(
            proportional.first().map(String::as_str),
            Some(CJK_FACE),
            "the proportional chain does not lead with the CJK face, so \
             fullwidth marks would be drawn by `Ubuntu-Light`"
        );

        let monospace = fonts
            .families
            .get(&FontFamily::Monospace)
            .expect("egui binds both families");
        assert_eq!(
            monospace.first().map(String::as_str),
            Some("Hack"),
            "the monospace chain must still lead with the monospaced Latin face"
        );
        assert_eq!(
            monospace.get(1).map(String::as_str),
            Some(CJK_FACE),
            "the CJK face is not ahead of `Ubuntu-Light` in the monospace chain"
        );
    }
}
