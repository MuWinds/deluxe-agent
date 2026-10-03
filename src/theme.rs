//! The window's dark colour scheme.
//!
//! egui ships a usable default palette, but a "dark theme" is not one colour —
//! it is a set of near-identical greys whose relationships carry the layout. So
//! the greys live here as named constants with a documented role, rather than
//! being sprinkled through the drawing code as literals.

use eframe::egui::{self, Color32, CornerRadius, Stroke};
use serde::{Deserialize, Serialize};

/// Which palette the window is drawn with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

impl ThemeChoice {
    /// The Chinese word shown in the menu's theme toggle.
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }
}

/// Every colour the UI draws with.
///
/// `Copy` and cheap to build, so callers take one per frame rather than holding
/// a reference to it.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// The far-left icon rail. The darkest surface, so it reads as behind the
    /// sidebar rather than beside it.
    pub rail_bg: Color32,
    /// The conversation sidebar. A step lighter than the rail.
    pub sidebar_bg: Color32,
    /// The transcript and composer area.
    pub main_bg: Color32,
    /// The background of the selected sidebar row.
    pub selected_bg: Color32,
    /// Row hover, and the fill of a raised control.
    pub hover_bg: Color32,
    /// The composer box and other inset surfaces.
    pub composer_bg: Color32,
    /// Hairline separators.
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    /// The running indicator, and the send button.
    pub accent: Color32,
    /// The user's own message bubble.
    pub bubble_user: Color32,
    /// A model or tool bubble.
    pub bubble_assistant: Color32,
    /// A bubble carrying a failure.
    pub bubble_notice: Color32,
    /// The body of a tool's code panel. Pushed away from the transcript so the
    /// panel reads as an inset surface rather than another bubble.
    pub code_bg: Color32,
    /// The panel's title bar, a step lighter than the body so the file name and
    /// the copy button sit on their own surface.
    pub code_header_bg: Color32,
    /// The wash behind a line a patch added.
    pub diff_add_bg: Color32,
    /// The wash behind a line a patch removed.
    pub diff_del_bg: Color32,
    /// Added-line text, and the `+n` in the title bar.
    pub diff_add_fg: Color32,
    /// Removed-line text, and the `-m` in the title bar.
    pub diff_del_fg: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

impl Palette {
    pub const fn dark() -> Self {
        Self {
            rail_bg: rgb(0x17, 0x17, 0x17),
            sidebar_bg: rgb(0x1f, 0x1f, 0x1f),
            main_bg: rgb(0x1a, 0x1a, 0x1a),
            selected_bg: rgb(0x2f, 0x2f, 0x2f),
            hover_bg: rgb(0x26, 0x26, 0x26),
            composer_bg: rgb(0x26, 0x26, 0x26),
            border: rgb(0x2e, 0x2e, 0x2e),
            text: rgb(0xe8, 0xe8, 0xe8),
            text_muted: rgb(0x9a, 0x9a, 0x9a),
            accent: rgb(0x3f, 0xb9, 0x50),
            bubble_user: rgb(0x2b, 0x3a, 0x4a),
            bubble_assistant: rgb(0x23, 0x23, 0x23),
            bubble_notice: rgb(0x3a, 0x24, 0x26),
            code_bg: rgb(0x16, 0x16, 0x16),
            code_header_bg: rgb(0x20, 0x20, 0x20),
            // Pre-blended rather than translucent: an alpha wash would blend
            // with whatever the panel happens to sit on, and these two are
            // tuned against `code_bg` specifically.
            diff_add_bg: rgb(0x1c, 0x2f, 0x20),
            diff_del_bg: rgb(0x35, 0x1f, 0x21),
            diff_add_fg: rgb(0x7e, 0xe7, 0x87),
            diff_del_fg: rgb(0xf0, 0x7b, 0x72),
        }
    }

