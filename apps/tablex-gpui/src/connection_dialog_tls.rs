//! The "TLS/SSL" tab of the connection dialog (PostgreSQL, MySQL / MariaDB, MongoDB): the mode
//! `Select`, the CA / client certificate / client key files and the MySQL server name.
//!
//! Official controls only (`Select`, `Input`, `Button`). Each driver family has its own mode list
//! (`connection_tls::TlsFamily`), so there is one `Select` per family and the tab shows the one of
//! the current kind. What a driver does not read stays visible but disabled, with the reason.
//! There is no passphrase field: no driver supports an encrypted key, so there is no secret here
//! and nothing for the credential store.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    div, px, AnyElement, App, AppContext, Entity, InteractiveElement, IntoElement, ParentElement,
    Styled, Window,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{Disableable, Icon, IndexPath};

use super::{config_row, replace_input};
use crate::connection_tls::{TlsFamily, TlsMode, TlsSettings};
use crate::connections::ConnectionKind;
use crate::theme;

type ModeSelect = Entity<SelectState<Vec<&'static str>>>;

const FAMILIES: [TlsFamily; 3] = [TlsFamily::Postgres, TlsFamily::MySql, TlsFamily::Mongo];

/// What the event handlers need. Kept apart from the subscriptions that hold the handlers, so no
/// handler owns the list it is stored in.
struct Parts {
    kind: Rc<Cell<ConnectionKind>>,
    mode: Cell<TlsMode>,
    ca: Entity<InputState>,
    cert: Entity<InputState>,
    key: Entity<InputState>,
    server_name: Entity<InputState>,
    /// Why the settings cannot be used, found without the disk (files are checked on Test / Save).
    problem: RefCell<Option<String>>,
}

impl Parts {
    fn settings(&self, cx: &App) -> TlsSettings {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        TlsSettings {
            mode: self.mode.get(),
            ca_file: text(&self.ca),
            client_cert: text(&self.cert),
            client_key: text(&self.key),
            server_name: text(&self.server_name),
        }
    }

    fn recheck(&self, cx: &App) {
        *self.problem.borrow_mut() = TlsFamily::of(self.kind.get())
            .and_then(|family| self.settings(cx).check(family).err())
            .map(|error| error.to_string());
    }
}

#[derive(Clone)]
pub struct TlsInputs {
    parts: Rc<Parts>,
    selects: Rc<Vec<(TlsFamily, ModeSelect)>>,
    _subscriptions: Rc<Vec<gpui::Subscription>>,
}

