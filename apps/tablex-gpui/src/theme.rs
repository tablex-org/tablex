use crate::settings::ThemeChoice;
use gpui::{App, Rgba, WindowAppearance};
use gpui_component::highlighter::LanguageRegistry;
use gpui_component::theme::{Theme, ThemeMode};
use std::cell::Cell;

pub const SIDEBAR_WIDTH: f32 = 260.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 800.0;
pub const TAB_HEIGHT: f32 = 36.0;
/// Group-chip row above the tabs in the grouped-rows strip (chips are 22px).
pub const GROUP_ROW_HEIGHT: f32 = 28.0;
pub const STATUS_HEIGHT: f32 = 22.0;
pub const ROW_HEIGHT: f32 = 26.0;
pub const SIDEBAR_ROW_HEIGHT: f32 = 28.0;
pub const SIDEBAR_INDENT: f32 = 16.0;
pub const SIDEBAR_ICON_SIZE: f32 = 14.0;
pub const HEADER_HEIGHT: f32 = 28.0;
pub const RESULT_TOOLBAR_HEIGHT: f32 = 36.0;
pub const RESULT_GRID_HEADER_HEIGHT: f32 = 42.0;
pub const RESULT_ACTION_HEIGHT: f32 = 32.0;
pub const RESULT_STATUS_HEIGHT: f32 = 22.0;
pub const RESULT_FOOTER_HEIGHT: f32 = RESULT_ACTION_HEIGHT + RESULT_STATUS_HEIGHT;

const fn rgb(hex: u32) -> Rgba {
    rgba(hex << 8 | 0xff)
}

const fn channel(hex: u32, shift: u32) -> f32 {
    ((hex >> shift) & 0xff) as f32 / 255.0
}

const fn rgba(hex: u32) -> Rgba {
    Rgba { r: channel(hex, 24), g: channel(hex, 16), b: channel(hex, 8), a: channel(hex, 0) }
}

