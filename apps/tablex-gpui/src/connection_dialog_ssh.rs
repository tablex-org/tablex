//! The "Tunnel / Proxy" tab of the connection dialog: the SSH tunnel settings.
//!
//! Official controls only (`Switch`, `Input`, `Select`, `Checkbox`, `Button`). The SSH passwords
//! and key passphrases (of the bastion and of each jump host) and the proxy password are masked
//! inputs and never leave the dialog except through the credential store
//! (`ssh_tunnel::persist_secrets`) or the in-memory connect path.
//!
//! A jump-host row is its inputs and one `Select`, nothing more: no per-row entity beyond them.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use engine_net::{Secret, SshConfig, TunnelSpec};
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, App, AppContext, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, NumberInput};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, IndexPath};

use super::{config_row, input, replace_input, TestStatus, LABEL_WIDTH};
use crate::ssh_tunnel::{
    JumpSettings, ProxySettings, ProxyType, SshAuthMethod, SshSecretInput, SshSettings,
};
use crate::theme;

type TestHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One jump-host row: its inputs and the method `Select`.
#[derive(Clone)]
struct JumpRow {
    host: Entity<gpui_component::input::InputState>,
    port: Entity<gpui_component::input::InputState>,
    user: Entity<gpui_component::input::InputState>,
    key_path: Entity<gpui_component::input::InputState>,
    /// The password or the key passphrase, whichever the method uses.
    secret: Entity<gpui_component::input::InputState>,
    auth: Rc<Cell<SshAuthMethod>>,
    auth_select: Entity<SelectState<Vec<&'static str>>>,
    /// Never reused: element ids must not follow the row's position, which changes.
    id: usize,
    /// The row's event handlers: they go with the row when it is removed or replaced.
    _subscriptions: Rc<Vec<gpui::Subscription>>,
}

/// What the line under the `~/.ssh/config` picker says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConfigNote {
    #[default]
    None,
    /// Directives of the chosen host that are not applied.
    Ignored(String),
    /// The host cannot be used (ProxyJump / ProxyCommand ...).
    Refused(String),
    /// `~/.ssh/config` could not be read: the picker is empty and says why.
    LoadFailed(String),
}

/// Reads the `~/.ssh/config` the picker offers (on a background thread: see
/// [`SshTabInputs::load_config`]). Unit tests never read the user's own file.
#[cfg(not(test))]
pub fn read_default_ssh_config() -> Result<SshConfig, engine_net::ConfigError> {
    SshConfig::load_default()
}

#[derive(Clone)]
pub struct SshTabInputs {
    pub enabled: Rc<Cell<bool>>,
    pub auth: Rc<Cell<SshAuthMethod>>,
    pub allow_legacy: Rc<Cell<bool>>,
    pub save_secrets: Rc<Cell<bool>>,
    /// The `~/.ssh/config` alias the fields were last filled from.
    pub config_host: Rc<RefCell<String>>,
    pub config_note: Rc<RefCell<ConfigNote>>,
    pub test_status: Rc<RefCell<TestStatus>>,
    host: Entity<gpui_component::input::InputState>,
    port: Entity<gpui_component::input::InputState>,
    user: Entity<gpui_component::input::InputState>,
    key_path: Entity<gpui_component::input::InputState>,
    password: Entity<gpui_component::input::InputState>,
    passphrase: Entity<gpui_component::input::InputState>,
    remote_host: Entity<gpui_component::input::InputState>,
    remote_port: Entity<gpui_component::input::InputState>,
    jumps: Rc<RefCell<Vec<JumpRow>>>,
    /// Set on the copies that live inside event handlers: they reach the rows through this and
    /// never own them (a row owns its handlers, which own such a copy).
    row_source: Option<std::rc::Weak<RefCell<Vec<JumpRow>>>>,
    next_row: Rc<Cell<usize>>,
    pub proxy_enabled: Rc<Cell<bool>>,
    pub proxy_kind: Rc<Cell<ProxyType>>,
    pub proxy_save: Rc<Cell<bool>>,
    proxy_host: Entity<gpui_component::input::InputState>,
    proxy_port: Entity<gpui_component::input::InputState>,
    proxy_user: Entity<gpui_component::input::InputState>,
    proxy_password: Entity<gpui_component::input::InputState>,
    proxy_kind_select: Entity<SelectState<Vec<&'static str>>>,
    auth_select: Entity<SelectState<Vec<&'static str>>>,
    config_select: Entity<SelectState<Vec<String>>>,
    config: Rc<RefCell<SshConfig>>,
    /// What the connection had when the dialog opened (to tell a changed bastion).
    initial: Rc<SshSettings>,
    /// The bastion differs from the saved one: saving removes the saved secret (inline note).
    bastion_dirty: Rc<Cell<bool>>,
    /// The stored tunnel settings could not be read; saving resets them.
    unreadable: Rc<Cell<bool>>,
    test_handler: Rc<RefCell<Option<TestHandler>>>,
    _subscriptions: Rc<RefCell<Vec<gpui::Subscription>>>,
}