impl TlsInputs {
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        kind: Rc<Cell<ConnectionKind>>,
        settings: &TlsSettings,
    ) -> Self {
        let field = |window: &mut Window, cx: &mut App, value: &str, placeholder: &'static str| {
            let value = value.to_string();
            cx.new(move |cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let parts = Rc::new(Parts {
            kind,
            mode: Cell::new(settings.mode),
            ca: field(window, cx, &settings.ca_file, "PEM file, e.g. /path/to/ca.pem"),
            cert: field(window, cx, &settings.client_cert, "Optional PEM file"),
            key: field(window, cx, &settings.client_key, "Optional unencrypted PEM file"),
            server_name: field(window, cx, &settings.server_name, "Optional, the name in the server certificate"),
            problem: RefCell::new(None),
        });
        let selects: Vec<(TlsFamily, ModeSelect)> = FAMILIES
            .iter()
            .map(|family| {
                let labels: Vec<&'static str> =
                    family.modes().iter().map(|mode| family.label(*mode)).collect();
                let row = mode_row(*family, settings.mode);
                let select = cx.new(|cx| {
                    SelectState::new(labels, Some(IndexPath::default().row(row)), window, cx)
                });
                (*family, select)
            })
            .collect();
        let mut subscriptions = Vec::new();
        for input in [&parts.ca, &parts.cert, &parts.key, &parts.server_name] {
            let parts = parts.clone();
            subscriptions.push(cx.subscribe(input, move |_, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    parts.recheck(cx);
                    cx.refresh_windows();
                }
            }));
        }
        for (family, select) in &selects {
            let (parts, family) = (parts.clone(), *family);
            subscriptions.push(cx.subscribe(
                select,
                move |select, event: &SelectEvent<Vec<&'static str>>, cx| {
                    if !matches!(event, SelectEvent::Confirm(Some(_))) {
                        return;
                    }
                    let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                    if let Some(mode) = family.modes().get(row) {
                        parts.mode.set(*mode);
                        parts.recheck(cx);
                        cx.refresh_windows();
                    }
                },
            ));
        }
        let this = Self {
            parts,
            selects: Rc::new(selects),
            _subscriptions: Rc::new(subscriptions),
        };
        this.parts.recheck(cx);
        this
    }

    pub fn read(&self, cx: &App) -> TlsSettings {
        self.parts.settings(cx)
    }

    /// Shows `settings` (parsing a URL into the fields, or opening a saved connection).
    pub fn fill(&self, settings: &TlsSettings, window: &mut Window, cx: &mut App) {
        for (input, value) in [
            (&self.parts.ca, &settings.ca_file),
            (&self.parts.cert, &settings.client_cert),
            (&self.parts.key, &settings.client_key),
            (&self.parts.server_name, &settings.server_name),
        ] {
            replace_input(input, value, window, cx);
        }
        self.parts.mode.set(settings.mode);
        self.sync_kind(window, cx);
    }

    /// The kind changed: a mode the new driver does not offer falls back to the default, and the
    /// new kind's `Select` shows the current mode.
    pub fn sync_kind(&self, window: &mut Window, cx: &mut App) {
        if let Some(family) = TlsFamily::of(self.parts.kind.get()) {
            if !family.modes().contains(&self.parts.mode.get()) {
                self.parts.mode.set(TlsMode::Default);
            }
            // A row the driver does not read says so instead of inviting input.
            for (input, used, text) in [
                (&self.parts.key, family.has_key_file(), "Optional unencrypted PEM file"),
                (&self.parts.server_name, family.has_server_name(), "Optional, the name in the server certificate"),
            ] {
                let text = if used { text } else { "Not used" };
                input.update(cx, |state, cx| state.set_placeholder(text, window, cx));
            }
            let row = mode_row(family, self.parts.mode.get());
            if let Some((_, select)) = self.selects.iter().find(|(own, _)| *own == family) {
                select.update(cx, |select, cx| {
                    select.set_selected_index(Some(IndexPath::default().row(row)), window, cx)
                });
            }
        }
        self.parts.recheck(cx);
        cx.refresh_windows();
    }

    /// The mode the current kind's `Select` shows, and the inline problem (tests).
    #[cfg(test)]
    pub fn shown_for_test(&self, cx: &App) -> (Option<String>, Option<String>) {
        let label = TlsFamily::of(self.parts.kind.get())
            .and_then(|family| self.selects.iter().find(|(own, _)| *own == family))
            .and_then(|(_, select)| select.read(cx).selected_value().map(|label| label.to_string()));
        (label, self.parts.problem.borrow().clone())
    }

    pub fn render(&self, use_uri: bool) -> AnyElement {
        let Some(family) = TlsFamily::of(self.parts.kind.get()) else {
            return div().into_any_element();
        };
        if use_uri {
            return div()
                .debug_selector(|| "connection-config-tls-url-note".into())
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(
                    "This URL is used as entered: put the TLS options in the URL itself \
                     (tls=true, tlsCAFile=..., tlsCertificateKeyFile=...).",
                )
                .into_any_element();
        }
        let select = self
            .selects
            .iter()
            .find(|(own, _)| *own == family)
            .map(|(_, select)| select.clone());
        let mode = self.parts.mode.get();
        let mut body = div().flex().flex_col().gap(px(14.0));
        if let Some(select) = select {
            body = body.child(config_row(
                "SSL Mode",
                "connection-config-ssl-mode",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .debug_selector(|| "connection-config-ssl-mode-select".into())
                            .child(Select::new(&select)),
                    )
                    .child(hint(None, family.describe(mode))),
            ));
        }
        body = body.child(config_row(
            "CA Certificate",
            "connection-config-ssl-ca",
            file_control("tls-browse-ca", "Choose the CA certificate", &self.parts.ca, false),
        ));
        body = body.child(config_row(
            "Client Cert",
            "connection-config-ssl-cert",
            if family.has_key_file() {
                file_control("tls-browse-cert", "Choose the client certificate", &self.parts.cert, false)
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(file_control(
                        "tls-browse-cert",
                        "Choose the client certificate and key file",
                        &self.parts.cert,
                        false,
                    ))
                    .child(hint(None, "One PEM file holding the certificate and its private key."))
                    .into_any_element()
            },
        ));
        body = body.child(config_row(
            "Client Key",
            "connection-config-ssl-key",
            if family.has_key_file() {
                file_control("tls-browse-key", "Choose the client key", &self.parts.key, false)
            } else {
                unsupported(
                    "connection-config-ssl-key-unsupported",
                    &self.parts.key,
                    "MongoDB reads the key from the client certificate file.",
                )
            },
        ));
        body = body.child(config_row(
            "Server Name",
            "connection-config-ssl-server-name",
            if family.has_server_name() {
                Input::new(&self.parts.server_name).appearance(true).into_any_element()
            } else {
                unsupported(
                    "connection-config-ssl-server-name-unsupported",
                    &self.parts.server_name,
                    match family {
                        TlsFamily::Postgres => {
                            "PostgreSQL checks the certificate against the Host. To reach another address, add hostaddr to URL Params."
                        }
                        _ => "MongoDB checks the certificate against the host of the URL.",
                    },
                )
            },
        ));
        if family.has_key_file() {
            body = body.child(hint(
                Some("connection-config-tls-key-note"),
                if cfg!(unix) {
                    "Private keys must be unencrypted PEM files that group and others cannot read (chmod 600). Key passphrases are not supported."
                } else {
                    "Private keys must be unencrypted PEM files. Key passphrases are not supported."
                },
            ));
        }
        if let Some(problem) = self.parts.problem.borrow().clone() {
            body = body.child(
                div()
                    .debug_selector(|| "connection-config-tls-error".into())
                    .text_size(px(12.0))
                    .text_color(theme::danger())
                    .child(problem),
            );
        }
        body.into_any_element()
    }
}