/// One color table, two columns: every token is declared with its Dark and
/// its Light value side by side, so a token can never exist in only one mode.
/// Each token also gets a free function (`theme::text()`) that reads the
/// active palette; call sites stay unchanged and never allocate or lock.
macro_rules! palette {
    ($($(#[$doc:meta])* $name:ident: $dark:expr, $light:expr;)*) => {
        /// The TableX colors of one appearance.
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct Palette {
            $($(#[$doc])* pub $name: Rgba,)*
        }

        static DARK: Palette = Palette { $($name: $dark,)* };
        static LIGHT: Palette = Palette { $($name: $light,)* };

        impl Palette {
            /// `(token name, color)` of every token, for completeness and
            /// contrast checks.
            #[cfg(test)]
            fn tokens(&self) -> Vec<(&'static str, Rgba)> {
                vec![$((stringify!($name), self.$name)),*]
            }
        }

        $($(#[$doc])* pub fn $name() -> Rgba { palette().$name })*
    };
}

palette! {
    background: rgb(0x131416), rgb(0xffffff);
    chrome: rgb(0x1b1b1e), rgb(0xeceef1);
    sidebar: rgb(0x19191c), rgb(0xf3f4f6);
    surface: rgb(0x1f2023), rgb(0xf4f5f7);
    grid_header: rgb(0x26272b), rgb(0xeef0f3);
    grid_alt_row: rgba(0xffffff08), rgba(0x0f172a08);
    grid_gutter: rgb(0x202125), rgb(0xf4f5f7);
    type_text: rgb(0x6ca6d8), rgb(0x2563a8);
    type_number: rgb(0x74c69d), rgb(0x17794c);
    type_object: rgb(0xc9a36a), rgb(0x855f10);
    hover: rgb(0x2a2a2d), rgb(0xe8eaee);
    selected: rgb(0x303238), rgb(0xd9dee7);
    text: rgb(0xd7d7db), rgb(0x1f2329);
    text_muted: rgb(0x9c9da2), rgb(0x555b66);
    text_faint: rgb(0x6e7076), rgb(0x687080);
    border: rgba(0xffffff28), rgba(0x0f172a36);
    /// Hairline between rows inside a panel (fainter than `border`).
    divider: rgba(0xffffff0a), rgba(0x0f172a14);
    /// Outline of floating panels (stronger than `border`).
    border_strong: rgba(0xffffff33), rgba(0x0f172a40);
    accent: rgb(0x89addf), rgb(0x2a62b5);
    accent_surface: rgb(0x27364a), rgb(0xdbe7f8);
    /// Translucent accent outline (a diagram entity related to the selection).
    accent_soft: rgba(0x89addf88), rgba(0x2a62b588);
    pending_cell: rgba(0xfbbf2429), rgba(0xd9770638);
    /// In-grid find: every matching cell / header.
    find_match: rgba(0xfacc1538), rgba(0xfacc1566);
    /// In-grid find: the current match (stronger than `find_match`).
    find_match_current: rgba(0xfacc1599), rgba(0xf59e0baa);
    danger: rgb(0xf3625f), rgb(0xc8312e);
    success: rgb(0x4ade80), rgb(0x12763a);
    warning: rgb(0xfbbf24), rgb(0x955f00);
    /// Navigator / palette object-kind glyphs (not brand colors: they follow
    /// the theme so they stay visible on both backgrounds).
    icon_folder: rgb(0xf59e0b), rgb(0xb45f06);
    icon_database: rgb(0xeab308), rgb(0x9a7400);
    icon_schema: rgb(0x38bdf8), rgb(0x0b78b0);
    icon_table: rgb(0x22c55e), rgb(0x15803d);
    icon_view: rgb(0xa855f7), rgb(0x7e22ce);
    icon_collection: rgb(0x4ade80), rgb(0x178a43);
    icon_bucket: rgb(0x06b6d6), rgb(0x0e7490);
    /// Series colors of the result charts (`gpui-component`'s `chart_1..chart_5`): each is 3:1
    /// against the background (graphics, not text).
    chart_1: rgb(0x89addf), rgb(0x2a62b5);
    chart_2: rgb(0x74c69d), rgb(0x17794c);
    chart_3: rgb(0xe0a458), rgb(0xb36b00);
    chart_4: rgb(0xc792ea), rgb(0x7e3fb5);
    chart_5: rgb(0xf0707a), rgb(0xc8312e);
}

thread_local! {
    // GPUI draws on the main thread only; a `Cell` read is a plain load (no
    // lock, no allocation) and keeps parallel test threads independent.
    static ACTIVE: Cell<ThemeMode> = const { Cell::new(ThemeMode::Dark) };
}

/// The active palette: one thread-local load and a branch.
#[inline]
pub fn palette() -> &'static Palette {
    if ACTIVE.with(Cell::get).is_dark() { &DARK } else { &LIGHT }
}

/// The mode the TableX palette (and the component theme) currently show.
pub fn mode() -> ThemeMode {
    ACTIVE.with(Cell::get)
}

/// The mode a theme choice shows given the OS appearance.
pub fn resolve(choice: ThemeChoice, system: WindowAppearance) -> ThemeMode {
    match choice {
        ThemeChoice::Dark => ThemeMode::Dark,
        ThemeChoice::Light => ThemeMode::Light,
        ThemeChoice::System => ThemeMode::from(system),
    }
}

/// Registers the SQL grammar so code editors can request "sql" highlighting
/// without pulling in every tree-sitter language via the component feature.
/// The language registry is a process-global singleton; no App handle needed.
pub fn init(cx: &mut App) {
    apply(ThemeMode::Dark, cx);
    register_sql_language();
}

/// Makes `mode` the TableX palette and re-derives the component theme from
/// it (stock theme of that mode, then the TableX overrides), refreshing every
/// window. Called at startup, on a Settings change and when the OS appearance
/// changes under `System`.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    ACTIVE.with(|active| active.set(mode));
    Theme::change(mode, None, cx);
    // `Theme::update` is the write path that keeps `colors`, `tokens` and the
    // Base projection in step: components such as Dialog paint from `tokens`
    // (e.g. `tokens.background`), so they must follow these color overrides.
    Theme::update(cx, |theme| {
        let colors = &mut theme.colors;
        colors.background = background().into();
        colors.foreground = text().into();
        colors.border = border().into();
        colors.input = border().into();
        colors.popover = surface().into();
        colors.popover_foreground = text().into();
        colors.secondary = surface().into();
        colors.secondary_foreground = text().into();
        colors.secondary_hover = hover().into();
        colors.secondary_active = selected().into();
        colors.accent = selected().into();
        colors.accent_foreground = text().into();
        colors.muted_foreground = text_muted().into();
        colors.primary = accent().into();
        colors.primary_foreground = background().into();
        colors.chart_1 = chart_1().into();
        colors.chart_2 = chart_2().into();
        colors.chart_3 = chart_3().into();
        colors.chart_4 = chart_4().into();
        colors.chart_5 = chart_5().into();
        colors.chart_grid = border().into();
        apply_syntax_colors(theme);
    });
}

fn hex(color: Rgba) -> String {
    let rgb = u32::from(color) >> 8;
    format!("#{rgb:06x}")
}

/// Tints the component highlight theme (used by every tree-sitter code view)
/// with TableX tokens so JSON keys, strings, numbers, booleans and null stay
/// visually distinct: keys red, strings green, numbers amber, booleans blue.
fn apply_syntax_colors(theme: &mut Theme) {
    let mut highlight = (*theme.highlight_theme).clone();
    let style = |color: Rgba| -> Option<gpui_component::highlighter::ThemeStyle> {
        serde_json::from_value(serde_json::json!({ "color": hex(color) })).ok()
    };
    let syntax = &mut highlight.style.syntax;
    syntax.property = style(danger());
    syntax.string = style(success());
    syntax.number = style(warning());
    syntax.boolean = style(accent());
    syntax.constant = style(text_faint());
    syntax.punctuation = style(text_muted());
    syntax.punctuation_bracket = style(text_muted());
    syntax.punctuation_delimiter = style(text_muted());
    theme.highlight_theme = std::sync::Arc::new(highlight);
}

fn register_sql_language() {
    LanguageRegistry::singleton().register(
        "sql",
        &gpui_component::highlighter::LanguageConfig::new(
            "sql",
            tree_sitter_sequel::LANGUAGE.into(),
            vec![],
            tree_sitter_sequel::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::{Palette, DARK, LIGHT};
    use gpui::{Rgba, TestAppContext};
    use gpui_component::theme::ThemeMode;

    /// WCAG relative luminance of an opaque color.
    fn luminance(color: Rgba) -> f32 {
        let linear = |c: f32| {
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    /// `foreground` (alpha-composited over `background`) against `background`.
    fn contrast(foreground: Rgba, background: Rgba) -> f32 {
        let a = foreground.a;
        let flat = Rgba {
            r: foreground.r * a + background.r * (1.0 - a),
            g: foreground.g * a + background.g * (1.0 - a),
            b: foreground.b * a + background.b * (1.0 - a),
            a: 1.0,
        };
        let (l1, l2) = (luminance(flat), luminance(background));
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    fn token(palette: &Palette, name: &str) -> Rgba {
        palette
            .tokens()
            .into_iter()
            .find(|(token, _)| *token == name)
            .unwrap_or_else(|| panic!("no token {name}"))
            .1
    }

    #[test]
    fn every_token_is_defined_in_both_palettes_and_differs_per_mode() {
        let (dark, light) = (DARK.tokens(), LIGHT.tokens());
        assert_eq!(
            dark.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            light.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        assert!(dark.len() >= 30, "the palette lost tokens: {}", dark.len());
        for ((name, dark), (_, light)) in dark.into_iter().zip(light) {
            for channel in [dark.r, dark.g, dark.b, dark.a, light.r, light.g, light.b, light.a] {
                assert!((0.0..=1.0).contains(&channel), "{name} has a channel out of range");
            }
            assert_ne!(dark, light, "{name} is the same in Dark and Light (copy-paste?)");
        }
    }

    /// Text-on-background pairs meet WCAG AA (4.5:1) in both palettes; the
    /// de-emphasized `text_faint` (hints, NULL) needs 3:1 (large / non-text).
    #[test]
    fn text_on_background_pairs_meet_contrast_targets_in_both_palettes() {
        let surfaces = [
            "background", "chrome", "sidebar", "surface", "grid_header", "grid_gutter", "hover",
            "selected", "accent_surface",
        ];
        for (mode, palette) in [("dark", &DARK), ("light", &LIGHT)] {
            for surface in surfaces {
                let ground = token(palette, surface);
                let base = !matches!(surface, "hover" | "selected" | "accent_surface");
                for (name, minimum) in [("text", 4.5), ("text_muted", 4.5), ("text_faint", 3.0)] {
                    // Hints are never drawn on a hovered / selected row.
                    if name == "text_faint" && !base {
                        continue;
                    }
                    let ratio = contrast(token(palette, name), ground);
                    assert!(ratio >= minimum, "{mode}: {name} on {surface} is {ratio:.2}:1 (< {minimum})");
                }
            }
            // Semantic and type colors are drawn as text on the main surfaces.
            for surface in ["background", "chrome", "sidebar", "surface", "grid_header"] {
                let ground = token(palette, surface);
                for name in [
                    "accent", "danger", "success", "warning", "type_text", "type_number",
                    "type_object",
                ] {
                    let ratio = contrast(token(palette, name), ground);
                    assert!(ratio >= 4.5, "{mode}: {name} on {surface} is {ratio:.2}:1 (< 4.5)");
                }
            }
            // Object-kind glyphs are graphics: 3:1 on the sidebar.
            for name in [
                "icon_folder", "icon_database", "icon_schema", "icon_table", "icon_view",
                "icon_collection", "icon_bucket",
            ] {
                let ratio = contrast(token(palette, name), token(palette, "sidebar"));
                assert!(ratio >= 3.0, "{mode}: {name} on sidebar is {ratio:.2}:1 (< 3)");
            }
            // Chart series are graphics: 3:1 on the background.
            for name in ["chart_1", "chart_2", "chart_3", "chart_4", "chart_5"] {
                let ratio = contrast(token(palette, name), token(palette, "background"));
                assert!(ratio >= 3.0, "{mode}: {name} on background is {ratio:.2}:1 (< 3)");
            }
            // The border must be visible against the surfaces it separates.
            for surface in ["background", "sidebar", "surface"] {
                let ratio = contrast(token(palette, "border"), token(palette, surface));
                assert!(ratio >= 1.25, "{mode}: border on {surface} is {ratio:.2}:1");
            }
        }
    }

    #[gpui::test]
    fn applying_a_mode_switches_the_palette_and_the_component_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            assert_eq!(super::mode(), ThemeMode::Dark);
            assert_eq!(super::background(), DARK.background);

            super::apply(ThemeMode::Light, cx);
            assert_eq!(super::mode(), ThemeMode::Light);
            assert_eq!(super::background(), LIGHT.background);
            assert_eq!(super::text(), LIGHT.text);
            let theme = gpui_component::theme::Theme::global(cx);
            assert!(!theme.is_dark());
            assert_eq!(gpui::Hsla::from(theme.tokens.background), gpui::Hsla::from(LIGHT.background));
            assert_eq!(gpui::Hsla::from(theme.tokens.popover), gpui::Hsla::from(LIGHT.surface));
            assert_eq!(
                theme.tokens,
                gpui_component::theme::ThemeTokens::from(&theme.colors),
                "component tokens stay in step with the colors"
            );

            super::apply(ThemeMode::Dark, cx);
            assert_eq!(super::background(), DARK.background);
            assert!(gpui_component::theme::Theme::global(cx).is_dark());
        });
    }

    #[gpui::test]
    fn syntax_colors_follow_the_mode(cx: &mut TestAppContext) {
        use gpui_component::input::HighlightStyleResolver;
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            for (mode, palette) in [(ThemeMode::Light, &LIGHT), (ThemeMode::Dark, &DARK)] {
                super::apply(mode, cx);
                let highlight = gpui_component::theme::Theme::global(cx).highlight_theme.clone();
                let color = |name: &str| highlight.style(name).and_then(|style| style.color);
                assert_eq!(color("property"), Some(palette.danger.into()));
                assert_eq!(color("string"), Some(palette.success.into()));
                assert_eq!(color("number"), Some(palette.warning.into()));
            }
        });
    }

    #[test]
    fn a_choice_resolves_against_the_os_appearance() {
        use crate::settings::ThemeChoice::{Dark, Light, System};
        use gpui::WindowAppearance as Os;
        assert_eq!(super::resolve(Dark, Os::Light), ThemeMode::Dark);
        assert_eq!(super::resolve(Light, Os::Dark), ThemeMode::Light);
        assert_eq!(super::resolve(System, Os::Dark), ThemeMode::Dark);
        assert_eq!(super::resolve(System, Os::VibrantDark), ThemeMode::Dark);
        assert_eq!(super::resolve(System, Os::Light), ThemeMode::Light);
        assert_eq!(super::resolve(System, Os::VibrantLight), ThemeMode::Light);
    }

    #[test]
    fn sql_language_is_registered_in_the_component_registry() {
        super::register_sql_language();

        let registry = gpui_component::highlighter::LanguageRegistry::singleton();
        assert!(registry.language("sql").is_some(), "sql grammar missing");
    }

    /// Every language an app editor asks for must have a registered grammar,
    /// or that editor silently renders without highlighting (the Mongo script
    /// and pipeline editors use "javascript", JSON views use "json").
    #[test]
    fn every_editor_language_has_a_grammar() {
        super::register_sql_language();

        let registry = gpui_component::highlighter::LanguageRegistry::singleton();
        for language in ["sql", "javascript", "json"] {
            assert!(
                registry.language(language).is_some(),
                "{language} grammar missing: enable gpui-component/tree-sitter-{language}"
            );
        }
    }

    #[gpui::test]
    fn component_tokens_follow_tablex_overrides(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            let theme = gpui_component::theme::Theme::global(cx);
            // Dialog paints `tokens.background`; it must not keep the stock
            // near-black palette after TableX overrides `colors`.
            assert_eq!(
                gpui::Hsla::from(theme.tokens.background),
                gpui::Hsla::from(super::background())
            );
            assert_eq!(
                gpui::Hsla::from(theme.tokens.popover),
                gpui::Hsla::from(super::surface())
            );
        });
    }

    #[gpui::test]
    fn component_tokens_match_a_full_rederivation_from_colors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            let theme = gpui_component::theme::Theme::global(cx);
            assert_eq!(
                theme.tokens,
                gpui_component::theme::ThemeTokens::from(&theme.colors)
            );
        });
    }

    #[gpui::test]
    fn json_syntax_colors_come_from_tablex_tokens(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            let highlight = gpui_component::theme::Theme::global(cx).highlight_theme.clone();
            let color = |name: &str| {
                use gpui_component::input::HighlightStyleResolver;
                highlight.style(name).and_then(|style| style.color)
            };
            assert_eq!(color("property"), Some(super::danger().into()));
            assert_eq!(color("string"), Some(super::success().into()));
            assert_ne!(color("property"), color("string"));
        });
    }

    #[gpui::test]
    fn component_colors_match_tablex_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            super::init(cx);
            let colors = &gpui_component::theme::Theme::global(cx).colors;
            assert_eq!(colors.background, super::background().into());
            assert_eq!(colors.popover, super::surface().into());
            assert_eq!(colors.border, super::border().into());
            assert_eq!(colors.foreground, super::text().into());
            assert_eq!(colors.primary, super::accent().into());
        });
    }
}
