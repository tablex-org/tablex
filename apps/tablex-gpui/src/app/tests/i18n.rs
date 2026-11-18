// Interface language proofs: the title bar's language menu and the Settings page change the language
// of the running app (menus, Settings, grid menus) without a restart, System follows the OS language,
// and the test suite itself runs in English whatever the machine's language is.

use crate::i18n::{self, Locale};
use crate::settings::LanguageChoice;

/// The names of the native menu bar, in order.
fn menu_bar_names(visual: &mut gpui::VisualTestContext) -> Vec<String> {
    visual.update(|_, cx| {
        cx.get_menus()
            .unwrap_or_default()
            .iter()
            .map(|menu| menu.name.to_string())
            .collect()
    })
}

/// What the Theme select of the Settings page shows (a text the page draws), if it exists.
fn theme_select_text(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<String> {
    app.read_with(visual, |this, cx| {
        this.settings_ui.page.as_ref()?.select_text("appearance.theme", cx)
    })
}

/// What the Language select of the Settings page shows.
fn language_select_text(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<String> {
    app.read_with(visual, |this, cx| {
        this.settings_ui.page.as_ref()?.select_text("appearance.language", cx)
    })
}

/// The labels of the grid header's sort menu (a menu the app builds when it opens).
fn sort_menu_labels() -> Vec<String> {
    use gpui_component::menu::PopupMenuItem;
    let sort = super::results::grid::ColumnSort { database: None, page: None, can_database_sort: true };
    super::results::grid::sort_menu_items(0, 0, sort, &gpui::WeakEntity::new_invalid())
        .iter()
        .filter_map(|item| match item {
            PopupMenuItem::Item { label, .. } => Some(label.to_string()),
            _ => None,
        })
        .collect()
}

/// Which entries the title bar's language menu checks.
fn language_menu_checked(current: LanguageChoice) -> Vec<bool> {
    use gpui_component::menu::PopupMenuItem;
    super::shell::language_menu_items(current, &gpui::WeakEntity::new_invalid())
        .iter()
        .map(|item| match item {
            PopupMenuItem::ElementItem { checked, .. } => *checked,
            _ => panic!("language entries are plain items"),
        })
        .collect()
}

fn open_language_menu(visual: &mut gpui::VisualTestContext) {
    assert!(!rendered(visual, "title-language-vi".into()), "closed");
    click(visual, "status-language".into());
    for entry in ["system", "en", "vi"] {
        assert!(
            rendered(visual, format!("title-language-{entry}")),
            "{entry} is listed"
        );
    }
}

#[test]
fn the_test_suite_runs_in_english_whatever_the_machine_speaks() {
    // Every test thread starts in English and never reads the machine's language.
    assert_eq!(i18n::current(), Locale::En);
    assert_eq!(i18n::system_language().as_deref(), Some("en-US"));
    assert_eq!(t!("titlebar.settings"), "Settings");
}

#[test]
fn a_text_with_a_count_has_a_form_for_one_and_for_other() {
    assert_eq!(t!("grid.rows_one"), "1 row");
    assert_eq!(t!("grid.rows_other", count = 1234), "1234 rows");
    i18n::set(Locale::Vi);
    assert_eq!(t!("grid.rows_one"), "1 dòng");
    assert_eq!(t!("grid.rows_other", count = 1234), "1234 dòng");
    i18n::set(Locale::En);
}

#[test]
fn the_limit_and_the_results_window_error_have_both_languages() {
    assert_eq!(t!("grid.limit", count = 500), "Limit 500");
    assert_eq!(t!("grid.could_not_open_results_window", error = "boom"), "Could not open the results window: boom");
    i18n::set(Locale::Vi);
    assert_eq!(t!("grid.limit", count = 500), "Giới hạn 500");
    assert_eq!(t!("grid.could_not_open_results_window", error = "boom"), "Không mở được cửa sổ kết quả: boom");
    i18n::set(Locale::En);
}

#[test]
fn gpui_component_shows_the_vietnamese_the_app_gives_it() {
    i18n::set(Locale::Vi);
    // The component's own lookup, asked for Vietnamese: the app's text answers (its own file has none).
    assert_eq!(
        gpui_component::_rust_i18n_translate("vi", "Dialog.cancel"),
        "Huỷ"
    );
    assert_eq!(gpui_component::locale().to_string(), "vi");
    i18n::set(Locale::En);
    assert_eq!(gpui_component::_rust_i18n_translate("en", "Dialog.cancel"), "Cancel");
    assert_eq!(gpui_component::locale().to_string(), "en");
}

#[gpui::test]
fn the_title_bar_language_menu_switches_the_language_without_a_restart(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    visual.update(|_, cx| cx.set_menus(super::menus::build()));
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::System);
    assert_eq!(i18n::current(), Locale::En);
    assert_eq!(menu_bar_names(visual)[1], "File");
    assert_eq!(sort_menu_labels()[0], "Sort Database Ascending");
    open_settings_page(&app, visual);
    settings_nav(visual, "appearance");
    assert_eq!(theme_select_text(&app, visual).as_deref(), Some("Dark"));
    assert_eq!(
        language_select_text(&app, visual).as_deref(),
        Some("System (follow the OS)")
    );

    // The quick switch is the status bar's language code, and only there: not in the title bar.
    assert!(visual.debug_bounds("title-language-button").is_none(), "no language button in the title bar");
    let button = visual.debug_bounds("status-language").expect("language segment");
    let bar = visual.debug_bounds("status-bar").expect("status bar");
    assert!(bar.contains(&button.center()), "the segment is in the status bar");

    // Opens with the three entries; Escape dismisses without a change.
    open_language_menu(visual);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!rendered(visual, "title-language-vi".into()), "Escape closes");
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::System);

    // The current choice is the checked entry.
    assert_eq!(language_menu_checked(LanguageChoice::System), [true, false, false]);
    assert_eq!(language_menu_checked(LanguageChoice::En), [false, true, false]);
    assert_eq!(language_menu_checked(LanguageChoice::Vi), [false, false, true]);

    // Tiếng Việt: applied at once, no Apply, no restart.
    open_language_menu(visual);
    click(visual, "title-language-vi".into());
    assert!(!rendered(visual, "title-language-vi".into()), "choosing closes the menu");
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::Vi);
    assert_eq!(i18n::current(), Locale::Vi);
    assert_eq!(gpui_component::locale().to_string(), "vi");
    assert_eq!(menu_bar_names(visual)[1], "Tệp", "the native menus are rebuilt");
    assert_eq!(sort_menu_labels()[0], "Sắp xếp bằng cơ sở dữ liệu tăng dần");
    // The open Settings page was redrawn: its selects hold Vietnamese labels, and its draft followed.
    settle(visual);
    assert_eq!(theme_select_text(&app, visual).as_deref(), Some("Tối"));
    assert_eq!(
        language_select_text(&app, visual).as_deref(),
        Some("Tiếng Việt")
    );
    assert_eq!(draft_of(&app, visual).appearance.language, LanguageChoice::Vi);
    assert_eq!(draft_of(&app, visual), applied(&app, visual), "nothing pending");
    // Written after the debounce.
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "Vietnamese on disk", |_| {
        crate::settings::store::load(&path, &crate::settings::store::Legacy::default())
            .settings
            .appearance
            .language
            == LanguageChoice::Vi
    });

    // Reopening: Tiếng Việt is checked; choosing English restores everything.
    visual.update(|_, _| {});
    assert_eq!(language_menu_checked(applied(&app, visual).appearance.language), [false, false, true]);
    open_language_menu(visual);
    click(visual, "title-language-en".into());
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::En);
    assert_eq!(i18n::current(), Locale::En);
    assert_eq!(gpui_component::locale().to_string(), "en");
    assert_eq!(menu_bar_names(visual)[1], "File");
    assert_eq!(sort_menu_labels()[0], "Sort Database Ascending");
    settle(visual);
    assert_eq!(theme_select_text(&app, visual).as_deref(), Some("Dark"));
    assert_eq!(language_select_text(&app, visual).as_deref(), Some("English"));
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn the_settings_page_language_applies_with_apply_and_system_follows_the_os(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    visual.update(|_, cx| cx.set_menus(super::menus::build()));
    open_settings_page(&app, visual);
    settings_nav(visual, "appearance");

    // Settings > Appearance > Language: System, English, Tiếng Việt; a draft until Apply.
    click(visual, "setting-control-appearance.language".into());
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert_eq!(draft_of(&app, visual).appearance.language, LanguageChoice::Vi);
    assert_eq!(i18n::current(), Locale::En, "a draft changes nothing yet");
    click(visual, "settings-apply".into());
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::Vi);
    assert_eq!(i18n::current(), Locale::Vi);
    assert_eq!(menu_bar_names(visual)[1], "Tệp");
    settle(visual);
    assert_eq!(language_select_text(&app, visual).as_deref(), Some("Tiếng Việt"));

    // System with a Vietnamese OS: Vietnamese (the entry reads "Hệ thống ..." in the new language).
    i18n::set_system_language(Some("vi-VN"));
    open_language_menu(visual);
    click(visual, "title-language-system".into());
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::System);
    assert_eq!(i18n::current(), Locale::Vi, "vi-VN maps to Vietnamese");
    assert_eq!(menu_bar_names(visual)[1], "Tệp");

    // System with a French OS: English.
    i18n::set_system_language(Some("fr-FR"));
    open_language_menu(visual);
    click(visual, "title-language-en".into());
    assert_eq!(i18n::current(), Locale::En);
    open_language_menu(visual);
    click(visual, "title-language-system".into());
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::System);
    assert_eq!(i18n::current(), Locale::En, "fr-FR maps to English");
    assert_eq!(menu_bar_names(visual)[1], "File");
    i18n::set_system_language(Some("en-US"));
}

#[gpui::test]
fn a_saved_language_applies_at_startup(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    assert_eq!(i18n::current(), Locale::En);
    // The settings file of a fresh run holds Vietnamese: it is the language before the first draw.
    let path = settings_temp_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!(
            r#"{{"schema_version": {}, "appearance": {{"language": "vi"}}}}"#,
            crate::settings::SCHEMA_VERSION
        ),
    )
    .unwrap();
    app.update(visual, |this, cx| {
        this.settings_ui.path = Some(path.clone());
        this.init_settings(cx);
    });
    assert_eq!(applied(&app, visual).appearance.language, LanguageChoice::Vi);
    assert_eq!(i18n::current(), Locale::Vi);
    i18n::set(Locale::En);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