fn mode_row(family: TlsFamily, mode: TlsMode) -> usize {
    family.modes().iter().position(|offered| *offered == mode).unwrap_or(0)
}

fn hint(selector: Option<&'static str>, text: &'static str) -> AnyElement {
    let line = div().text_size(px(11.0)).text_color(theme::text_muted()).child(text);
    match selector {
        Some(selector) => line.debug_selector(move || selector.into()).into_any_element(),
        None => line.into_any_element(),
    }
}

/// A path input with the system file picker next to it.
fn file_control(
    id: &'static str,
    tooltip: &'static str,
    target: &Entity<InputState>,
    off: bool,
) -> AnyElement {
    let picked_into = target.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(Input::new(target).appearance(true).disabled(off)),
        )
        .child(
            Button::new(id)
                .debug_selector(move || id.into())
                .icon(Icon::empty().path("icons/folder-open.svg").size(px(14.0)))
                .outline()
                .w(px(32.0))
                .h(px(32.0))
                .disabled(off)
                .tooltip(tooltip)
                .on_click(move |_, window, cx| {
                    let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: None,
                    });
                    let target = picked_into.clone();
                    window
                        .spawn(cx, async move |cx| {
                            if let Ok(Ok(Some(paths))) = picked.await {
                                if let Some(path) = paths.first() {
                                    let text = path.display().to_string();
                                    let _ = cx.update(|window, cx| {
                                        replace_input(&target, &text, window, cx);
                                        // `set_value` is silent; the inline check listens for edits.
                                        target.update(cx, |_, cx| cx.emit(InputEvent::Change));
                                    });
                                }
                            }
                        })
                        .detach();
                }),
        )
        .into_any_element()
}

/// A disabled input and the reason this driver does not read it.
fn unsupported(
    selector: &'static str,
    target: &Entity<InputState>,
    reason: &'static str,
) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(Input::new(target).appearance(true).disabled(true))
        .child(hint(None, reason))
        .into_any_element()
}