impl SshTabInputs {
    pub fn new(window: &mut Window, cx: &mut App, settings: &SshSettings) -> Self {
        let auth_labels: Vec<&'static str> =
            SshAuthMethod::ALL.iter().map(|method| method.label()).collect();
        let auth_row = SshAuthMethod::ALL
            .iter()
            .position(|method| *method == settings.auth)
            .unwrap_or(0);
        let auth_select = cx.new(|cx| {
            SelectState::new(auth_labels, Some(IndexPath::default().row(auth_row)), window, cx)
        });
        let proxy_labels: Vec<&'static str> =
            ProxyType::ALL.iter().map(|kind| kind.label()).collect();
        let proxy_row = ProxyType::ALL
            .iter()
            .position(|kind| *kind == settings.proxy.kind)
            .unwrap_or(0);
        let proxy_kind_select = cx.new(|cx| {
            SelectState::new(proxy_labels, Some(IndexPath::default().row(proxy_row)), window, cx)
        });
        // Empty until `load_config` delivers the aliases.
        let config_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx).searchable(true));
        let this = Self {
            enabled: Rc::new(Cell::new(settings.enabled)),
            auth: Rc::new(Cell::new(settings.auth)),
            allow_legacy: Rc::new(Cell::new(settings.allow_legacy_algorithms)),
            save_secrets: Rc::new(Cell::new(settings.save_secrets)),
            config_host: Rc::new(RefCell::new(settings.config_host.clone())),
            config_note: Rc::new(RefCell::new(ConfigNote::None)),
            test_status: Rc::new(RefCell::new(TestStatus::Idle)),
            host: input(window, cx, &settings.host, false),
            port: input(window, cx, &settings.port, false),
            user: input(window, cx, &settings.user, false),
            key_path: input(window, cx, &settings.key_path, false),
            password: input(window, cx, "", true),
            passphrase: input(window, cx, "", true),
            remote_host: input(window, cx, &settings.remote_host, false),
            remote_port: input(window, cx, &settings.remote_port, false),
            jumps: Rc::new(RefCell::new(Vec::new())),
            row_source: None,
            next_row: Rc::new(Cell::new(0)),
            proxy_enabled: Rc::new(Cell::new(settings.proxy.enabled)),
            proxy_kind: Rc::new(Cell::new(settings.proxy.kind)),
            proxy_save: Rc::new(Cell::new(settings.proxy.save_secrets)),
            proxy_host: input(window, cx, &settings.proxy.host, false),
            proxy_port: input(window, cx, &settings.proxy.port, false),
            proxy_user: input(window, cx, &settings.proxy.user, false),
            proxy_password: input(window, cx, "", true),
            proxy_kind_select,
            auth_select,
            config_select,
            config: Rc::new(RefCell::new(SshConfig::default())),
            initial: Rc::new(settings.clone()),
            bastion_dirty: Rc::new(Cell::new(false)),
            unreadable: Rc::new(Cell::new(false)),
            test_handler: Rc::new(RefCell::new(None)),
            _subscriptions: Rc::new(RefCell::new(Vec::new())),
        };
        this.subscribe(window, cx);
        for jump in &settings.jumps {
            this.add_jump(jump, window, cx);
        }
        this
    }

    /// A copy for event closures that live inside this tab's own subscriptions (they must not own
    /// the list they are stored in).
    fn detached(&self) -> Self {
        Self {
            _subscriptions: Rc::new(RefCell::new(Vec::new())),
            jumps: Rc::new(RefCell::new(Vec::new())),
            row_source: self.row_source.clone().or_else(|| Some(Rc::downgrade(&self.jumps))),
            ..self.clone()
        }
    }

    /// The jump rows (a copy of the handles, cheap).
    fn rows(&self) -> Vec<JumpRow> {
        match &self.row_source {
            Some(source) => source.upgrade().map(|rows| rows.borrow().clone()).unwrap_or_default(),
            None => self.jumps.borrow().clone(),
        }
    }

    /// Appends a jump-host row filled from `jump` and wires its widgets.
    fn add_jump(&self, jump: &JumpSettings, window: &mut Window, cx: &mut App) {
        let id = self.next_row.get();
        self.next_row.set(id + 1);
        let auth_row = SshAuthMethod::ALL.iter().position(|m| *m == jump.auth).unwrap_or(0);
        let labels: Vec<&'static str> = SshAuthMethod::ALL.iter().map(|m| m.label()).collect();
        let auth_select = cx.new(|cx| {
            SelectState::new(labels, Some(IndexPath::default().row(auth_row)), window, cx)
        });
        let mut row = JumpRow {
            host: input(window, cx, &jump.host, false),
            port: input(window, cx, &jump.port, false),
            user: input(window, cx, &jump.user, false),
            key_path: input(window, cx, &jump.key_path, false),
            secret: input(window, cx, "", true),
            auth: Rc::new(Cell::new(jump.auth)),
            auth_select,
            id,
            _subscriptions: Rc::new(Vec::new()),
        };
        let auth = row.auth.clone();
        let tab = self.detached();
        let mut subscriptions = vec![cx.subscribe(
            &row.auth_select,
            move |select, event: &SelectEvent<Vec<&'static str>>, cx| {
                if !matches!(event, SelectEvent::Confirm(Some(_))) {
                    return;
                }
                let index = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                auth.set(SshAuthMethod::ALL.get(index).copied().unwrap_or_default());
                tab.refresh_dirty(cx);
                cx.refresh_windows();
            },
        )];
        for input in [&row.host, &row.port, &row.user] {
            let tab = self.detached();
            subscriptions.push(cx.subscribe(
                input,
                move |_, event: &gpui_component::input::InputEvent, cx| {
                    if matches!(event, gpui_component::input::InputEvent::Change) {
                        tab.refresh_dirty(cx);
                    }
                },
            ));
        }
        row._subscriptions = Rc::new(subscriptions);
        self.jumps.borrow_mut().push(row);
    }

    /// Recomputes whether a hop of the route differs from the saved one (the inline note).
    fn refresh_dirty(&self, cx: &mut App) {
        let changed = SshSettings::route_changed(&self.initial, &self.read(cx));
        if self.bastion_dirty.replace(changed) != changed {
            cx.refresh_windows();
        }
    }

    fn subscribe(&self, window: &mut Window, cx: &mut App) {
        let auth = self.auth.clone();
        let auth_subscription = cx.subscribe(
            &self.auth_select,
            move |select, event: &SelectEvent<Vec<&'static str>>, cx| {
                if !matches!(event, SelectEvent::Confirm(Some(_))) {
                    return;
                }
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                auth.set(SshAuthMethod::ALL.get(row).copied().unwrap_or_default());
                cx.refresh_windows();
            },
        );
        // Filling the fields needs the window: finish after the event, like any deferred update.
        let handle = window.window_handle();
        // The closure must not own the subscriptions it lives in.
        let inputs = Self { _subscriptions: Rc::new(RefCell::new(Vec::new())), ..self.clone() };
        let config_subscription = cx.subscribe(
            &self.config_select,
            move |_, event: &SelectEvent<Vec<String>>, cx| {
                let SelectEvent::Confirm(Some(alias)) = event else {
                    return;
                };
                let (alias, inputs) = (alias.clone(), inputs.clone());
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        inputs.apply_config_host(&alias, window, cx)
                    });
                });
            },
        );
        let mut subscriptions = vec![auth_subscription, config_subscription];
        let proxy_kind = self.proxy_kind.clone();
        let (proxy_port, tab) = (self.proxy_port.clone(), self.detached());
        let proxy_handle = window.window_handle();
        subscriptions.push(cx.subscribe(
            &self.proxy_kind_select,
            move |select, event: &SelectEvent<Vec<&'static str>>, cx| {
                if !matches!(event, SelectEvent::Confirm(Some(_))) {
                    return;
                }
                let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                let before = proxy_kind.replace(ProxyType::ALL.get(row).copied().unwrap_or_default());
                let now = proxy_kind.get();
                // An untouched default port follows the protocol.
                let (port, tab) = (proxy_port.clone(), tab.clone());
                if before != now {
                    cx.defer(move |cx| {
                        let _ = proxy_handle.update(cx, |_, window, cx| {
                            let text = port.read(cx).value().trim().to_owned();
                            if text.is_empty() || text == before.default_port() {
                                replace_input(&port, now.default_port(), window, cx);
                            }
                            tab.refresh_dirty(cx);
                        });
                    });
                }
                cx.refresh_windows();
            },
        ));
        for input in [
            &self.host,
            &self.port,
            &self.user,
            &self.proxy_host,
            &self.proxy_port,
            &self.proxy_user,
        ] {
            let tab = self.detached();
            subscriptions.push(cx.subscribe(
                input,
                move |_, event: &gpui_component::input::InputEvent, cx| {
                    if matches!(event, gpui_component::input::InputEvent::Change) {
                        tab.refresh_dirty(cx);
                    }
                },
            ));
        }
        self._subscriptions.borrow_mut().extend(subscriptions);
    }

    /// Reads `~/.ssh/config` with `loader` on a background thread and fills the picker when it is
    /// ready; an error is shown under the picker, not swallowed.
    pub fn load_config(
        &self,
        window: &mut Window,
        cx: &mut App,
        loader: impl FnOnce() -> Result<SshConfig, engine_net::ConfigError> + Send + 'static,
    ) {
        let task = cx.background_executor().spawn(async move { smol::unblock(loader).await });
        let inputs = self.clone();
        window
            .spawn(cx, async move |cx| {
                let result = task.await;
                let _ = cx.update(|window, cx| inputs.set_config_result(result, window, cx));
            })
            .detach();
    }

    /// Installs a loaded `~/.ssh/config` (or shows why there is none).
    pub fn set_config_result(
        &self,
        result: Result<SshConfig, engine_net::ConfigError>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let config = match result {
            Ok(config) => config,
            Err(error) => {
                *self.config_note.borrow_mut() =
                    ConfigNote::LoadFailed(format!("Could not read ~/.ssh/config: {error}"));
                SshConfig::default()
            }
        };
        let aliases = config.host_aliases();
        *self.config.borrow_mut() = config;
        self.config_select
            .update(cx, |select, cx| select.set_items(aliases, window, cx));
        cx.refresh_windows();
    }

    /// Empties the password and passphrase widgets (when the dialog closes). Their internal buffers
    /// are not zeroizable; this at least stops the text outliving the dialog in the widget state.
    pub fn clear_secrets(&self, window: &mut Window, cx: &mut App) {
        replace_input(&self.password, "", window, cx);
        replace_input(&self.passphrase, "", window, cx);
        replace_input(&self.proxy_password, "", window, cx);
        for row in self.rows() {
            replace_input(&row.secret, "", window, cx);
        }
    }

    /// The record's stored tunnel settings were unreadable: the tab shows defaults and says so.
    pub fn note_unreadable_settings(&self) {
        self.unreadable.set(true);
    }

    /// The bastion of a connection with a tunnel is being replaced: a saved secret goes with it.
    #[cfg(test)]
    pub fn bastion_changed(&self, cx: &App) -> bool {
        crate::ssh_tunnel::bastion_changed(&self.initial, &self.read(cx))
    }

    #[cfg(test)]
    /// The jump rows as the tab shows them, top (dialled first) to bottom.
    pub fn jump_count(&self) -> usize {
        self.rows().len()
    }

    #[cfg(test)]
    /// The host input of row `index`, to see whether it is released with its row.
    pub fn jump_host_handle(&self, index: usize) -> Option<gpui::WeakEntity<gpui_component::input::InputState>> {
        self.rows().get(index).map(|row| row.host.downgrade())
    }

    /// The button's action: the dialog wires it to the app.
    pub fn set_test_handler(&self, handler: impl Fn(&mut Window, &mut App) + 'static) {
        *self.test_handler.borrow_mut() = Some(Rc::new(handler));
    }

    /// The settings as the tab shows them.
    pub fn read(&self, cx: &App) -> SshSettings {
        let text = |input: &Entity<gpui_component::input::InputState>| {
            input.read(cx).value().trim().to_owned()
        };
        SshSettings {
            enabled: self.enabled.get(),
            host: text(&self.host),
            port: text(&self.port),
            user: text(&self.user),
            auth: self.auth.get(),
            key_path: text(&self.key_path),
            config_host: self.config_host.borrow().clone(),
            remote_host: text(&self.remote_host),
            remote_port: text(&self.remote_port),
            allow_legacy_algorithms: self.allow_legacy.get(),
            save_secrets: self.save_secrets.get(),
            jumps: self
                .rows()
                .iter()
                .map(|row| JumpSettings {
                    host: text(&row.host),
                    port: text(&row.port),
                    user: text(&row.user),
                    auth: row.auth.get(),
                    key_path: text(&row.key_path),
                })
                .collect(),
            proxy: ProxySettings {
                enabled: self.proxy_enabled.get(),
                kind: self.proxy_kind.get(),
                host: text(&self.proxy_host),
                port: text(&self.proxy_port),
                user: text(&self.proxy_user),
                save_secrets: self.proxy_save.get(),
            },
        }
    }

    /// What was typed into the password / passphrase fields.
    pub fn read_secrets(&self, cx: &App) -> SshSecretInput {
        SshSecretInput::new(
            Secret::new(self.password.read(cx).value().to_string()),
            Secret::new(self.passphrase.read(cx).value().to_string()),
        )
        .with_jumps(
            self.rows()
                .iter()
                .map(|row| Secret::new(row.secret.read(cx).value().to_string()))
                .collect(),
        )
        .with_proxy(Secret::new(self.proxy_password.read(cx).value().to_string()))
    }

    /// Fills the fields from a `~/.ssh/config` host: the host itself, and its `ProxyJump` chain as
    /// jump rows. A host the tunnel cannot use (ProxyCommand ...) fills nothing and says why, in
    /// engine-net's words.
    pub fn apply_config_host(&self, alias: &str, window: &mut Window, cx: &mut App) {
        let chain = match self.config.borrow().resolve_chain(alias) {
            Ok(chain) => chain,
            Err(error) => {
                *self.config_note.borrow_mut() = ConfigNote::Refused(error.to_string());
                cx.refresh_windows();
                return;
            }
        };
        if let Err(error) = TunnelSpec::from_ssh_config(&chain, None, PathBuf::from("known_hosts"))
        {
            *self.config_note.borrow_mut() = ConfigNote::Refused(error.to_string());
            cx.refresh_windows();
            return;
        }
        let resolved = &chain.target;
        replace_input(&self.host, &resolved.host_name, window, cx);
        replace_input(&self.port, &resolved.port.to_string(), window, cx);
        if let Some(user) = &resolved.user {
            replace_input(&self.user, user, window, cx);
        }
        if let Some(identity) = resolved.identity_files.first() {
            replace_input(&self.key_path, &identity.display().to_string(), window, cx);
            self.set_auth(SshAuthMethod::KeyFile, window, cx);
        }
        // The chain replaces whatever rows were there; a host without one clears them.
        let rows = chain
            .jumps
            .iter()
            .map(|jump| {
                let identity = jump.identity_files.first();
                JumpSettings {
                    host: jump.host_name.clone(),
                    port: jump.port.to_string(),
                    user: jump.user.clone().unwrap_or_default(),
                    auth: if identity.is_some() {
                        SshAuthMethod::KeyFile
                    } else {
                        SshAuthMethod::Agent
                    },
                    key_path: identity.map(|path| path.display().to_string()).unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        self.set_jumps(&rows, &[], window, cx);
        *self.config_host.borrow_mut() = alias.to_owned();
        *self.config_note.borrow_mut() = {
            let mut keywords: Vec<&str> = Vec::new();
            for directive in chain
                .jumps
                .iter()
                .chain([resolved])
                .flat_map(|host| &host.unsupported)
            {
                if !keywords.contains(&directive.keyword.as_str()) {
                    keywords.push(&directive.keyword);
                }
            }
            if keywords.is_empty() {
                ConfigNote::None
            } else {
                ConfigNote::Ignored(keywords.join(", "))
            }
        };
        cx.refresh_windows();
    }

    /// Replaces all jump rows (typed secrets go to the rows by position).
    pub fn set_jumps(
        &self,
        jumps: &[JumpSettings],
        secrets: &[Secret],
        window: &mut Window,
        cx: &mut App,
    ) {
        self.jumps.borrow_mut().clear();
        for (index, jump) in jumps.iter().take(engine_net::MAX_JUMPS).enumerate() {
            self.add_jump(jump, window, cx);
            if let (Some(secret), Some(row)) = (secrets.get(index), self.jumps.borrow().last()) {
                replace_input(&row.secret, secret.expose(), window, cx);
            }
        }
        self.refresh_dirty(cx);
        cx.refresh_windows();
    }

    pub fn set_auth(&self, method: SshAuthMethod, window: &mut Window, cx: &mut App) {
        self.auth.set(method);
        let row = SshAuthMethod::ALL.iter().position(|m| *m == method).unwrap_or(0);
        self.auth_select.update(cx, |select, cx| {
            select.set_selected_index(Some(IndexPath::default().row(row)), window, cx)
        });
        cx.refresh_windows();
    }

    /// Presses "Test tunnel" the way a click would.
    #[cfg(feature = "ui-scenarios")]
    pub fn trigger_test(&self, window: &mut Window, cx: &mut App) {
        let handler = self.test_handler.borrow().clone();
        if let Some(handler) = handler {
            handler(window, cx);
        }
    }

    /// Test and scenario seeds: puts values into the tab like a user would.
    #[cfg(feature = "ui-scenarios")]
    pub fn fill(&self, settings: &SshSettings, secrets: &SshSecretInput, window: &mut Window, cx: &mut App) {
        self.enabled.set(settings.enabled);
        replace_input(&self.host, &settings.host, window, cx);
        replace_input(&self.port, &settings.port, window, cx);
        replace_input(&self.user, &settings.user, window, cx);
        replace_input(&self.key_path, &settings.key_path, window, cx);
        replace_input(&self.remote_host, &settings.remote_host, window, cx);
        replace_input(&self.remote_port, &settings.remote_port, window, cx);
        replace_input(&self.password, secrets.password.expose(), window, cx);
        replace_input(&self.passphrase, secrets.passphrase.expose(), window, cx);
        self.allow_legacy.set(settings.allow_legacy_algorithms);
        self.save_secrets.set(settings.save_secrets);
        self.set_auth(settings.auth, window, cx);
        self.set_jumps(&settings.jumps, &secrets.jumps, window, cx);
        self.proxy_enabled.set(settings.proxy.enabled);
        self.proxy_save.set(settings.proxy.save_secrets);
        replace_input(&self.proxy_host, &settings.proxy.host, window, cx);
        replace_input(&self.proxy_port, &settings.proxy.port, window, cx);
        replace_input(&self.proxy_user, &settings.proxy.user, window, cx);
        replace_input(&self.proxy_password, secrets.proxy.expose(), window, cx);
        self.set_proxy_kind(settings.proxy.kind, window, cx);
    }

    #[cfg(feature = "ui-scenarios")]
    pub fn set_proxy_kind(&self, kind: ProxyType, window: &mut Window, cx: &mut App) {
        self.proxy_kind.set(kind);
        let row = ProxyType::ALL.iter().position(|k| *k == kind).unwrap_or(0);
        self.proxy_kind_select.update(cx, |select, cx| {
            select.set_selected_index(Some(IndexPath::default().row(row)), window, cx)
        });
        cx.refresh_windows();
    }

    pub fn render(&self, secrets_stored: bool) -> AnyElement {
        let enabled = self.enabled.get();
        let method = self.auth.get();
        let off = !enabled;
        let route_off = !(enabled || self.proxy_enabled.get());
        let mut body = div().flex().flex_col().gap(px(14.0));
        body = body.child(note(
            "connection-config-tunnel-notice",
            "The SSH server (bastion) opens the connection to the database for you: the database host and port are reached from the SSH server, optionally through jump hosts and a proxy. Each database session of the connection gets its own tunnel.",
        ));
        if self.unreadable.get() {
            body = body.child(small_note(
                "connection-config-ssh-unreadable",
                "The stored tunnel settings were unreadable and have been reset; review them. Saving writes the settings shown here.".to_owned(),
                true,
                false,
            ));
        }
        body = body.child(config_row(
            "SSH Tunnel",
            "connection-config-ssh-enable",
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div().debug_selector(|| "ssh-enable-switch".into()).child(
                        Switch::new("ssh-enable").checked(enabled).on_click({
                            let enabled = self.enabled.clone();
                            move |checked, _, cx| {
                                enabled.set(*checked);
                                cx.refresh_windows();
                            }
                        }),
                    ),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme::text_muted())
                        .child("Connect through an SSH tunnel"),
                ),
        ));
        body = body.child(config_row(
            "SSH Config",
            "connection-config-ssh-config",
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div().debug_selector(|| "connection-config-ssh-config-select".into()).child(
                        Select::new(&self.config_select)
                            .placeholder("Use a ~/.ssh/config host")
                            .search_placeholder("Search hosts")
                            .disabled(off),
                    ),
                )
                .children(match self.config_note.borrow().clone() {
                    ConfigNote::None => None,
                    ConfigNote::Ignored(keywords) => Some(small_note(
                        "connection-config-ssh-config-ignored",
                        format!("Not applied from ~/.ssh/config: {keywords}"),
                        false,
                        false,
                    )),
                    ConfigNote::Refused(reason) => Some(small_note(
                        "connection-config-ssh-config-refused",
                        reason,
                        true,
                        false,
                    )),
                    ConfigNote::LoadFailed(reason) => Some(small_note(
                        "connection-config-ssh-config-failed",
                        reason,
                        true,
                        false,
                    )),
                }),
        ));
        body = body.child(config_row(
            "SSH Host",
            "connection-config-ssh-host",
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(Input::new(&self.host).appearance(true).disabled(off)),
                )
                .child(
                    div()
                        .debug_selector(|| "connection-config-ssh-port".into())
                        .w(px(110.0))
                        .flex_shrink_0()
                        .child(NumberInput::new(&self.port).disabled(off)),
                ),
        ));
        body = body.child(config_row(
            "SSH User",
            "connection-config-ssh-user",
            Input::new(&self.user).appearance(true).disabled(off),
        ));
        if self.bastion_dirty.get() {
            body = body.child(small_note(
                "connection-config-ssh-secret-reset",
                "Changing the SSH host, port or user removes the saved password or passphrase when you save: a secret is never sent to a server it was not entered for. Enter it again below.".to_owned(),
                false,
                true,
            ));
        }
        body = body.child(config_row(
            "Auth Method",
            "connection-config-ssh-auth",
            div()
                .debug_selector(|| "connection-config-ssh-auth-select".into())
                .child(Select::new(&self.auth_select).disabled(off)),
        ));
        body = match method {
            SshAuthMethod::Password => body.child(config_row(
                "Password",
                "connection-config-ssh-password",
                Input::new(&self.password)
                    .mask_toggle()
                    .appearance(true)
                    .disabled(off),
            )),
            SshAuthMethod::KeyFile => body
                .child(config_row(
                    "Private Key",
                    "connection-config-ssh-key",
                    self.key_file_control(off),
                ))
                .child(config_row(
                    "Passphrase",
                    "connection-config-ssh-passphrase",
                    Input::new(&self.passphrase)
                        .mask_toggle()
                        .appearance(true)
                        .disabled(off),
                )),
            SshAuthMethod::Agent => body.child(note(
                "connection-config-ssh-agent-note",
                "Signs in with the keys of your running ssh-agent (SSH_AUTH_SOCK on macOS and Linux, the OpenSSH agent or Pageant on Windows).",
            )),
            SshAuthMethod::KeyboardInteractive => body.child(note(
                "connection-config-ssh-ki-note",
                "The server's questions (a one-time code, a second factor) are shown in a dialog when you connect. All database sessions of this connection share one tunnel, so you answer once.",
            )),
        };
        let jump_methods: Vec<SshAuthMethod> =
            self.jumps.borrow().iter().map(|row| row.auth.get()).collect();
        let saves = |wanted: SshAuthMethod| method == wanted || jump_methods.contains(&wanted);
        if saves(SshAuthMethod::Password) || saves(SshAuthMethod::KeyFile) {
            body = body.child(self.save_secrets_row(
                saves(SshAuthMethod::Password),
                saves(SshAuthMethod::KeyFile),
                !jump_methods.is_empty(),
                secrets_stored,
                off,
            ));
        }
        body = body.child(self.jumps_section(off));
        // Under the hops it opens (the whole route, proxy included), above the optional settings.
        body = body.child(self.test_row(route_off));
        body = body
            .child(config_row(
                "Remote Host",
                "connection-config-ssh-remote",
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div().flex_1().min_w(px(0.0)).child(
                            Input::new(&self.remote_host).appearance(true).disabled(off),
                        ),
                    )
                    .child(
                        div()
                            .w(px(110.0))
                            .flex_shrink_0()
                            .child(NumberInput::new(&self.remote_port).disabled(off)),
                    ),
            ))
            .child(small_note(
                "connection-config-ssh-remote-hint",
                "Optional. Leave empty to reach the database host and port of the connection from the SSH server.".to_owned(),
                false,
                true,
            ))
            .child(config_row(
                "Algorithms",
                "connection-config-ssh-legacy",
                Checkbox::new("ssh-legacy")
                    .label("Allow legacy algorithms (diffie-hellman-group14-sha1, ssh-rsa)")
                    .checked(self.allow_legacy.get())
                    .disabled(off)
                    .on_click({
                        let allow = self.allow_legacy.clone();
                        move |checked, _, cx| {
                            allow.set(*checked);
                            cx.refresh_windows();
                        }
                    }),
            ));
        body = body.child(self.proxy_section());
        body.into_any_element()
    }

    fn key_file_control(&self, off: bool) -> AnyElement {
        let key_path = self.key_path.clone();
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(Input::new(&self.key_path).appearance(true).disabled(off)),
            )
            .child(
                Button::new("ssh-browse-key")
                    .debug_selector(|| "ssh-browse-key".into())
                    .icon(Icon::empty().path("icons/folder-open.svg").size(px(14.0)))
                    .outline()
                    .w(px(32.0))
                    .h(px(32.0))
                    .disabled(off)
                    .tooltip("Choose a private key file")
                    .on_click(move |_, window, cx| {
                        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: false,
                            prompt: None,
                        });
                        let key_path = key_path.clone();
                        window
                            .spawn(cx, async move |cx| {
                                if let Ok(Ok(Some(paths))) = picked.await {
                                    if let Some(path) = paths.first() {
                                        let text = path.display().to_string();
                                        let _ = cx.update(|window, cx| {
                                            replace_input(&key_path, &text, window, cx)
                                        });
                                    }
                                }
                            })
                            .detach();
                    }),
            )
            .into_any_element()
    }

    fn save_secrets_row(
        &self,
        passwords: bool,
        passphrases: bool,
        several: bool,
        stored: bool,
        off: bool,
    ) -> AnyElement {
        let what = match (passwords, passphrases, several) {
            (true, false, false) => "password",
            (false, true, false) => "passphrase",
            (true, false, true) => "passwords",
            (false, true, true) => "passphrases",
            _ => "passwords and passphrases",
        };
        let save = Checkbox::new("ssh-save-secrets")
            .label(format!("Save {what}"))
            .checked(self.save_secrets.get())
            .disabled(off)
            .on_click({
                let save = self.save_secrets.clone();
                move |checked, _, cx| {
                    save.set(*checked);
                    cx.refresh_windows();
                }
            });
        div()
            .debug_selector(|| "connection-config-ssh-save".into())
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(LABEL_WIDTH + 12.0))
            .child(save)
            .child(
                div()
                    .id("connection-ssh-save-help")
                    .flex()
                    .items_center()
                    .child(
                        Icon::empty()
                            .path("icons/circle-help.svg")
                            .size(px(14.0))
                            .text_color(theme::text_muted()),
                    )
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(format!(
                            "The {what} is stored in the {}. Unsaved, you are asked for it when you connect and it is kept in memory only while the connection is open.{}",
                            crate::platform::CREDENTIAL_STORE_LABEL,
                            if stored { " A saved one is kept when this field is left empty." } else { "" }
                        ))
                        .build(window, cx)
                    }),
            )
            .into_any_element()
    }

    fn jumps_section(&self, off: bool) -> AnyElement {
        let rows = self.jumps.borrow().clone();
        let count = rows.len();
        let tab = self.clone();
        let add = Button::new("ssh-jump-add")
            .debug_selector(|| "ssh-jump-add".into())
            .icon(Icon::empty().path("icons/plus.svg").size(px(14.0)))
            .label("Add jump host")
            .compact()
            .outline()
            .disabled(off || count >= engine_net::MAX_JUMPS)
            .on_click(move |_, window, cx| {
                tab.add_jump(&JumpSettings::default(), window, cx);
                cx.refresh_windows();
            });
        let mut section = div().flex().flex_col().gap(px(8.0)).child(config_row(
            "Jump hosts",
            "connection-config-ssh-jumps",
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(add)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(theme::text_muted())
                        .child(if count == 0 {
                            "Optional. SSH servers to pass through before the SSH host, in the order shown."
                        } else {
                            "Dialled in this order, then the SSH host above."
                        }),
                ),
        ));
        for (index, row) in rows.iter().enumerate() {
            section = section.child(self.jump_row(index, count, row, off));
        }
        section.into_any_element()
    }

    fn jump_row(&self, index: usize, count: usize, row: &JumpRow, off: bool) -> AnyElement {
        let select = |name: &str| format!("ssh-jump-{index}-{name}");
        let caption = |text: &'static str| {
            div()
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .child(text)
        };
        let icon_button = |name: &'static str, icon: &'static str, tip: &'static str, disabled: bool| {
            Button::new(SharedString::from(format!("ssh-jump-{}-{name}", row.id)))
                .debug_selector({
                    let selector = select(name);
                    move || selector.clone()
                })
                .icon(Icon::empty().path(icon).size(px(14.0)))
                .outline()
                .w(px(28.0))
                .h(px(28.0))
                .tooltip(tip)
                .disabled(off || disabled)
        };
        let jumps = self.jumps.clone();
        let tab = self.clone();
        let swap = move |from: usize, to: usize| {
            let (jumps, tab) = (jumps.clone(), tab.clone());
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                jumps.borrow_mut().swap(from, to);
                tab.refresh_dirty(cx);
                cx.refresh_windows();
            }
        };
        let remove = {
            let (jumps, tab) = (self.jumps.clone(), self.clone());
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                if index < jumps.borrow().len() {
                    jumps.borrow_mut().remove(index);
                }
                tab.refresh_dirty(cx);
                cx.refresh_windows();
            }
        };
        let method = row.auth.get();
        let mut second = div().flex().items_center().gap(px(8.0)).child(
            div()
                .debug_selector({
                    let selector = select("auth");
                    move || selector.clone()
                })
                .w(px(190.0))
                .flex_shrink_0()
                .child(Select::new(&row.auth_select).disabled(off)),
        );
        let secret_input = |label: &'static str, selector: String| {
            div()
                .debug_selector(move || selector.clone())
                .flex_1()
                .min_w(px(0.0))
                .child(
                    Input::new(&row.secret)
                        .mask_toggle()
                        .appearance(true)
                        .prefix(caption(label))
                        .disabled(off),
                )
        };
        second = match method {
            SshAuthMethod::Password => second.child(secret_input("Password", select("secret"))),
            SshAuthMethod::KeyFile => second.child(
                div()
                    .debug_selector({
                        let selector = select("key");
                        move || selector.clone()
                    })
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        Input::new(&row.key_path)
                            .appearance(true)
                            .prefix(caption("Key"))
                            .disabled(off),
                    ),
            ),
            SshAuthMethod::Agent => second.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child("Keys of your running ssh-agent"),
            ),
            SshAuthMethod::KeyboardInteractive => second.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child("Questions are asked in a dialog"),
            ),
        };
        let field = |input: &Entity<gpui_component::input::InputState>, label: &'static str, name: &str, width: Option<f32>| {
            let selector = select(name);
            let wrapper = div().debug_selector(move || selector.clone());
            match width {
                Some(width) => wrapper.w(px(width)).flex_shrink_0(),
                None => wrapper.flex_1().min_w(px(0.0)),
            }
            .child(
                Input::new(input)
                    .appearance(true)
                    .prefix(caption(label))
                    .disabled(off),
            )
        };
        div()
            .debug_selector({
                let selector = select("row");
                move || selector.clone()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .p(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::surface())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.0))
                                    .text_color(theme::text_muted())
                                    .child(format!("Jump host {} of {count}", index + 1)),
                            )
                            .child(
                                icon_button("up", "icons/arrow-up.svg", "Move up (dialled earlier)", index == 0)
                                    .on_click(swap(index.saturating_sub(1), index)),
                            )
                            .child(
                                icon_button("down", "icons/arrow-down.svg", "Move down (dialled later)", index + 1 >= count)
                                    .on_click(swap(index, (index + 1).min(count.saturating_sub(1)))),
                            )
                            .child(
                                icon_button("remove", "icons/trash.svg", "Remove this jump host", false)
                                    .on_click(remove),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            // The host takes what the narrow port and user fields leave.
                            .child(field(&row.host, "Host", "host", None))
                            .child(field(&row.port, "Port", "port", Some(88.0)))
                            .child(field(&row.user, "User", "user", Some(128.0))),
                    )
                    .child(second)
                    .children(
                        (method == SshAuthMethod::KeyFile)
                            .then(|| secret_input("Passphrase", select("secret"))),
                    ),
            )
            .into_any_element()
    }

    fn proxy_section(&self) -> AnyElement {
        let enabled = self.proxy_enabled.get();
        let off = !enabled;
        let mut section = div().flex().flex_col().gap(px(14.0));
        section = section.child(config_row(
            "Proxy",
            "connection-config-proxy",
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div().debug_selector(|| "proxy-enable-switch".into()).child(
                        Switch::new("proxy-enable").checked(enabled).on_click({
                            let enabled = self.proxy_enabled.clone();
                            let tab = self.clone();
                            move |checked, _, cx| {
                                enabled.set(*checked);
                                tab.refresh_dirty(cx);
                                cx.refresh_windows();
                            }
                        }),
                    ),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme::text_muted())
                        .child("Connect through a SOCKS5 or HTTP proxy"),
                ),
        ));
        if !enabled {
            return section.into_any_element();
        }
        let kind = self.proxy_kind.get();
        section = section
            .child(config_row(
                "Proxy Type",
                "connection-config-proxy-kind",
                div()
                    .debug_selector(|| "connection-config-proxy-kind-select".into())
                    .child(Select::new(&self.proxy_kind_select).disabled(off)),
            ))
            .child(config_row(
                "Proxy Host",
                "connection-config-proxy-host",
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(Input::new(&self.proxy_host).appearance(true).disabled(off)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "connection-config-proxy-port".into())
                            // Room for "1080" between the stepper buttons.
                            .w(px(140.0))
                            .flex_shrink_0()
                            .child(NumberInput::new(&self.proxy_port).disabled(off)),
                    ),
            ))
            .child(config_row(
                "Proxy User",
                "connection-config-proxy-user",
                Input::new(&self.proxy_user).appearance(true).disabled(off),
            ))
            .child(config_row(
                "Proxy Password",
                "connection-config-proxy-password",
                Input::new(&self.proxy_password)
                    .mask_toggle()
                    .appearance(true)
                    .disabled(off),
            ))
            .child(
                div()
                    .debug_selector(|| "connection-config-proxy-save".into())
                    .pl(px(LABEL_WIDTH + 12.0))
                    .child(
                        Checkbox::new("proxy-save-secrets")
                            .label("Save password")
                            .checked(self.proxy_save.get())
                            .disabled(off)
                            .on_click({
                                let save = self.proxy_save.clone();
                                move |checked, _, cx| {
                                    save.set(*checked);
                                    cx.refresh_windows();
                                }
                            }),
                    ),
            );
        if kind == ProxyType::Http {
            section = section.child(small_note(
                "connection-config-proxy-http-note",
                "HTTP proxy Basic credentials are sent to the proxy unencrypted. Use them only on a network you trust, or use a SOCKS5 proxy.".to_owned(),
                false,
                true,
            ));
        }
        section = section.child(small_note(
            "connection-config-proxy-note",
            "With an SSH tunnel the proxy carries the connection to the first SSH server only. Without SSH it carries the connection to the database. Leave the user empty for a proxy without a login.".to_owned(),
            false,
            true,
        ));
        section.into_any_element()
    }

    fn test_row(&self, off: bool) -> AnyElement {
        let status = self.test_status.borrow().clone();
        let running = status == TestStatus::Running;
        let handler = self.test_handler.borrow().clone();
        let (text, color): (SharedString, gpui::Rgba) = match &status {
            TestStatus::Idle => (SharedString::default(), theme::text_muted()),
            TestStatus::Running => ("Testing the tunnel...".into(), theme::text_muted()),
            TestStatus::Success { millis } => (
                format!("Tunnel works ({millis} ms)").into(),
                theme::success(),
            ),
            TestStatus::Failure(message) => (message.clone().into(), theme::danger()),
        };
        let full = text.clone();
        config_row(
            "Test",
            "connection-config-ssh-test",
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    Button::new("ssh-test-tunnel")
                        .debug_selector(|| "ssh-test-tunnel".into())
                        .label(if running { "Testing..." } else { "Test tunnel" })
                        .compact()
                        .outline()
                        .disabled(off || running || handler.is_none())
                        .on_click(move |_, window, cx| {
                            if let Some(handler) = &handler {
                                handler(window, cx);
                            }
                            window.refresh();
                        }),
                )
                .child(
                    div()
                        .id("connection-ssh-test-status")
                        .debug_selector(|| "connection-ssh-test-status".into())
                        .flex_1()
                        .min_w(px(0.0))
                        // One line, so a long error never moves the button; the tooltip has it all.
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(color)
                        .when(!full.is_empty(), |this| {
                            this.tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(full.clone())
                                    .build(window, cx)
                            })
                        })
                        .child(text),
                ),
        )
    }
}

fn note(selector: &'static str, text: &'static str) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

fn small_note(selector: &'static str, text: String, danger: bool, indent: bool) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .when(indent, |this| this.pl(px(LABEL_WIDTH + 12.0)))
        .text_size(px(11.0))
        .text_color(if danger { theme::danger() } else { theme::text_muted() })
        .child(text)
        .into_any_element()
}
