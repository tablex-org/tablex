// The question asked when no Secret Service runs (Linux): it opens, and its words say what the
// user needs to decide. The answers move a key file in the real data directory, so they are
// exercised by `engine_connect::key_home`'s tests, not here.

#[gpui::test]
fn the_key_home_question_opens_one_dialog(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = palette_app(cx);
    assert!(!palette_is_open(visual));
    app.update_in(visual, |this, window, cx| this.open_key_home_dialog(window, cx));
    visual.run_until_parked();
    assert!(palette_is_open(visual), "the question is a dialog");
    app.read_with(visual, |this, _| assert!(this.tunnel_ui.opened_dialogs.contains(&"key-home")));
}

#[test]
fn the_question_names_the_less_secure_file_the_no_save_answer_and_how_to_get_a_keyring() {
    let en = |key: &str| rust_i18n::t!(key, locale = "en").into_owned();
    let file = en("security.dialog.file");
    assert!(file.contains("less secure") && file.contains("decrypt"), "{file}");
    let keyring = en("security.dialog.keyring");
    for needle in ["gnome-keyring", "KWallet", "KeePassXC", "Settings > Security"] {
        assert!(keyring.contains(needle), "{needle:?} in {keyring:?}");
    }
    assert!(en("security.dialog.none").contains("asks for the password"));
    for key in ["security.dialog.intro", "security.page.no_keyring"] {
        assert!(en(key).contains("tablex") && !en(key).contains("TableX"), "{key}");
    }
}

#[gpui::test]
fn secrets_without_a_reachable_key_open_the_dialog_that_offers_no_file_and_no_permanent_choice(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = palette_app(cx);
    app.update_in(visual, |this, window, cx| this.open_key_unreachable_dialog(window, cx));
    visual.run_until_parked();
    assert!(palette_is_open(visual));
    app.read_with(visual, |this, _| {
        assert!(this.tunnel_ui.opened_dialogs.contains(&"key-unreachable"));
        assert!(!this.tunnel_ui.opened_dialogs.contains(&"key-home"), "not the question that offers a file");
    });
}

#[test]
fn the_unreachable_key_dialog_says_where_the_key_is_and_that_the_session_choice_is_forgotten() {
    let en = |key: &str| rust_i18n::t!(key, locale = "en").into_owned();
    let body = en("security.dialog.unreachable");
    assert!(body.contains("keyring") && body.contains("restart tablex") && body.contains("Nothing was changed"), "{body}");
    assert!(!body.contains("Reset"), "{body}");
    assert!(en("security.dialog.unreachable_session").contains("not remembered"));
    assert!(en("security.dialog.session_no_save").contains("this session"));
}

#[test]
fn the_security_page_menu_texts_exist_in_both_languages() {
    for key in [
        "security.dialog.unreachable_title",
        "security.dialog.unreachable",
        "security.dialog.unreachable_session",
        "security.dialog.session_no_save",
        "security.changed.session",
    ] {
        for locale in ["en", "vi"] {
            let text = rust_i18n::t!(key, locale = locale).into_owned();
            assert!(!text.is_empty() && text != key && !text.contains("security."), "{key} {locale}: {text}");
        }
    }
}
