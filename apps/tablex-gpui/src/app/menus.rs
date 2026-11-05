//! Application menu bar and launch activation.
//!
//! macOS shows the menu bar of whichever app is frontmost, so an app that
//! never installs menus nor activates itself leaves the previous app's menus
//! on screen. `set_menus` stores the menus on Linux and Windows too (no native
//! bar there), where the same commands stay reachable from the command palette
//! (see `command_palette::commands::StaticAction`).
//!
//! Menu items dispatch GPUI actions. Each action below either maps to an
//! existing palette command (`StaticAction::run`, so behavior is never
//! re-implemented here) or to an application-level platform call.

use super::actions::{
    CloseActiveTab, OpenCommandPalette, OpenSettings, ToggleAssistant, ToggleErrorLog,
};
use super::command_palette::StaticAction;
use super::AppRoot;
use gpui::{Action, App, Menu, MenuItem, OsAction};
use gpui_component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};

gpui::actions!(
    tablex_menu,
    [
        About,
        Quit,
        Hide,
        HideOtherApps,
        ShowAll,
        Minimize,
        Zoom,
        NewConnection,
        NewQuery,
        RunSqlFile,
        BackupDatabase,
        RestoreDatabase,
        PendingTransactions,
        DriverManager,
        ToggleSidebar,
        SplitRight,
        SplitDown,
    ]
);

/// Display name of the application in the menu bar and About dialog.
pub(super) const APP_NAME: &str = "TableX";

/// Installs the key bindings, action handlers and menu bar, then asks the
/// platform to activate the app. Called once, after the first window opened:
/// `activate` is a parameter so the launch sequence can be asserted without a
/// real platform.
pub(super) fn finish_launch(cx: &mut App, activate: impl FnOnce(&mut App)) {
    super::shortcuts::bind_defaults(cx);
    register_actions(cx);
    cx.set_menus(build());
    activate(cx);
}

/// The menus show each action's shortcut from the keymap, which the platform
/// reads when the menus are set: call after the key bindings changed.
pub(super) fn refresh(cx: &mut App) {
    if cx.get_menus().is_some() {
        cx.set_menus(build());
    }
}

/// Runs `run` on the window's `AppRoot` once the current dispatch finished
/// (the window is leased while its action is dispatched).
fn on_app_root<A: Action>(
    cx: &mut App,
    run: fn(&mut AppRoot, &mut gpui::Window, &mut gpui::Context<AppRoot>),
) {
    cx.on_action(move |_: &A, cx: &mut App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| root.read(cx).view().clone().downcast::<AppRoot>().ok());
                if let Some(app) = app {
                    app.update(cx, |app, cx| run(app, window, cx));
                }
            });
        });
    });
}

fn on_active_window<A: Action>(cx: &mut App, run: fn(&mut gpui::Window)) {
    cx.on_action(move |_: &A, cx: &mut App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| run(window));
        });
    });
}