    pub const fn light() -> Self {
        Self {
            rail_bg: rgb(0xe8, 0xe8, 0xe8),
            sidebar_bg: rgb(0xf2, 0xf2, 0xf2),
            main_bg: rgb(0xff, 0xff, 0xff),
            selected_bg: rgb(0xdc, 0xdc, 0xdc),
            hover_bg: rgb(0xe9, 0xe9, 0xe9),
            composer_bg: rgb(0xf4, 0xf4, 0xf4),
            border: rgb(0xd4, 0xd4, 0xd4),
            text: rgb(0x1a, 0x1a, 0x1a),
            text_muted: rgb(0x6b, 0x6b, 0x6b),
            accent: rgb(0x1a, 0x7f, 0x37),
            bubble_user: rgb(0xdb, 0xe7, 0xf5),
            bubble_assistant: rgb(0xf2, 0xf2, 0xf2),
            bubble_notice: rgb(0xfb, 0xe4, 0xe4),
            code_bg: rgb(0xfa, 0xfa, 0xfa),
            code_header_bg: rgb(0xf0, 0xf0, 0xf0),
            diff_add_bg: rgb(0xdc, 0xf5, 0xdd),
            diff_del_bg: rgb(0xfb, 0xdd, 0xdc),
            diff_add_fg: rgb(0x11, 0x63, 0x29),
            diff_del_fg: rgb(0x9b, 0x1c, 0x1c),
        }
    }
}

/// The palette for the chosen theme.
///
/// A free function rather than a method so the caller can pick a palette before
/// it has a `ThemeChoice` value in hand — the menu reads the choice only after
/// the frame that draws with the palette.
pub fn palette(choice: ThemeChoice) -> Palette {
    match choice {
        ThemeChoice::Dark => Palette::dark(),
        ThemeChoice::Light => Palette::light(),
    }
}

/// The green of a success indicator — a finished job, an applied patch.
pub const OK_GREEN: Color32 = rgb(0x2e, 0xa0, 0x43);

/// The amber of a warning — a job stopping, a denied command.
pub const WARN_AMBER: Color32 = rgb(0xd9, 0x8a, 0x00);

/// The red of a failure — a crashed job, a rejected command.
pub const BAD_RED: Color32 = rgb(0xc0, 0x39, 0x2b);

/// The window's type size, as a multiple of egui's own.
///
/// Every font size in the window — both the explicit `.size(…)` call sites and
/// the stock `TextStyle`s — is run through [`font`], so this one number is the
/// knob for how large the interface reads.
///
/// Only *type* is scaled. The layout's own metrics — row heights, padding, the
/// rail's width — keep their design values, because scaling those as well is
/// what `Context::set_zoom_factor` does. The window should read larger without
/// the interface zooming with it.
pub const FONT_SCALE: f32 = 1.15;

/// A design font size, scaled to the window's type size.
pub const fn font(size: f32) -> f32 {
    size * FONT_SCALE
}

/// Registers both palettes and switches to `choice`.
///
/// Both are installed because egui keeps a separate `Style` per theme: setting
/// only the active one leaves the other at the stock palette, so the first
/// toggle would land on colours that were never designed.
pub fn apply(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_visuals_of(egui::Theme::Dark, build(Palette::dark()));
    ctx.set_visuals_of(egui::Theme::Light, build(Palette::light()));
    ctx.all_styles_mut(scale_type);
    ctx.set_theme(match choice {
        ThemeChoice::Dark => egui::ThemePreference::Dark,
        ThemeChoice::Light => egui::ThemePreference::Light,
    });
}

/// Rebuilds the stock text styles at [`FONT_SCALE`].
///
/// Rebuilt from egui's defaults rather than scaled in place: `apply` runs again
/// on every theme toggle, so scaling whatever the styles currently hold would
/// compound and the type would creep up a step each time the theme changed.
fn scale_type(style: &mut egui::Style) {
    style.text_styles = egui::style::default_text_styles();
    for font_id in style.text_styles.values_mut() {
        font_id.size = font(font_id.size);
    }
}

