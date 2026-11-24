//! The interface language. The texts live in `locales/*.yml` (rust-i18n format 2: one entry per key with an
//! `en` and a `vi` value; `en` is the fallback) and are read with `t!("area.key")`. The locale is one of
//! two values kept in an atomic, so a lookup is a hash lookup and no allocation for a plain text.
//!
//! The language is the Appearance > Language setting: `System` follows the OS preferred language
//! (`vi*` is Vietnamese, anything else English). [`apply`] sets the locale of the app and of gpui-component
//! (whose own strings the app extends with its Vietnamese from `locales/gpui_component.yml`).
//!
//! Never translated: data, SQL, database and driver messages, shortcuts, connection names.

use crate::settings::LanguageChoice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    En,
    Vi,
}

impl Locale {
    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Vi => "vi",
        }
    }

    /// The locale of an OS language tag (`vi-VN`, `fr-FR`): Vietnamese for `vi*`, English for the rest.
    pub fn from_tag(tag: &str) -> Self {
        let primary = tag.split(['-', '_']).next().unwrap_or_default();
        if primary.eq_ignore_ascii_case("vi") {
            Self::Vi
        } else {
            Self::En
        }
    }
}

/// The locale a setting asks for; `system` is the OS preferred language tag, when there is one.
pub fn resolve(choice: LanguageChoice, system: Option<&str>) -> Locale {
    match choice {
        LanguageChoice::En => Locale::En,
        LanguageChoice::Vi => Locale::Vi,
        LanguageChoice::System => system.map_or(Locale::En, Locale::from_tag),
    }
}

/// The OS preferred language (macOS: the first of the Language & Region list).
#[cfg(not(test))]
pub fn system_language() -> Option<String> {
    sys_locale::get_locale()
}

// Tests never read the machine's language: English unless a test injects one (per test thread, like the
// current locale below, so tests that run side by side do not see each other's).
#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::Cell<Locale> = const { std::cell::Cell::new(Locale::En) };
    static SYSTEM: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(Some("en-US".into()));
}

#[cfg(test)]
pub fn system_language() -> Option<String> {
    SYSTEM.with(|system| system.borrow().clone())
}

/// Test hook: the OS language `System` resolves against on this thread.
#[cfg(test)]
pub fn set_system_language(tag: Option<&str>) {
    SYSTEM.with(|system| *system.borrow_mut() = tag.map(str::to_owned));
}

#[cfg(not(test))]
static CURRENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The code `t!` looks texts up in.
pub fn code() -> &'static str {
    current().code()
}

#[cfg(not(test))]
pub fn current() -> Locale {
    match CURRENT.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Locale::Vi,
        _ => Locale::En,
    }
}

#[cfg(test)]
pub fn current() -> Locale {
    CURRENT.with(std::cell::Cell::get)
}

/// The text of a key built at run time (a setting's id, a section): the key itself when no text has it
/// (the test of the locale files lists such keys, so that does not happen). `t!` is for literal keys.
pub fn lookup(key: &str) -> std::borrow::Cow<'static, str> {
    crate::_rust_i18n_try_translate(code(), key)
        .unwrap_or_else(|| std::borrow::Cow::Owned(key.to_owned()))
}

/// [`lookup`] with a text of its own for a key that has none (English labels that are their own source).
pub fn lookup_or(key: &str, fallback: &str) -> std::borrow::Cow<'static, str> {
    crate::_rust_i18n_try_translate(code(), key)
        .unwrap_or_else(|| std::borrow::Cow::Owned(fallback.to_owned()))
}

/// Makes `locale` the language of the app and of gpui-component. Returns whether it changed.
pub fn set(locale: Locale) -> bool {
    extend_gpui_component();
    let changed = current() != locale;
    #[cfg(not(test))]
    CURRENT.store(
        u8::from(locale == Locale::Vi),
        std::sync::atomic::Ordering::Relaxed,
    );
    #[cfg(test)]
    CURRENT.with(|current| current.set(locale));
    // gpui-component keeps its locale in a process global: tests (one per thread) only touch it when
    // they change the language, so side-by-side tests that never do cannot disturb the one that does.
    if changed || cfg!(not(test)) {
        gpui_component::set_locale(locale.code());
    }
    changed
}