fn register_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| {
        let Some(window) = cx.active_window().or_else(|| cx.windows().into_iter().next()) else {
            return cx.quit();
        };
        cx.defer(move |cx| {
            // A window with structure changes that were not applied asks first.
            let asked = window
                .update(cx, |root, window, cx| {
                    let app = root
                        .downcast::<gpui_component::Root>()
                        .ok()
                        .and_then(|root| root.read(cx).view().clone().downcast::<AppRoot>().ok());
                    app.is_some_and(|app| app.update(cx, |app, cx| app.request_quit(window, cx)))
                })
                .unwrap_or(false);
            if !asked {
                cx.quit();
            }
        });
    });
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOtherApps, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    on_active_window::<Minimize>(cx, |window| window.minimize_window());
    on_active_window::<Zoom>(cx, |window| window.zoom_window());
    on_app_root::<About>(cx, |app, window, cx| {
        StaticAction::About.run(app, window, cx)
    });
    on_app_root::<NewConnection>(cx, |app, window, cx| {
        StaticAction::NewConnection.run(app, window, cx)
    });
    on_app_root::<NewQuery>(cx, |app, window, cx| {
        StaticAction::NewQuery.run(app, window, cx)
    });
    on_app_root::<RunSqlFile>(cx, |app, window, cx| {
        StaticAction::RunSqlFile.run(app, window, cx)
    });
    on_app_root::<BackupDatabase>(cx, |app, window, cx| {
        StaticAction::BackupDatabase.run(app, window, cx)
    });
    on_app_root::<RestoreDatabase>(cx, |app, window, cx| {
        StaticAction::RestoreDatabase.run(app, window, cx)
    });
    on_app_root::<PendingTransactions>(cx, |app, window, cx| {
        StaticAction::PendingTransactions.run(app, window, cx)
    });
    on_app_root::<DriverManager>(cx, |app, window, cx| {
        StaticAction::DriverManager.run(app, window, cx)
    });
    on_app_root::<ToggleSidebar>(cx, |app, window, cx| {
        StaticAction::ToggleSidebar.run(app, window, cx)
    });
    on_app_root::<SplitRight>(cx, |app, window, cx| {
        StaticAction::SplitEditorRight.run(app, window, cx)
    });
    on_app_root::<SplitDown>(cx, |app, window, cx| {
        StaticAction::SplitEditorDown.run(app, window, cx)
    });
}