fn build(p: Palette) -> egui::Visuals {
    let dark = p.main_bg.r() < 0x80;
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = p.main_bg;
    v.window_fill = p.sidebar_bg;
    v.extreme_bg_color = p.composer_bg;
    v.faint_bg_color = p.hover_bg;
    v.code_bg_color = p.code_bg;
    v.text_edit_bg_color = Some(Color32::TRANSPARENT);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.accent;

    // `ui.weak()` and friends read these, so a muted label does not need an
    // explicit colour at every call site.
    v.weak_text_color = Some(p.text_muted);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);

    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_stroke = Stroke::new(1.0, p.border);

    v.selection.bg_fill = p.accent.gamma_multiply(if dark { 0.30 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0, p.text);

    v.widgets.noninteractive.bg_fill = p.main_bg;
    v.widgets.noninteractive.weak_bg_fill = p.main_bg;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);

    for widget in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
        // The stock theme outlines every control; flat fills read closer to the
        // reference, and a stroke can still be added per widget where it earns
        // its place.
        widget.bg_stroke = Stroke::NONE;
    }

    v.widgets.inactive.bg_fill = p.hover_bg;
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);

    v.widgets.hovered.bg_fill = p.selected_bg;
    v.widgets.hovered.weak_bg_fill = p.selected_bg;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);

    v.widgets.active.bg_fill = p.selected_bg;
    v.widgets.active.weak_bg_fill = p.selected_bg;
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text);

    v.widgets.open.bg_fill = p.hover_bg;
    v.widgets.open.weak_bg_fill = p.hover_bg;
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);

    // One text rasterisation mode for both palettes, deliberately.
    //
    // egui bakes the glyph coverage curve into the font atlas, and its two
    // stock `Visuals` disagree on it: dark mode asks for
    // `TwoCoverageMinusCoverageSq` (white-on-black needs the boost) and light
    // mode for `Off`. `Fonts::begin_pass` treats *any* change in `TextOptions`
    // as a reason to throw the whole atlas away and rebuild every glyph the
    // next frame asks for, so letting the palettes disagree made every theme
    // switch re-rasterise every glyph in use — a stall proportional to the
    // conversation, and seconds long once a long session has pulled thousands
    // of CJK glyphs into the atlas.
    //
    // Pinning both to the dark-mode curve costs a slightly heavier face in the
    // light theme and buys a switch that rebuilds no glyphs at all.
    v.text_options.color_transfer_function =
        egui::epaint::FontColorTransferFunction::TwoCoverageMinusCoverageSq;

    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_palettes_are_distinguishable() {
        // Guards against a copy-paste slip that makes the theme toggle a no-op.
        assert_ne!(Palette::dark().main_bg, Palette::light().main_bg);
        assert_ne!(Palette::dark().text, Palette::light().text);
    }

    /// WCAG relative luminance.
    fn luminance(colour: Color32) -> f32 {
        let channel = |byte: u8| {
            let value = byte as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(colour.r()) + 0.7152 * channel(colour.g()) + 0.0722 * channel(colour.b())
    }

    fn contrast(one: Color32, other: Color32) -> f32 {
        let (one, other) = (luminance(one), luminance(other));
        let (lighter, darker) = if one > other {
            (one, other)
        } else {
            (other, one)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn diff_text_stays_readable_on_its_wash() {
        // The diff colours are picked by eye against one theme and then reused
        // in the other, which is exactly the kind of thing that silently ends
        // up unreadable. 3:1 is the large-text floor and 4.5:1 the body floor.
        for palette in [Palette::dark(), Palette::light()] {
            assert!(
                contrast(palette.diff_add_fg, palette.diff_add_bg) >= 3.0,
                "added-line text on its wash"
            );
            assert!(
                contrast(palette.diff_del_fg, palette.diff_del_bg) >= 3.0,
                "removed-line text on its wash"
            );
            assert!(
                contrast(palette.text, palette.code_bg) >= 4.5,
                "panel body text on the panel"
            );
        }
    }

    #[test]
    fn dark_is_detected_from_the_background_not_the_choice() {
        // `build` infers which base `Visuals` to start from by looking at the
        // palette, so a light palette must not produce a dark base.
        assert!(build(Palette::dark()).dark_mode);
        assert!(!build(Palette::light()).dark_mode);
    }

    #[test]
    fn the_two_themes_share_one_text_rasterisation_mode() {
        // A difference here makes egui drop the font atlas on every theme
        // switch and re-rasterise every glyph the next frame draws, which is
        // what made the toggle stall on a long conversation.
        assert_eq!(
            build(Palette::dark()).text_options,
            build(Palette::light()).text_options
        );
    }

    #[test]
    fn scaling_the_type_twice_does_not_compound() {
        // `apply` runs on every theme toggle, so a `scale_type` that read the
        // current sizes would grow the type by a step each time.
        let mut style = egui::Style::default();
        scale_type(&mut style);
        let body = style.text_styles[&egui::TextStyle::Body].size;
        assert_eq!(body, font(13.0));
        scale_type(&mut style);
        assert_eq!(style.text_styles[&egui::TextStyle::Body].size, body);
    }
}