/// Applies the setting: the app's locale and gpui-component's. Returns whether the language changed.
pub fn apply(choice: LanguageChoice) -> bool {
    set(resolve(choice, system_language().as_deref()))
}

/// Gives gpui-component the Vietnamese texts of `locales/gpui_component.yml` for its own strings (once).
fn extend_gpui_component() {
    static EXTEND: std::sync::Once = std::sync::Once::new();
    EXTEND.call_once(|| rust_i18n::extend!(gpui_component));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    /// `key -> text` of one locale of the app's own files.
    fn messages(locale: &str) -> BTreeMap<String, String> {
        crate::_rust_i18n_backend()
            .messages_for_locale(locale)
            .unwrap_or_default()
            .into_iter()
            .map(|(key, text)| (key.into_owned(), text.into_owned()))
            .collect()
    }

    /// The keys the app owns: gpui-component's own keys (`gpui_component.*`) only carry a Vietnamese text.
    fn own(messages: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
        messages
            .iter()
            .filter(|(key, _)| !key.starts_with("gpui_component."))
            .map(|(key, text)| (key.as_str(), text.as_str()))
            .collect()
    }

    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && path.file_name().is_some_and(|name| name != "i18n.rs")
                {
                    out.push((
                        path.display().to_string(),
                        std::fs::read_to_string(&path).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    fn placeholders(text: &str) -> BTreeSet<&str> {
        text.split("%{")
            .skip(1)
            .filter_map(|rest| rest.split('}').next())
            .collect()
    }

    #[test]
    fn a_language_tag_maps_to_vietnamese_or_english() {
        for (tag, locale) in [
            ("vi", Locale::Vi),
            ("vi-VN", Locale::Vi),
            ("vi_VN", Locale::Vi),
            ("VI-vn", Locale::Vi),
            ("en-US", Locale::En),
            ("fr-FR", Locale::En),
            ("zh-Hant-TW", Locale::En),
            ("", Locale::En),
        ] {
            assert_eq!(Locale::from_tag(tag), locale, "{tag}");
        }
    }

    #[test]
    fn the_setting_decides_and_system_follows_the_os() {
        assert_eq!(resolve(LanguageChoice::En, Some("vi-VN")), Locale::En);
        assert_eq!(resolve(LanguageChoice::Vi, Some("en-US")), Locale::Vi);
        assert_eq!(resolve(LanguageChoice::System, Some("vi-VN")), Locale::Vi);
        assert_eq!(resolve(LanguageChoice::System, Some("fr-FR")), Locale::En);
        assert_eq!(resolve(LanguageChoice::System, None), Locale::En);
    }

    #[test]
    fn every_key_has_an_english_and_a_vietnamese_text() {
        let (en, vi) = (messages("en"), messages("vi"));
        let (en, vi) = (own(&en), own(&vi));
        assert!(!en.is_empty());
        let missing_vi: Vec<_> = en.keys().filter(|key| !vi.contains_key(*key)).collect();
        let missing_en: Vec<_> = vi.keys().filter(|key| !en.contains_key(*key)).collect();
        assert!(missing_vi.is_empty(), "no Vietnamese text: {missing_vi:?}");
        assert!(missing_en.is_empty(), "no English text: {missing_en:?}");
        for (key, english) in &en {
            let vietnamese = vi[key];
            assert!(!english.trim().is_empty() && !vietnamese.trim().is_empty(), "{key}");
            assert_eq!(
                placeholders(english),
                placeholders(vietnamese),
                "{key}: the texts take different values"
            );
        }
    }

    #[test]
    fn the_vietnamese_of_gpui_component_is_for_keys_it_has() {
        let vi = messages("vi");
        let keys: Vec<_> = vi
            .keys()
            .filter_map(|key| key.strip_prefix("gpui_component."))
            .collect();
        assert!(!keys.is_empty());
        for key in keys {
            // Its own English text answers for the key: an unknown key would come back as itself.
            let english = gpui_component_text("en", key);
            assert!(english.is_some(), "gpui-component has no key {key}");
        }
    }

    /// The text gpui-component has for `key`, in its own English file.
    fn gpui_component_text(locale: &str, key: &str) -> Option<String> {
        gpui_component::_rust_i18n_try_translate(locale, key).map(std::borrow::Cow::into_owned)
    }

    #[test]
    fn every_key_the_source_reads_exists() {
        let literal = regex::Regex::new(r#"\bts?!\(\s*"([^"]+)""#).unwrap();
        let unavailable = regex::Regex::new(r#""(settings\.unavailable\.[a-z_]+)""#).unwrap();
        let en = messages("en");
        let mut used = 0;
        let mut unknown = Vec::new();
        for (path, text) in sources() {
            for captures in literal.captures_iter(&text).chain(unavailable.captures_iter(&text)) {
                used += 1;
                let key = &captures[1];
                if !en.contains_key(key) {
                    unknown.push(format!("{key} ({path})"));
                }
            }
        }
        assert!(used > 20, "the scan found only {used} keys");
        assert!(unknown.is_empty(), "keys with no text: {unknown:#?}");
    }

    #[test]
    fn no_key_goes_unused() {
        // Keys read by a name built at run time: the registry (a setting's id, its group, a page) and
        // the ones below, each tested where it is built.
        const BUILT: &[&str] = &["settings.item.", "settings.group.", "settings.nav.", "menu.tab."];
        let sources = sources();
        let unused: Vec<_> = messages("en")
            .keys()
            .filter(|key| !key.starts_with("gpui_component."))
            .filter(|key| !BUILT.iter().any(|prefix| key.starts_with(prefix)))
            .filter(|key| {
                let quoted = format!("\"{key}\"");
                !sources.iter().any(|(_, text)| text.contains(&quoted))
            })
            .cloned()
            .collect();
        assert!(unused.is_empty(), "keys nothing reads: {unused:#?}");
    }

    #[test]
    fn the_tab_menu_keys_are_the_slugs_of_their_english_labels() {
        // `tab_menu_text` finds a text by the slug of the English label the menu entry carries.
        let en = messages("en");
        let keys: Vec<_> = en.iter().filter(|(key, _)| key.starts_with("menu.tab.")).collect();
        assert!(keys.len() > 20);
        for (key, label) in keys {
            assert_eq!(
                key.as_str(),
                format!("menu.tab.{}", crate::settings::registry::group_slug(label)),
                "{label}"
            );
        }
    }

    #[test]
    fn the_settings_texts_are_the_registry_ones() {
        use crate::settings::registry;
        let en = messages("en");
        let text = |key: String| en.get(&key).unwrap_or_else(|| panic!("no key {key}")).clone();
        for descriptor in registry::registry() {
            let id = descriptor.id;
            assert_eq!(text(format!("settings.item.{id}.title")), descriptor.title, "{id}");
            assert_eq!(
                text(format!("settings.item.{id}.description")),
                descriptor.description,
                "{id}"
            );
            assert_eq!(
                text(format!("settings.group.{}", registry::group_slug(descriptor.group))),
                descriptor.group
            );
            if let registry::Kind::Choice { options } = &descriptor.kind {
                for (option, label) in *options {
                    assert_eq!(
                        text(format!("settings.item.{id}.options.{option}")),
                        *label,
                        "{id}.{option}"
                    );
                }
            }
        }
        for section in registry::Section::ALL {
            assert_eq!(text(format!("settings.nav.{}", section.key())), section.title());
        }
        // Every key under the run-time prefixes belongs to a setting, a group or a page.
        let known: BTreeSet<String> = registry::registry()
            .iter()
            .flat_map(|descriptor| {
                let mut keys = vec![
                    format!("settings.item.{}.title", descriptor.id),
                    format!("settings.item.{}.description", descriptor.id),
                    format!("settings.group.{}", registry::group_slug(descriptor.group)),
                ];
                if let registry::Kind::Choice { options } = &descriptor.kind {
                    keys.extend(options.iter().map(|(option, _)| {
                        format!("settings.item.{}.options.{option}", descriptor.id)
                    }));
                }
                keys
            })
            .chain(
                ["appearance", "editor", "results", "query", "navigation", "ai", "shortcuts", "security", "drivers", "about"]
                    .map(|page| format!("settings.nav.{page}")),
            )
            .collect();
        for key in en.keys() {
            if ["settings.item.", "settings.group.", "settings.nav."]
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                assert!(known.contains(key), "{key} belongs to no setting, group or page");
            }
        }
    }
}
