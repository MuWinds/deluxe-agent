//! Font installation.
//!
//! Three things are loaded here, into one `FontDefinitions`:
//!
//! * the **icon font**, bundled in the `egui_phosphor` crate;
//! * a **CJK fallback** — a subset of Noto Sans SC, embedded in the binary; and
//! * a **bold face** — the same subset's bold cut, registered under
//!   [`BOLD_FAMILY`] so Markdown's `**emphasis**` can change weight.
//!
//! The CJK face is embedded rather than borrowed from the host on purpose. The
//! host approach had two costs. The obvious one is memory: loading Microsoft
//! YaHei meant 19 MB of face bytes plus 17 MB of bold, held for the process
//! lifetime and parsed into a large `skrifa` cache. The subtler one is looks:
//! the bold face was a *different* family from the regular one, so a run that
//! switched weight — or any symbol the regular host face happened to lack —
//! picked its glyphs from a second typeface, which reads as a jarring weight and
//! shape change mid-sentence. One bundled family, both weights cut from the same
//! variable font, keeps every glyph on one typeface.
//!
//! The embedded subset carries the Hanzi, CJK punctuation and fullwidth forms
//! Chinese needs, plus the arrows, box drawing and geometric/dingbat symbols the
//! UI draws — so a weight switch never leaves the family. Latin letters are
//! deliberately *not* in it: egui's own default faces are kept ahead of it, so
//! Latin keeps egui's metrics and this face only supplies what they lack.
//!
//! Noto Sans SC is distributed under the SIL Open Font License 1.1; see
//! `assets/fonts/NOTICE.txt` for attribution, the upstream source, and the exact
//! subsetting command used to produce the two files.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily, FontId};

/// The bundled CJK fallback, subset to the coverage above.
const CJK_FONT: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-Regular.otf");

/// The bundled bold face, the same subset cut at weight 700.
const BOLD_FONT: &[u8] = include_bytes!("../assets/fonts/NotoSansSC-Bold.otf");

/// The name the CJK face is registered under inside egui.
const CJK_FACE: &str = "cjk";

/// The name the bold face is registered under inside egui.
const BOLD_FACE: &str = "bold-face";

/// The family `**bold**` is drawn with.
///
/// Private to this module in spirit: callers go through [`bold`] rather than
/// naming the family, so the name stays an implementation detail of the
/// installation above.
const BOLD_FAMILY: &str = "bold";

/// A `FontId` in the bold face.
///
/// Always usable, because [`definitions`] binds the family: a `FontFamily` with
/// no fonts behind it panics on the first glyph lookup.
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD_FAMILY.into()))
}

/// Installs the icon font, the bundled CJK fallback and the bundled bold face.
pub fn install(ctx: &eframe::egui::Context) {
    ctx.set_fonts(definitions());
}

/// Builds the `FontDefinitions` [`install`] hands to egui.
///
/// Split from [`install`] so a test can lay text out with the very definitions
/// the app uses, without an egui `Context`.
fn definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    // Everything goes into this one struct on purpose. `set_fonts` replaces the
    // whole definition, so installing a face in a second call would silently
    // discard the rest.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    fonts.font_data.insert(
        CJK_FACE.to_owned(),
        Arc::new(FontData::from_static(CJK_FONT)),
    );
    fonts.font_data.insert(
        BOLD_FACE.to_owned(),
        Arc::new(FontData::from_static(BOLD_FONT)),
    );

    // Appended rather than prepended: Latin keeps egui's own metrics, and only
    // the codepoints the earlier faces lack — Chinese, and the symbols egui's
    // defaults carry no glyph for — fall through to this one. Phosphor's icons
    // live in the Private Use Area, so the two never compete for a codepoint.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(CJK_FACE.to_owned());
    }

    // Read after the CJK face has been appended, so the bold fallback chain
    // carries it too.
    let regular = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    // The bold face first, then the whole regular chain. Anything the bold face
    // lacks — Latin, emoji, phosphor glyphs — resolves exactly as it does in
    // regular text, so a bold run is never left with a tofu box.
    let mut chain = vec![BOLD_FACE.to_owned()];
    chain.extend(regular);
    fonts
        .families
        .insert(FontFamily::Name(BOLD_FAMILY.into()), chain);

    fonts
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::text::{Fonts, LayoutJob, TextOptions};
    use eframe::egui::{Color32, FontId};

    /// Lays `text` out with `face` as the *only* font in the proportional
    /// family, and returns the total advance width.
    ///
    /// Replacing the family rather than appending means the width can only come
    /// from the face under test: if it is truncated or unparsable, `cmap` may
    /// still claim the codepoints while `hmtx` is unreadable, and the glyphs are
    /// laid out at zero advance instead of failing loudly.
    fn width_of(face: &'static [u8], name: &str, text: &str) -> f32 {
        let mut fonts = FontDefinitions::default();
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(face)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.insert(family, vec![name.to_owned()]);
        }

        let mut fonts = Fonts::new(TextOptions::default(), fonts);
        let mut view = fonts.with_pixels_per_point(1.0);
        let job = LayoutJob::simple_singleline(
            text.to_owned(),
            FontId::proportional(20.0),
            Color32::WHITE,
        );
        view.layout_job(job)
            .rows
            .iter()
            .map(|row| row.rect().width())
            .sum()
    }

    /// Guards against a truncated or otherwise unparsable bundled face, and
    /// proves the subset carries the Chinese the UI draws from this family —
    /// both Simplified and Traditional. A subset cut to GB2312 alone passes a
    /// Simplified-only probe but leaves Traditional text as tofu boxes, which is
    /// exactly the regression this catches.
    #[test]
    fn bundled_regular_face_covers_cjk_and_symbols() {
        let width = width_of(CJK_FONT, CJK_FACE, "本地工具桥接 編證譯驗 — → ├ ● ✓");
        assert!(
            width > 0.0,
            "the bundled CJK face laid out zero-width glyphs; \
             assets/fonts/NotoSansSC-Regular.otf is probably truncated"
        );
    }

    /// The bold face is a separate file, so it needs the same guard.
    #[test]
    fn bundled_bold_face_covers_cjk_and_symbols() {
        let width = width_of(BOLD_FONT, BOLD_FACE, "**本地工具桥接** 編證譯驗 — → ├ ● ✓");
        assert!(
            width > 0.0,
            "the bundled bold face laid out zero-width glyphs; \
             assets/fonts/NotoSansSC-Bold.otf is probably truncated"
        );
    }

    /// The bold family must be bound, and must fall through to the same chain as
    /// regular text, or a bold glyph the subset lacks would panic or go tofu.
    #[test]
    fn bold_family_is_bound_and_falls_through() {
        let fonts = definitions();
        let chain = fonts
            .families
            .get(&FontFamily::Name(BOLD_FAMILY.into()))
            .expect("the bold family is not bound; the first bold glyph would panic");
        assert_eq!(chain.first().map(String::as_str), Some(BOLD_FACE));
        assert!(
            chain.iter().any(|name| name == CJK_FACE),
            "the bold chain does not fall through to the CJK face"
        );
    }
}