/// The menu bar. The first menu is the macOS application menu.
pub(super) fn build() -> Vec<Menu> {
    vec![
        Menu::new(APP_NAME).items([
            MenuItem::action(format!("About {APP_NAME}"), About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Driver Manager…", DriverManager),
            MenuItem::separator(),
            MenuItem::action(format!("Hide {APP_NAME}"), Hide),
            MenuItem::action("Hide Others", HideOtherApps),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {APP_NAME}"), Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Connection", NewConnection),
            MenuItem::action("New Query", NewQuery),
            MenuItem::action("Run SQL File…", RunSqlFile),
            MenuItem::action("Backup…", BackupDatabase),
            MenuItem::action("Restore…", RestoreDatabase),
            MenuItem::action("Pending Transactions…", PendingTransactions),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseActiveTab),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Error Log", ToggleErrorLog),
            MenuItem::action("AI Assistant", ToggleAssistant),
            MenuItem::separator(),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
            MenuItem::separator(),
            MenuItem::action("Command Palette", OpenCommandPalette),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
        Menu::new("Help").items([MenuItem::action("Command Palette", OpenCommandPalette)]),
    ]
}

impl AppRoot {
    pub(super) fn open_about(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        use gpui::ParentElement as _;
        use gpui_component::WindowExt as _;
        window.open_alert_dialog(cx, |dialog, _, _| {
            dialog
                .title(format!("About {APP_NAME}"))
                .child(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{OwnedMenuItem, TestAppContext};
    use std::cell::Cell;
    use std::rc::Rc;

    fn names(menu: &Menu) -> Vec<String> {
        menu.items
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "-".to_string(),
                MenuItem::Action { name, .. } => name.to_string(),
                MenuItem::Submenu(menu) => menu.name.to_string(),
                MenuItem::SystemMenu(menu) => menu.name.to_string(),
            })
            .collect()
    }

    #[test]
    fn menu_bar_has_the_expected_menus_and_items() {
        let menus = build();
        let titles: Vec<_> = menus.iter().map(|menu| menu.name.to_string()).collect();
        assert_eq!(titles, ["TableX", "File", "Edit", "View", "Window", "Help"]);
        assert_eq!(
            names(&menus[0]),
            [
                "About TableX",
                "-",
                "Settings…",
                "Driver Manager…",
                "-",
                "Hide TableX",
                "Hide Others",
                "Show All",
                "-",
                "Quit TableX"
            ]
        );
        assert_eq!(
            names(&menus[1]),
            ["New Connection", "New Query", "Run SQL File…", "Backup…", "Restore…", "Pending Transactions…", "-", "Close Tab"]
        );
        assert_eq!(
            names(&menus[2]),
            ["Undo", "Redo", "-", "Cut", "Copy", "Paste", "Select All"]
        );
        assert_eq!(
            names(&menus[3]),
            [
                "Toggle Sidebar",
                "Error Log",
                "AI Assistant",
                "-",
                "Split Right",
                "Split Down",
                "-",
                "Command Palette"
            ]
        );
        assert_eq!(names(&menus[4]), ["Minimize", "Zoom"]);
        assert_eq!(names(&menus[5]), ["Command Palette"]);
    }

    #[test]
    fn menu_items_dispatch_the_existing_actions() {
        let menus = build();
        let action_of = |menu: usize, label: &str| -> &'static str {
            menus[menu]
                .items
                .iter()
                .find_map(|item| match item {
                    MenuItem::Action { name, action, .. } if name == label => Some(action.name()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("menu item {label}"))
        };
        assert_eq!(action_of(1, "Backup…"), "tablex_menu::BackupDatabase");
        assert_eq!(action_of(1, "Restore…"), "tablex_menu::RestoreDatabase");
        assert_eq!(action_of(1, "Pending Transactions…"), "tablex_menu::PendingTransactions");
        assert_eq!(action_of(0, "Settings…"), "tablex::OpenSettings");
        assert_eq!(
            action_of(0, "Driver Manager…"),
            "tablex_menu::DriverManager"
        );
        assert_eq!(action_of(0, "Quit TableX"), "tablex_menu::Quit");
        assert_eq!(action_of(1, "Close Tab"), "tablex::CloseActiveTab");
        assert_eq!(action_of(3, "Error Log"), "tablex::ToggleErrorLog");
        assert_eq!(action_of(3, "AI Assistant"), "tablex::ToggleAssistant");
        assert_eq!(
            action_of(3, "Command Palette"),
            "tablex::OpenCommandPalette"
        );
    }

    #[test]
    fn edit_menu_maps_to_os_actions_so_inputs_and_editors_receive_them() {
        let menus = build();
        let os_actions: Vec<_> = menus[2]
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { os_action, .. } => Some(*os_action),
                _ => None,
            })
            .collect();
        let expected = [
            OsAction::Undo,
            OsAction::Redo,
            OsAction::Cut,
            OsAction::Copy,
            OsAction::Paste,
            OsAction::SelectAll,
        ];
        assert_eq!(os_actions.len(), expected.len());
        assert!(os_actions
            .iter()
            .zip(expected)
            .all(|(actual, expected)| *actual == Some(expected)));
    }

    #[gpui::test]
    fn launch_installs_the_menus_and_requests_activation_once(cx: &mut TestAppContext) {
        let activations = Rc::new(Cell::new(0));
        let seen = activations.clone();
        cx.update(|cx| {
            assert!(cx.get_menus().is_none_or(|menus| menus.is_empty()));
            finish_launch(cx, move |_| seen.set(seen.get() + 1));
        });
        assert_eq!(activations.get(), 1);
        let titles = cx.update(|cx| {
            cx.get_menus()
                .expect("platform keeps the menus")
                .iter()
                .map(|menu| menu.name.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(titles, ["TableX", "File", "Edit", "View", "Window", "Help"]);
        let quit_items = cx.update(|cx| {
            cx.get_menus().unwrap()[0]
                .items
                .iter()
                .filter(|item| matches!(item, OwnedMenuItem::Action { name, .. } if name == "Quit TableX"))
                .count()
        });
        assert_eq!(quit_items, 1);
        // Quit is bound to the primary modifier (Cmd on macOS, Ctrl elsewhere).
        let bound = cx.update(|cx| {
            cx.all_bindings_for_input(&[crate::app::shortcuts::parse_chord("secondary-q").unwrap()])
        });
        assert!(
            bound
                .iter()
                .any(|binding| binding.action().name() == "tablex_menu::Quit"),
            "secondary-q must quit"
        );
    }
}
