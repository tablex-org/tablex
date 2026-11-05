use crate::connection_form::ConnectionFormState;
use crate::connections::ConnectionKind;
use crate::agent_access::{agent_select, AgentChoice};
use crate::settings::connection::{
    AgentLevel, AgentResolution, ConnectionProfile, ConnectionType, OverrideValue, OverrideValueExt as _, Overrides, AGENT_ACCESS,
};
use crate::settings::registry::{self, Kind, SettingDescriptor, Value};
use crate::settings::Settings;
use crate::theme;
#[path = "connection_picker_catalog.rs"]
mod connection_picker_catalog;
pub(crate) use connection_picker_catalog::DriverAvailability;
#[path = "connection_dialog_ssh.rs"]
mod ssh_tab;
#[cfg(test)]
pub use ssh_tab::ConfigNote;
pub use ssh_tab::SshTabInputs;
#[path = "connection_dialog_tls.rs"]
mod tls_tab;
pub use tls_tab::TlsInputs;
use connection_picker_catalog::{PickerCategory, PickerEntry};
use gpui::prelude::FluentBuilder;
use gpui::{
    div, img, px, AnyElement, App, AppContext, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, NumberInput};
use gpui_component::popover::Popover;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::switch::Switch;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{Disableable, Icon, Selectable, StyledExt};
#[cfg(test)]
use gpui_component::WindowExt;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(feature = "ui-scenarios")]
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConnectionDialogStep {
    Picker,
    Config,
}

/// Tabs of the connection form (Connection | TLS/SSL | Tunnel / Proxy | Advanced).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigTab {
    Connection,
    Tls,
    Tunnel,
    Advanced,
    /// Per-connection values of the overridable settings.
    Overrides,
}

impl ConfigTab {
    fn label(self) -> &'static str {
        match self {
            Self::Connection => "Connection",
            Self::Tls => "TLS/SSL",
            Self::Tunnel => "Tunnel / Proxy",
            Self::Advanced => "Advanced",
            Self::Overrides => "Overrides",
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Connection => "connection-config-tab-connection",
            Self::Tls => "connection-config-tab-tls",
            Self::Tunnel => "connection-config-tab-tunnel",
            Self::Advanced => "connection-config-tab-advanced",
            Self::Overrides => "connection-config-tab-overrides",
        }
    }

    /// Tabs shown for `kind`; file databases have no network layers.
    pub fn for_kind(kind: ConnectionKind) -> Vec<Self> {
        if kind == ConnectionKind::Sqlite {
            vec![Self::Connection, Self::Advanced, Self::Overrides]
        } else if matches!(kind, ConnectionKind::Clickhouse | ConnectionKind::Redis) {
            // TLS lives on the Connection tab (Secure toggle, ClickHouse: + CA certificate): neither
            // driver reads a mode or a client certificate.
            vec![Self::Connection, Self::Tunnel, Self::Advanced, Self::Overrides]
        } else {
            vec![Self::Connection, Self::Tls, Self::Tunnel, Self::Advanced, Self::Overrides]
        }
    }
}

/// Inline result of the footer "Test" button.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TestStatus {
    #[default]
    Idle,
    Running,
    Success { millis: u64 },
    Failure(String),
}

/// Color swatches: (value, name); `""` is the "none" swatch.
const COLOR_SWATCHES: [(&str, &str); 7] = [
    ("", "None"),
    ("#22c55e", "Green"),
    ("#eab308", "Yellow"),
    ("#f97316", "Orange"),
    ("#ef4444", "Red"),
    ("#3b82f6", "Blue"),
    ("#a855f7", "Purple"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerView {
    Grid,
    List,
}

#[cfg(feature = "ui-scenarios")]
static NEXT_SCENARIO_DIALOG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct ConnectionDialogInputs {
    pub name: Entity<InputState>,
    pub uri: Entity<InputState>,
    pub password: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub database: Entity<InputState>,
    pub auth_database: Entity<InputState>,
    pub uri_options: Entity<InputState>,
    pub file_path: Entity<InputState>,
    pub connect_timeout: Entity<InputState>,
    /// ClickHouse CA certificate file (`ca_cert=`).
    pub ca_cert: Entity<InputState>,
    /// ClickHouse HTTPS toggle (`secure=true`).
    pub secure: Rc<Cell<bool>>,
    /// The TLS/SSL tab (PostgreSQL, MySQL / MariaDB, MongoDB).
    pub tls: TlsInputs,
    /// The Tunnel / Proxy tab.
    pub ssh: SshTabInputs,
    custom_color: Entity<InputState>,
    /// `None` = untouched (keep the stored / default color).
    pub color: Rc<RefCell<Option<String>>>,
    pub tab: Rc<Cell<ConfigTab>>,
    pub test_status: Rc<RefCell<TestStatus>>,
    /// Inline message under the URL row when applying it fails.
    pub url_error: Rc<RefCell<Option<String>>>,
    pub save_password: Rc<Cell<bool>>,
    pub is_pinned: Rc<Cell<bool>>,
    pub kind: Rc<Cell<ConnectionKind>>,
    pub use_uri: Rc<Cell<bool>>,
    /// The connection exists in storage (enables "Select Visible Databases").
    pub saved: Rc<Cell<bool>>,
    /// Id of the chosen connection type; empty = none (global settings).
    pub type_id: Rc<RefCell<String>>,
    type_select: Entity<SelectState<Vec<gpui::SharedString>>>,
    /// The types as the select lists them (names, presets) for the summary line.
    types: Rc<Vec<ConnectionType>>,
    /// The Overrides tab: one row per overridable setting.
    override_rows: Rc<Vec<OverrideRow>>,
    /// The Overrides tab's "Agent access" row: the connection has its own level (else the type's
    /// default applies), which level, and the `Select` that picks it.
    agent_custom: Rc<Cell<bool>>,
    agent_level: Rc<Cell<AgentLevel>>,
    agent_select: Entity<SelectState<Vec<AgentChoice>>>,
    _agent_subscription: Rc<gpui::Subscription>,
    /// Overrides the record had; ones this tab cannot show are kept on save.
    initial_overrides: Rc<Overrides>,
    global: Arc<Settings>,
    _type_subscription: Rc<gpui::Subscription>,
    pub step: Rc<Cell<ConnectionDialogStep>>,
    pub picker_search: Entity<InputState>,
    picker_category: Rc<Cell<PickerCategory>>,
    picker_view: Rc<Cell<PickerView>>,
    picker_filter: Rc<RefCell<String>>,
    /// Which drivers exist (set by the app when it opens the dialog).
    drivers: Rc<RefCell<DriverAvailability>>,
    _picker_subscription: Rc<gpui::Subscription>,
    _color_subscription: Rc<gpui::Subscription>,
    #[cfg(feature = "ui-scenarios")]
    scenario_id: u64,
    #[cfg(feature = "ui-scenarios")]
    scenario_enabled: bool,
}

impl ConnectionDialogInputs {
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        form: &ConnectionFormState,
        settings: &Arc<Settings>,
    ) -> Self {
        let profile = form.profile.clone().unwrap_or_default();
        let types = Rc::new(settings.connection_types.clone());
        let type_ids: Rc<Vec<String>> = Rc::new(types.iter().map(|ty| ty.id.clone()).collect());
        let type_labels: Vec<gpui::SharedString> = std::iter::once(NO_TYPE_LABEL.into())
            .chain(types.iter().map(|ty| ty.name.clone().into()))
            .collect();
        let selected_type = type_ids
            .iter()
            .position(|id| *id == profile.type_id)
            .map_or(0, |position| position + 1);
        let type_select = cx.new(|cx| {
            SelectState::new(
                type_labels,
                Some(gpui_component::IndexPath::default().row(selected_type)),
                window,
                cx,
            )
        });
        let type_id = Rc::new(RefCell::new(profile.type_id.clone()));
        let type_subscription = {
            let type_id = type_id.clone();
            let type_ids = type_ids.clone();
            cx.subscribe(
                &type_select,
                move |select, event: &SelectEvent<Vec<gpui::SharedString>>, cx| {
                    if !matches!(event, SelectEvent::Confirm(Some(_))) {
                        return;
                    }
                    let row = select.read(cx).selected_index(cx).map_or(0, |index| index.row);
                    *type_id.borrow_mut() = row
                        .checked_sub(1)
                        .and_then(|position| type_ids.get(position).cloned())
                        .unwrap_or_default();
                    cx.refresh_windows();
                },
            )
        };
        let override_rows = Rc::new(
            registry::registry()
                .iter()
                .filter(|descriptor| descriptor.overridable && descriptor.is_visible())
                .map(|descriptor| {
                    let own = profile
                        .overrides
                        .get(descriptor.id)
                        .and_then(|value| value.to_value(descriptor));
                    let shown = own.clone().unwrap_or_else(|| descriptor.get(settings));
                    OverrideRow {
                        descriptor,
                        custom: Rc::new(Cell::new(own.is_some())),
                        toggle: Rc::new(Cell::new(matches!(shown, Value::Bool(true)))),
                        input: match shown {
                            Value::Number(number) => {
                                Some(input(window, cx, &format_number(number), false))
                            }
                            _ => None,
                        },
                    }
                })
                .collect::<Vec<_>>(),
        );
        let agent_own = profile.agent_level();
        let agent_level = Rc::new(Cell::new(agent_own.unwrap_or_default()));
        let agent_select = agent_select(&AgentLevel::ALL, Some(agent_level.get()), window, cx);
        let agent_subscription = {
            let level = agent_level.clone();
            cx.subscribe(
                &agent_select,
                move |_, event: &SelectEvent<Vec<AgentChoice>>, cx| {
                    if let SelectEvent::Confirm(Some(chosen)) = event {
                        level.set(*chosen);
                        cx.refresh_windows();
                    }
                },
            )
        };
        let picker_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search databases"));
        let picker_filter = Rc::new(RefCell::new(String::new()));
        let picker_filter_for_events = picker_filter.clone();
        let picker_search_for_events = picker_search.clone();
        let picker_subscription = cx.subscribe(&picker_search, move |_, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                *picker_filter_for_events.borrow_mut() =
                    picker_search_for_events.read(cx).value().to_string();
                cx.refresh_windows();
            }
        });
        let custom_color = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("#rrggbb")
                .default_value(match &form.color_hex {
                    Some(color) if !color.is_empty() && !is_preset_color(color) => color.clone(),
                    _ => String::new(),
                })
        });
        // A new connection starts with the "none" swatch.
        let color: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(
            form.color_hex
                .clone()
                .or_else(|| form.id.is_none().then(String::new)),
        ));
        let color_for_events = color.clone();
        let custom_for_events = custom_color.clone();
        let color_subscription = cx.subscribe(&custom_color, move |_, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = custom_for_events.read(cx).value().trim().to_string();
                let text = if text.starts_with('#') { text } else { format!("#{text}") };
                if is_hex_color(&text) {
                    *color_for_events.borrow_mut() = Some(text.to_ascii_lowercase());
                    cx.refresh_windows();
                }
            }
        });
        let kind = Rc::new(Cell::new(form.kind));
        let this = Self {
            connect_timeout: input(window, cx, &form.connect_timeout, false),
            ca_cert: input(window, cx, &form.ca_cert, false),
            secure: Rc::new(Cell::new(form.secure)),
            tls: TlsInputs::new(window, cx, kind.clone(), &form.tls),
            ssh: SshTabInputs::new(window, cx, &form.ssh.clone().unwrap_or_default()),
            custom_color,
            color,
            tab: Rc::new(Cell::new(ConfigTab::Connection)),
            test_status: Rc::new(RefCell::new(TestStatus::Idle)),
            url_error: Rc::new(RefCell::new(None)),
            saved: Rc::new(Cell::new(form.id.is_some())),
            type_id,
            type_select,
            types,
            override_rows,
            agent_custom: Rc::new(Cell::new(agent_own.is_some())),
            agent_level,
            agent_select,
            _agent_subscription: Rc::new(agent_subscription),
            initial_overrides: Rc::new(profile.overrides),
            global: settings.clone(),
            _type_subscription: Rc::new(type_subscription),
            _color_subscription: Rc::new(color_subscription),
            name: input(window, cx, &form.name, false),
            uri: input(window, cx, &form.uri, false),
            password: input(window, cx, &form.password, true),
            host: input(window, cx, &form.host, false),
            port: input(window, cx, &form.port, false),
            username: input(window, cx, &form.username, false),
            database: input(window, cx, &form.database, false),
            auth_database: input(window, cx, &form.auth_database, false),
            uri_options: input(window, cx, &form.uri_options, false),
            file_path: input(window, cx, &form.file_path, false),
            save_password: Rc::new(Cell::new(form.save_password)),
            is_pinned: Rc::new(Cell::new(form.is_pinned)),
            kind,
            use_uri: Rc::new(Cell::new(form.use_uri)),
            step: Rc::new(Cell::new(if form.id.is_some() {
                ConnectionDialogStep::Config
            } else {
                ConnectionDialogStep::Picker
            })),
            picker_search,
            picker_category: Rc::new(Cell::new(category_for_kind(form.kind))),
            picker_view: Rc::new(Cell::new(PickerView::Grid)),
            picker_filter,
            drivers: Default::default(),
            _picker_subscription: Rc::new(picker_subscription),
            #[cfg(feature = "ui-scenarios")]
            scenario_id: NEXT_SCENARIO_DIALOG_ID.fetch_add(1, Ordering::Relaxed),
            #[cfg(feature = "ui-scenarios")]
            scenario_enabled: false,
        };
        this.apply_kind_placeholders(form.kind, window, cx);
        // `~/.ssh/config` is read off the UI thread; the picker fills when it is ready.
        // (Unit tests are single-threaded and deterministic: they feed the picker themselves.)
        #[cfg(not(test))]
        this.ssh.load_config(window, cx, ssh_tab::read_default_ssh_config);
        this
    }

    /// Capture seed (`TABLEX_SEED_TUNNEL`): the Tunnel tab filled in for a connection behind a
    /// bastion, optionally running "Test tunnel".
    #[cfg(feature = "ui-scenarios")]
    pub fn seed_tunnel_capture(&self, run_test: bool, chain: bool, window: &mut Window, cx: &mut App) {
        use crate::ssh_tunnel::{
            JumpSettings, ProxySettings, ProxyType, SshAuthMethod, SshSecretInput, SshSettings,
        };
        for (input, value) in [
            (&self.name, "Orders (via bastion)"),
            (&self.host, "db.internal.example.com"),
            (&self.port, "5432"),
            (&self.username, "app"),
            (&self.database, "orders"),
        ] {
            replace_input(input, value, window, cx);
        }
        self.set_step(ConnectionDialogStep::Config, cx);
        self.tab.set(ConfigTab::Tunnel);
        let mut settings = SshSettings {
            enabled: true,
            host: "bastion.example.com".into(),
            user: "deploy".into(),
            auth: SshAuthMethod::KeyFile,
            key_path: "~/.ssh/id_ed25519".into(),
            ..SshSettings::default()
        };
        let mut secrets = SshSecretInput::new(engine_net::Secret::default(), engine_net::Secret::new("passphrase"));
        if chain {
            // Two jump hosts in front of the bastion, and a SOCKS5 proxy in front of the first.
            settings.jumps = vec![
                JumpSettings {
                    host: "gateway.example.com".into(),
                    user: "ops".into(),
                    auth: SshAuthMethod::Agent,
                    ..JumpSettings::default()
                },
                JumpSettings {
                    host: "jump.internal.example.com".into(),
                    port: "2222".into(),
                    user: "hop".into(),
                    auth: SshAuthMethod::Password,
                    ..JumpSettings::default()
                },
            ];
            settings.proxy = ProxySettings {
                enabled: true,
                kind: ProxyType::Socks5,
                host: "proxy.corp.example.com".into(),
                user: "svc".into(),
                ..ProxySettings::default()
            };
            secrets = secrets
                .with_jumps(vec![engine_net::Secret::default(), engine_net::Secret::new("jump-password")])
                .with_proxy(engine_net::Secret::new("proxy-password"));
        }
        self.ssh.fill(&settings, &secrets, window, cx);
        if run_test {
            let ssh = self.ssh.clone();
            window.defer(cx, move |window, cx| ssh.trigger_test(window, cx));
        }
    }

    /// Capture seed (`TABLEX_SEED_TLS=postgres|mysql|mongodb|postgres-error`): the TLS tab of a new
    /// connection with the settings a verified connection would have (`-error`: verify-ca without
    /// its CA file, to show the inline error).
    #[cfg(feature = "ui-scenarios")]
    pub fn seed_tls_capture(&self, seed: &str, window: &mut Window, cx: &mut App) {
        use crate::connection_tls::{TlsMode, TlsSettings};
        let (kind, port, user, tls) = match seed {
            "mysql" => (
                ConnectionKind::Mysql,
                "3306",
                "app",
                TlsSettings {
                    mode: TlsMode::VerifyFull,
                    ca_file: "/etc/ssl/tablex/ca.pem".into(),
                    client_cert: "/etc/ssl/tablex/client-cert.pem".into(),
                    client_key: "/etc/ssl/tablex/client-key.pem".into(),
                    server_name: "db.example.com".into(),
                },
            ),
            "mongodb" => (
                ConnectionKind::Mongodb,
                "27017",
                "app",
                TlsSettings {
                    mode: TlsMode::VerifyFull,
                    ca_file: "/etc/ssl/tablex/ca.pem".into(),
                    client_cert: "/etc/ssl/tablex/client.pem".into(),
                    ..TlsSettings::default()
                },
            ),
            error => (
                ConnectionKind::Postgres,
                "5432",
                "app",
                if error == "postgres-error" {
                    TlsSettings { mode: TlsMode::VerifyCa, ..TlsSettings::default() }
                } else {
                    TlsSettings {
                        mode: TlsMode::VerifyFull,
                        ca_file: "/etc/ssl/tablex/ca.pem".into(),
                        client_cert: "/etc/ssl/tablex/client-cert.pem".into(),
                        client_key: "/etc/ssl/tablex/client-key.pem".into(),
                        ..TlsSettings::default()
                    }
                },
            ),
        };
        self.kind.set(kind);
        self.apply_kind_placeholders(kind, window, cx);
        for (input, value) in [
            (&self.name, "Orders (TLS)"),
            (&self.host, "db.example.com"),
            (&self.port, port),
            (&self.username, user),
            (&self.database, "orders"),
        ] {
            replace_input(input, value, window, cx);
        }
        self.use_uri.set(false);
        self.tls.fill(&tls, window, cx);
        self.set_step(ConnectionDialogStep::Config, cx);
        self.tab.set(ConfigTab::Tls);
    }

    pub fn is_picker(&self) -> bool {
        self.step.get() == ConnectionDialogStep::Picker
    }

    #[cfg(feature = "ui-scenarios")]
    pub fn scenario_id(&self) -> u64 {
        self.scenario_id
    }

    #[cfg(feature = "ui-scenarios")]
    pub fn set_scenario_enabled(&mut self, enabled: bool) {
        self.scenario_enabled = enabled;
    }

    #[cfg(feature = "ui-scenarios")]
    pub fn scenario_state(&self, cx: &App) -> serde_json::Value {
        serde_json::json!({
            "step": if self.is_picker() { "picker" } else { "config" },
            "category": self.picker_category.get().label(),
            "view": format!("{:?}", self.picker_view.get()),
            "kind": kind_label(self.kind.get()),
            "search_length": self.picker_search.read(cx).value().chars().count(),
            "next_enabled": self.picker_selection_is_visible(),
            "use_uri": self.use_uri.get(),
        })
    }

    /// Tells the picker which drivers exist (installed / downloadable).
    pub fn set_driver_availability(&self, drivers: DriverAvailability) {
        *self.drivers.borrow_mut() = drivers;
    }

    pub fn picker_selection_is_visible(&self) -> bool {
        let query = self.picker_filter.borrow().trim().to_ascii_lowercase();
        let kind = self.kind.get();
        connection_picker_catalog::entry_for_kind(kind).is_some_and(|entry| {
            entry.is_available(&self.drivers.borrow())
                && connection_picker_catalog::matches_query(entry, &query)
                && (!query.is_empty() || entry.category == self.picker_category.get())
        })
    }

    pub fn set_step(&self, step: ConnectionDialogStep, cx: &mut App) {
        self.step.set(step);
        cx.refresh_windows();
    }

    pub fn read_form(&self, cx: &App, mut form: ConnectionFormState) -> ConnectionFormState {
        form.name = self.name.read(cx).value().to_string();
        form.uri = self.uri.read(cx).value().to_string();
        form.password = self.password.read(cx).value().to_string();
        form.host = self.host.read(cx).value().to_string();
        form.port = self.port.read(cx).value().to_string();
        form.username = self.username.read(cx).value().to_string();
        form.database = self.database.read(cx).value().to_string();
        form.auth_database = self.auth_database.read(cx).value().to_string();
        form.uri_options = self.uri_options.read(cx).value().to_string();
        form.file_path = self.file_path.read(cx).value().to_string();
        form.connect_timeout = self.connect_timeout.read(cx).value().trim().to_string();
        form.ca_cert = self.ca_cert.read(cx).value().to_string();
        form.secure = self.secure.get();
        form.tls = self.tls.read(cx);
        form.color_hex = self.color.borrow().clone();
        form.save_password = self.save_password.get();
        form.is_pinned = self.is_pinned.get();
        form.kind = self.kind.get();
        form.use_uri = self.use_uri.get();
        form.profile = Some(self.read_profile(cx));
        let mut ssh = self.ssh.read(cx);
        // Files have no network layer; a stale switch must not survive a change of kind.
        let networked = form.kind != ConnectionKind::Sqlite;
        ssh.enabled &= networked;
        ssh.proxy.enabled &= networked;
        form.ssh = Some(ssh);
        form.ssh_secrets = self.ssh.read_secrets(cx);
        form
    }

    /// The type and overrides as the dialog shows them. A custom number that
    /// is not a number is kept as NaN, which `ConnectionFormState::validate`
    /// reports.
    pub fn read_profile(&self, cx: &App) -> ConnectionProfile {
        let mut overrides = (*self.initial_overrides).clone();
        for row in self.override_rows.iter() {
            let id = row.descriptor.id;
            overrides.clear(id);
            if !row.custom.get() {
                continue;
            }
            let value = match (&row.descriptor.kind, &row.input) {
                (Kind::Toggle, _) => OverrideValue::Bool(row.toggle.get()),
                (Kind::Number { .. }, Some(input)) => OverrideValue::Number(
                    input.read(cx).value().trim().parse::<f64>().unwrap_or(f64::NAN),
                ),
                _ => continue,
            };
            overrides.set(id, value);
        }
        overrides.clear(AGENT_ACCESS);
        if self.agent_custom.get() {
            overrides.set(AGENT_ACCESS, OverrideValue::Text(self.agent_level.get().id().to_owned()));
        }
        ConnectionProfile {
            type_id: self.type_id.borrow().clone(),
            overrides,
        }
    }

    #[cfg(test)]
    pub fn render(&self) -> AnyElement {
        self.render_with_height(px(600.0))
    }

    pub fn render_with_height(&self, height: Pixels) -> AnyElement {
        if self.is_picker() {
            return database_picker(
                self.kind.clone(),
                self.picker_search.clone(),
                self.picker_filter.clone(),
                (self.picker_category.clone(), self.picker_view.clone()),
                self.step.clone(),
                self.kind_selected_callback(),
                self.drivers.borrow().clone(),
                height,
            )
            .into_any_element();
        }
        self.render_config(height)
    }

    /// Callback for the picker: refreshes the per-kind placeholders of the
    /// config fields ("Optional, defaults to postgres", URL example, ...).
    fn kind_selected_callback(&self) -> OnKindSelected {
        let inputs = self.clone();
        Rc::new(move |kind, window, cx| inputs.apply_kind_placeholders(kind, window, cx))
    }

    pub fn apply_kind_placeholders(&self, kind: ConnectionKind, window: &mut Window, cx: &mut App) {
        let (uri, database, port, host) = match kind {
            ConnectionKind::Postgres => (
                "postgresql://user:password@host:port/database",
                "Optional, defaults to postgres",
                "5432",
                "localhost",
            ),
            ConnectionKind::Mongodb => (
                "mongodb://user:password@host:port/database",
                "Optional",
                "27017",
                "localhost",
            ),
            ConnectionKind::Clickhouse => (
                "clickhouse://user:password@host:8123/database",
                "Optional, defaults to default",
                "8123",
                "localhost",
            ),
            ConnectionKind::Redis => (
                "redis://user:password@host:6379/0",
                "Database index, defaults to 0",
                "6379",
                "localhost",
            ),
            _ => ("", "Optional", "", "localhost"),
        };
        let set = |input: &Entity<InputState>, text: &'static str, window: &mut Window, cx: &mut App| {
            input.update(cx, |state, cx| state.set_placeholder(text, window, cx));
        };
        set(&self.uri, uri, window, cx);
        set(&self.database, database, window, cx);
        set(&self.port, port, window, cx);
        set(&self.host, host, window, cx);
        set(&self.auth_database, "Optional, often admin", window, cx);
        set(
            &self.uri_options,
            match kind {
                ConnectionKind::Postgres => "application_name=tablex",
                ConnectionKind::Clickhouse => "max_execution_time=30&async_insert=1",
                _ => "",
            },
            window,
            cx,
        );
        set(&self.ca_cert, "Optional PEM file, e.g. ~/ca.pem", window, cx);
        self.tls.sync_kind(window, cx);
        cx.refresh_windows();
    }

    pub fn picker_footer(&self) -> AnyElement {
        let kind = self.kind.get();
        let status = (!kind.is_supported()).then_some(" · Not supported");
        div()
            .debug_selector(|| "connection-picker-selected".into())
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w(px(0.0))
            .text_size(px(13.0))
            .text_color(theme::text_muted())
            .child(database_logo(kind, 18.0))
            .child(div().truncate().child(format!(
                "Selected: {}{}",
                kind_label(kind),
                status.unwrap_or_default()
            )))
            .into_any_element()
    }

    fn render_config(&self, height: Pixels) -> AnyElement {
        let kind = self.kind.get();
        let tabs = ConfigTab::for_kind(kind);
        let active = if tabs.contains(&self.tab.get()) {
            self.tab.get()
        } else {
            ConfigTab::Connection
        };
        let tab_bar = TabBar::new("connection-config-tabs")
            .segmented()
            .selected_index(tabs.iter().position(|tab| *tab == active).unwrap_or(0))
            .on_click({
                let tab = self.tab.clone();
                let tabs = tabs.clone();
                move |index, _, cx| {
                    if let Some(next) = tabs.get(*index) {
                        tab.set(*next);
                        cx.refresh_windows();
                    }
                }
            })
            .children(tabs.iter().map(|tab| {
                let selector = tab.selector();
                Tab::new()
                    .label(tab.label())
                    .debug_selector(move || selector.into())
            }));
        let content = match active {
            ConfigTab::Connection => self.connection_tab(kind),
            ConfigTab::Tls => self.tls.render(self.use_uri.get()),
            ConfigTab::Tunnel => self.tunnel_tab(),
            ConfigTab::Advanced => self.advanced_tab(kind),
            ConfigTab::Overrides => self.overrides_tab(),
        };
        let body = div()
            .debug_selector(|| "connection-config-body".into())
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .pt(px(14.0))
            .pb(px(6.0))
            .pr(px(6.0))
            .overflow_y_scrollbar()
            .child(content);
        div()
            .debug_selector(|| "connection-config".into())
            .h(height)
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .debug_selector(|| "connection-config-tab-bar".into())
                    .flex_shrink_0()
                    .child(tab_bar),
            )
            .child(body)
            .into_any_element()
    }

    fn connection_tab(&self, kind: ConnectionKind) -> AnyElement {
        let mut body = div().flex().flex_col().gap(px(14.0));
        if kind != ConnectionKind::Sqlite {
            let inputs_for_url = self.clone();
            let url_control = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(Input::new(&self.uri).appearance(true)),
                )
                .child(
                    Button::new("apply-connection-url")
                        .debug_selector(|| "apply-connection-url".into())
                        .icon(Icon::empty().path("icons/link.svg").size(px(14.0)))
                        .outline()
                        .w(px(32.0))
                        .h(px(32.0))
                        .tooltip("Parse the URL into the fields below")
                        .on_click(move |_, window, cx| {
                            let result = inputs_for_url.apply_uri_to_fields(window, cx);
                            *inputs_for_url.url_error.borrow_mut() = result.err();
                            cx.refresh_windows();
                        }),
                );
            let url_error = self.url_error.borrow().clone();
            body = body.child(config_row(
                "URL",
                "connection-config-url",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(url_control)
                    .children(url_error.map(|error| {
                        div()
                            .debug_selector(|| "connection-config-url-error".into())
                            .text_size(px(11.0))
                            .text_color(theme::danger())
                            .child(error)
                    })),
            ));
        }
        body = body.child(config_row(
            "Name",
            "connection-config-name",
            Input::new(&self.name).appearance(true),
        ));
        body = body.child(config_row(
            "Type",
            "connection-config-type",
            config_type_selector(
                kind,
                self.step.clone(),
                #[cfg(feature = "ui-scenarios")]
                self.scenario_id(),
                #[cfg(not(feature = "ui-scenarios"))]
                0,
                #[cfg(feature = "ui-scenarios")]
                self.scenario_enabled,
                #[cfg(not(feature = "ui-scenarios"))]
                false,
            ),
        ));
        body = body.child(config_row(
            "Environment",
            "connection-config-environment",
            self.environment_control(),
        ));
        body = body.child(config_row(
            "Color",
            "connection-config-color",
            self.color_swatches(),
        ));
        if kind == ConnectionKind::Sqlite {
            body = body.child(self.sqlite_file_row());
        } else if self.use_uri.get() {
            body = body.child(
                div()
                    .debug_selector(|| "connection-config-cluster-url".into())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(
                        "This URL uses a cluster format (mongodb+srv or several hosts) and is \
                         used as entered.",
                    ),
            );
            body = body.child(self.password_rows());
        } else {
            body = body.child(self.network_rows(kind));
        }
        body.into_any_element()
    }

    fn network_rows(&self, kind: ConnectionKind) -> AnyElement {
        let mut fields = div().flex().flex_col().gap(px(14.0));
        let host_and_port = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(Input::new(&self.host).appearance(true)),
            )
            .child(
                div()
                    .debug_selector(|| "connection-config-port".into())
                    .w(px(130.0))
                    .flex_shrink_0()
                    .child(NumberInput::new(&self.port)),
            );
        fields = fields.child(config_row("Host", "connection-config-host", host_and_port));
        if kind == ConnectionKind::Clickhouse {
            fields = fields.child(self.clickhouse_tls_rows());
        }
        if kind == ConnectionKind::Redis {
            fields = fields.child(self.redis_tls_rows());
        }
        fields = fields.child(config_row(
            "User",
            "connection-config-username",
            Input::new(&self.username).appearance(true),
        ));
        fields = fields.child(self.password_rows());
        fields = fields.child(config_row(
            "Database",
            "connection-config-database",
            Input::new(&self.database).appearance(true),
        ));
        if kind == ConnectionKind::Mongodb {
            fields = fields.child(config_row(
                "Auth DB",
                "connection-config-auth-database",
                Input::new(&self.auth_database).appearance(true),
            ));
        }
        fields = fields.child(config_row(
            if kind == ConnectionKind::Clickhouse { "Settings" } else { "URL Params" },
            "connection-config-uri-options",
            Input::new(&self.uri_options).appearance(true),
        ));
        fields.into_any_element()
    }

    /// ClickHouse: the HTTPS toggle (it moves a default port between 8123 and
    /// 8443) and the optional CA certificate file.
    fn clickhouse_tls_rows(&self) -> AnyElement {
        let secure = Checkbox::new("clickhouse-secure")
            .label("Use HTTPS (TLS)")
            .checked(self.secure.get())
            .on_click({
                let inputs = self.clone();
                move |checked, window, cx| {
                    inputs.secure.set(*checked);
                    let port = inputs.port.read(cx).value().to_string();
                    if let Some(next) =
                        ConnectionFormState::clickhouse_port_for_secure(&port, *checked)
                    {
                        replace_input(&inputs.port, next, window, cx);
                    }
                    cx.refresh_windows();
                }
            });
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(config_row("Secure", "connection-config-secure", secure))
            .child(config_row(
                "CA Cert",
                "connection-config-ca-cert",
                Input::new(&self.ca_cert).appearance(true),
            ))
            .into_any_element()
    }

    /// Redis: the TLS toggle (`rediss://`); a default port stays 6379.
    fn redis_tls_rows(&self) -> AnyElement {
        let secure = Checkbox::new("redis-secure")
            .label("Use TLS (rediss://)")
            .checked(self.secure.get())
            .on_click({
                let inputs = self.clone();
                move |checked, _, cx| {
                    inputs.secure.set(*checked);
                    cx.refresh_windows();
                }
            });
        config_row("Secure", "connection-config-secure", secure)
    }

    /// Password (eye toggle inside the input) plus the Save-password checkbox
    /// with its help tooltip.
    fn password_rows(&self) -> AnyElement {
        let save_password = Checkbox::new("save-password")
            .label("Save password")
            .checked(self.save_password.get())
            .on_click({
                let save_password = self.save_password.clone();
                move |checked, _, cx| {
                    save_password.set(*checked);
                    cx.refresh_windows();
                }
            });
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(config_row(
                "Password",
                "connection-config-password",
                Input::new(&self.password).mask_toggle().appearance(true),
            ))
            .child(
                div()
                    .debug_selector(|| "connection-config-save-password".into())
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(LABEL_WIDTH + 12.0))
                    .child(save_password)
                    .child(
                        div()
                            .id("connection-save-password-help")
                            .debug_selector(|| "connection-save-password-help".into())
                            .flex()
                            .items_center()
                            .child(
                                Icon::empty()
                                    .path("icons/circle-help.svg")
                                    .size(px(14.0))
                                    .text_color(theme::text_muted()),
                            )
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new(format!(
                                    "The password is stored in the {}, so you won't need \
                                     to enter it again.",
                                    crate::platform::CREDENTIAL_STORE_LABEL
                                ))
                                .build(window, cx)
                            }),
                    ),
            )
            .into_any_element()
    }

    fn sqlite_file_row(&self) -> AnyElement {
        let file = self.file_path.clone();
        config_row(
            "File Path",
            "connection-config-file",
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(Input::new(&self.file_path).appearance(true)),
                )
                .child(
                    Button::new("connection-browse-file")
                        .debug_selector(|| "connection-browse-file".into())
                        .icon(Icon::empty().path("icons/folder-open.svg").size(px(14.0)))
                        .outline()
                        .w(px(32.0))
                        .h(px(32.0))
                        .tooltip("Choose a database file")
                        .on_click(move |_, window, cx| {
                            let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                                files: true,
                                directories: false,
                                multiple: false,
                                prompt: None,
                            });
                            let file = file.clone();
                            window
                                .spawn(cx, async move |cx| {
                                    if let Ok(Ok(Some(paths))) = picked.await {
                                        if let Some(path) = paths.first() {
                                            let text = path.display().to_string();
                                            let _ = cx.update(|window, cx| {
                                                replace_input(&file, &text, window, cx)
                                            });
                                        }
                                    }
                                })
                                .detach();
                        }),
                ),
        )
    }

    fn color_swatches(&self) -> AnyElement {
        let current = self.color.borrow().clone();
        let selected_value = current.clone().unwrap_or_default();
        let custom_selected = !selected_value.is_empty() && !is_preset_color(&selected_value);
        let mut row = div().flex().items_center().gap(px(8.0)).h(px(32.0));
        for (index, (value, name)) in COLOR_SWATCHES.iter().enumerate() {
            let is_selected = selected_value == *value;
            let swatch_color = parse_swatch(value);
            let color = self.color.clone();
            let value = *value;
            row = row.child(
                div()
                    .id(("connection-color", index))
                    .debug_selector(move || format!("connection-color-{index}"))
                    .size(px(24.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .cursor_pointer()
                    .border_1()
                    .when(swatch_color.is_none(), |this| this.border_dashed())
                    .border_color(theme::border())
                    .when_some(swatch_color, |this, rgb| this.bg(rgb))
                    .when(is_selected, |this| {
                        this.border_2().border_color(theme::text())
                    })
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(*name).build(window, cx)
                    })
                    .on_click(move |_, _, cx| {
                        *color.borrow_mut() = Some(value.to_string());
                        cx.refresh_windows();
                    }),
            );
        }
        let custom = Popover::new("connection-custom-color")
            .trigger(
                Button::new("connection-custom-color-button")
                    .debug_selector(|| "connection-custom-color".into())
                    .icon(Icon::empty().path("icons/pipette.svg").size(px(13.0)))
                    .size(px(24.0))
                    .p(px(0.0))
                    .rounded_full()
                    .outline()
                    .when(!custom_selected, |this| this.border_dashed())
                    .when(custom_selected, |this| {
                        this.bg(parse_swatch(&selected_value).unwrap_or(gpui::rgb(0x888888)))
                    })
                    .tooltip("Custom color"),
            )
            .p(px(8.0))
            .child(
                div()
                    .w(px(150.0))
                    .child(Input::new(&self.custom_color).appearance(true)),
            );
        row.child(custom).into_any_element()
    }

    fn tunnel_tab(&self) -> AnyElement {
        self.ssh.render(self.saved.get())
    }

    fn advanced_tab(&self, kind: ConnectionKind) -> AnyElement {
        let timeout_supported = matches!(kind, ConnectionKind::Postgres | ConnectionKind::Mongodb);
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(config_row(
                "Timeout",
                "connection-config-timeout",
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .w(px(130.0))
                            .child(NumberInput::new(&self.connect_timeout).disabled(!timeout_supported)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(if timeout_supported {
                                "seconds (empty = driver default)"
                            } else {
                                "not applicable to file databases"
                            }),
                    ),
            ))
            .child(config_row(
                "Options",
                "connection-config-options",
                Checkbox::new("pin-connection")
                    .label("Pin connection in sidebar")
                    .checked(self.is_pinned.get())
                    .on_click({
                        let is_pinned = self.is_pinned.clone();
                        move |checked, _, cx| {
                            is_pinned.set(*checked);
                            cx.refresh_windows();
                        }
                    }),
            ))
            .child(notice(
                "connection-config-readonly-note",
                "Read-only, confirmations and the other safety presets belong to the connection type (Environment on the Connection tab). Edit the types in Settings \u{203A} Query & Connections.",
            ))
            .into_any_element()
    }

    /// The connection-type select with a line that says what the type does.
    fn environment_control(&self) -> AnyElement {
        let chosen = self.type_id.borrow().clone();
        let summary = self
            .types
            .iter()
            .find(|ty| ty.id == chosen)
            .map_or_else(
                || "No type: the global settings apply and nothing is confirmed.".to_owned(),
                preset_summary,
            );
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .debug_selector(|| "connection-config-environment-select".into())
                    .child(Select::new(&self.type_select)),
            )
            .child(
                div()
                    .debug_selector(|| "connection-config-environment-summary".into())
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(summary),
            )
            .into_any_element()
    }

    /// Puts the dialog on the Overrides tab with the connection's own agent level, optionally with
    /// the dropdown open (offscreen captures).
    #[cfg(all(test, feature = "bench"))]
    pub fn show_agent_for_capture(
        &self,
        level: Option<AgentLevel>,
        open: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.tab.set(ConfigTab::Overrides);
        if let Some(level) = level {
            self.agent_custom.set(true);
            self.agent_level.set(level);
            self.agent_select.update(cx, |select, cx| select.set_selected_value(&level, window, cx));
        }
        if open {
            self.agent_select.update(cx, |select, cx| select.focus(window, cx));
            window.dispatch_keystroke(gpui::Keystroke::parse("down").expect("a key"), cx);
        }
        cx.refresh_windows();
    }

    /// What the Agent access row says the connection ends up with (tests).
    #[cfg(test)]
    pub fn effective_agent_for_test(&self, visual: &mut gpui::VisualTestContext) -> engine_connect::profile::AgentAccess {
        visual.update(|_, cx| {
            let profile = self.read_profile(cx);
            self.global.for_connection(&profile).agent_access
        })
    }

    /// The "Resulting level" line of the Agent access row (tests).
    #[cfg(test)]
    pub fn agent_note_for_test(&self, visual: &mut gpui::VisualTestContext) -> String {
        visual.update(|_, cx| {
            let profile = self.read_profile(cx);
            AgentResolution::of(&self.global.connection_types, &profile).note
        })
    }

    /// The level the Agent access select shows (tests).
    #[cfg(test)]
    pub fn agent_select_value(&self, cx: &App) -> Option<AgentLevel> {
        self.agent_select.read(cx).selected_value().copied()
    }

    /// The label the Environment select shows (tests).
    #[cfg(test)]
    pub fn type_select_value(&self, cx: &App) -> Option<String> {
        self.type_select.read(cx).selected_value().map(|label| label.to_string())
    }

    /// Per-connection settings page: each overridable setting is
    /// either "Use global (value)" or has its own value.
    fn overrides_tab(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(notice(
                "connection-config-overrides-notice",
                "Each setting follows the global value (Settings) unless you uncheck \"Use global\" and give this connection its own.",
            ))
            .children(
                self.override_rows
                    .iter()
                    .enumerate()
                    .map(|(index, row)| self.override_row(index, row)),
            )
            .child(self.agent_access_row())
            .into_any_element()
    }

    /// "Agent access": how far an AI agent may go on this connection. Either the type's default
    /// ("Use default") or the connection's own level; below it, what the connection ends up with.
    fn agent_access_row(&self) -> AnyElement {
        let custom = self.agent_custom.get();
        let profile = self.read_profile_agent_only();
        let effective_with = |profile: &ConnectionProfile| self.global.for_connection(profile).agent_access;
        let default_level = effective_with(&ConnectionProfile { overrides: Overrides::default(), ..profile.clone() });
        let use_default = Checkbox::new("override-use-global-agent")
            .label(format!("Use default ({})", default_level.as_str().replace('_', " ")))
            .checked(!custom)
            .on_click({
                let custom = self.agent_custom.clone();
                move |checked, _, cx| {
                    custom.set(!*checked);
                    cx.refresh_windows();
                }
            });
        let explanation = if custom {
            self.agent_level.get().description().to_owned()
        } else {
            "Follows the connection type's default. Off unless a type or this connection says otherwise.".to_owned()
        };
        let resolution = AgentResolution::of(&self.global.connection_types, &profile);
        div()
            .debug_selector(|| "override-row-agent.access".into())
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .child("Agent access"),
            )
            .child(
                div().text_size(px(11.0)).text_color(theme::text_muted()).child(
                    "How far an AI agent (the in-app assistant, MCP clients) may go on this connection. \
                     A Production type needs an explicit level here, and a read-only type caps it at Read.",
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .child(
                        div()
                            .debug_selector(|| "override-global-agent.access".into())
                            .flex_shrink_0()
                            .child(use_default),
                    )
                    .when(custom, |this| {
                        this.child(
                            div()
                                .debug_selector(|| "override-control-agent.access".into())
                                .w(px(190.0))
                                .child(Select::new(&self.agent_select).menu_width(px(400.0))),
                        )
                    }),
            )
            .child(
                div()
                    .debug_selector(|| "override-agent-explanation".into())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(explanation),
            )
            .child(
                div()
                    .debug_selector(|| "override-agent-effective".into())
                    .text_size(px(12.0))
                    .text_color(theme::text())
                    .child(format!("Resulting level: {}", resolution.note)),
            )
            .into_any_element()
    }

    /// The profile as it stands for the agent row (type and the agent override only).
    fn read_profile_agent_only(&self) -> ConnectionProfile {
        let mut overrides = Overrides::default();
        if self.agent_custom.get() {
            overrides.set(AGENT_ACCESS, OverrideValue::Text(self.agent_level.get().id().to_owned()));
        }
        ConnectionProfile { type_id: self.type_id.borrow().clone(), overrides }
    }

    fn override_row(&self, index: usize, row: &OverrideRow) -> AnyElement {
        let descriptor = row.descriptor;
        let id = descriptor.id;
        let custom = row.custom.get();
        let use_global = Checkbox::new(("override-use-global", index))
            .label(format!("Use global ({})", global_label(descriptor, &self.global)))
            .checked(!custom)
            .on_click({
                let custom = row.custom.clone();
                move |checked, _, cx| {
                    custom.set(!*checked);
                    cx.refresh_windows();
                }
            });
        let control = custom.then(|| match (&descriptor.kind, &row.input) {
            (Kind::Toggle, _) => Switch::new(("override-switch", index))
                .checked(row.toggle.get())
                .on_click({
                    let toggle = row.toggle.clone();
                    move |checked, _, cx| {
                        toggle.set(*checked);
                        cx.refresh_windows();
                    }
                })
                .into_any_element(),
            (Kind::Number { .. }, Some(input)) => {
                div().w(px(130.0)).child(NumberInput::new(input)).into_any_element()
            }
            _ => div().into_any_element(),
        });
        div()
            .debug_selector(move || format!("override-row-{id}"))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .child(descriptor.title),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(descriptor.description),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .child(
                        div()
                            .debug_selector(move || format!("override-global-{id}"))
                            .flex_shrink_0()
                            .child(use_global),
                    )
                    .children(control.map(|control| {
                        div()
                            .debug_selector(move || format!("override-control-{id}"))
                            .child(control)
                    })),
            )
            .into_any_element()
    }

    pub fn apply_uri_to_fields(&self, window: &mut Window, cx: &mut App) -> Result<(), String> {
        let current_password = self.password.read(cx).value().to_string();
        let mut form = ConnectionFormState {
            kind: self.kind.get(),
            uri: self.uri.read(cx).value().to_string(),
            use_uri: true,
            ..Default::default()
        };
        crate::connections::validate_uri(form.kind, &form.uri)?;
        form.parse_uri_fields();
        if form.use_uri {
            return Err("This URL uses a cluster format; keep URL mode enabled.".to_string());
        }
        replace_input(&self.host, &form.host, window, cx);
        replace_input(&self.port, &form.port, window, cx);
        replace_input(&self.username, &form.username, window, cx);
        replace_input(&self.database, &form.database, window, cx);
        replace_input(&self.auth_database, &form.auth_database, window, cx);
        replace_input(&self.uri_options, &form.uri_options, window, cx);
        replace_input(&self.connect_timeout, &form.connect_timeout, window, cx);
        replace_input(&self.ca_cert, &form.ca_cert, window, cx);
        self.secure.set(form.secure);
        self.tls.fill(&form.tls, window, cx);
        replace_input(&self.file_path, &form.file_path, window, cx);
        replace_input(
            &self.password,
            if form.password.is_empty() {
                &current_password
            } else {
                &form.password
            },
            window,
            cx,
        );
        self.use_uri.set(false);
        cx.refresh_windows();
        Ok(())
    }

    #[cfg(test)]
    pub fn set_uri_mode(
        &self,
        use_uri: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        if use_uri {
            let mut form = self.read_form(cx, ConnectionFormState::new(self.kind.get()));
            form.use_uri = false;
            form.password.clear();
            let uri = uri_for_url_mode(&form)?;
            if uri.is_empty() {
                return Err("Enter connection fields before switching to URL mode.".to_string());
            }
            replace_input(&self.uri, &uri, window, cx);
        } else {
            self.apply_uri_to_fields(window, cx)?;
        }
        self.use_uri.set(use_uri);
        cx.refresh_windows();
        Ok(())
    }
}

const NO_TYPE_LABEL: &str = "None (global settings)";

/// One overridable setting on the Overrides tab.
#[derive(Clone)]
struct OverrideRow {
    descriptor: &'static SettingDescriptor,
    /// "Use global" is unchecked: the connection has its own value.
    custom: Rc<Cell<bool>>,
    /// The own value of a toggle.
    toggle: Rc<Cell<bool>>,
    /// The own value of a number.
    input: Option<Entity<InputState>>,
}

/// How the global value reads in "Use global (...)".
fn global_label(descriptor: &SettingDescriptor, settings: &Settings) -> String {
    match descriptor.get(settings) {
        Value::Bool(true) => "on".into(),
        Value::Bool(false) => "off".into(),
        Value::Number(number) => format_number(number),
        Value::Text(text) => text,
        Value::Choice(id) => id.into(),
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}

/// What a connection type does, for the line under the Environment select.
fn preset_summary(ty: &ConnectionType) -> String {
    let mut parts = Vec::new();
    if ty.confirm_execute {
        parts.push("asks before running SQL");
    }
    if ty.confirm_data_change {
        parts.push("asks before applying data changes");
    }
    if ty.read_only {
        parts.push("blocks writes (read-only)");
    }
    if parts.is_empty() {
        format!("{}: no safety presets.", ty.name)
    } else {
        format!("{}: {}.", ty.name, parts.join(", "))
    }
}

fn input(window: &mut Window, cx: &mut App, value: &str, masked: bool) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(move |cx| {
        InputState::new(window, cx)
            .default_value(value)
            .masked(masked)
    })
}

fn replace_input(input: &Entity<InputState>, value: &str, window: &mut Window, cx: &mut App) {
    let value = value.to_string();
    input.update(cx, |state, cx| state.set_value(value, window, cx));
}

#[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
fn database_picker(
    selected: Rc<Cell<ConnectionKind>>,
    search: Entity<InputState>,
    filter: Rc<RefCell<String>>,
    (category, view): (Rc<Cell<PickerCategory>>, Rc<Cell<PickerView>>),
    step: Rc<Cell<ConnectionDialogStep>>,
    on_select: OnKindSelected,
    drivers: DriverAvailability,
    height: Pixels,
) -> impl IntoElement {
    let query = filter.borrow().trim().to_ascii_lowercase();
    let search_active = !query.is_empty();
    let visible_categories = PickerCategory::all()
        .into_iter()
        .filter(|candidate| {
            (search_active || *candidate == category.get())
                && connection_picker_catalog::available_entries(*candidate, &drivers)
                    .any(|entry| connection_picker_catalog::matches_query(entry, &query))
        })
        .collect::<Vec<_>>();

    let mut results = div()
        .debug_selector(|| "connection-picker-results".into())
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .gap(px(16.0))
        .overflow_y_scrollbar();
    if search_active {
        results = results.child(
            div()
                .text_size(px(13.0))
                .font_semibold()
                .text_color(theme::text())
                .child("Search results"),
        );
    }
    for candidate in visible_categories.iter().copied() {
        let entries = connection_picker_catalog::available_entries(candidate, &drivers)
            .filter(|entry| connection_picker_catalog::matches_query(*entry, &query))
            .collect::<Vec<_>>();
        let mut section = div().flex().flex_col().gap(px(8.0));
        if search_active {
            section = section.child(
                div()
                    .text_size(px(12.0))
                    .font_semibold()
                    .text_color(theme::text_muted())
                    .child(candidate.label()),
            );
        }
        if view.get() == PickerView::Grid {
            let mut grid = div()
                .debug_selector(|| "connection-picker-grid".into())
                .flex()
                .flex_wrap()
                .w_full()
                .gap(px(8.0));
            for entry in entries {
                grid = grid.child(picker_tile(
                    entry,
                    entry.needs_download(&drivers),
                    selected.clone(),
                    category.clone(),
                    step.clone(),
                    on_select.clone(),
                ));
            }
            section = section.child(grid);
        } else {
            let mut list = div()
                .debug_selector(|| "connection-picker-list".into())
                .flex()
                .flex_col()
                .gap(px(6.0));
            for entry in entries {
                list = list.child(picker_row(
                    entry,
                    entry.needs_download(&drivers),
                    selected.clone(),
                    category.clone(),
                    step.clone(),
                    on_select.clone(),
                ));
            }
            section = section.child(list);
        }
        results = results.child(section);
    }

    let results = if visible_categories.is_empty() {
        div()
            .debug_selector(|| "connection-picker-empty".into())
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(if search_active {
                "No matching databases."
            } else {
                "No database types available in this category yet."
            })
            .into_any_element()
    } else {
        results.into_any_element()
    };

    let mut categories = div()
        .debug_selector(|| "connection-picker-categories".into())
        .w(px(160.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .border_r_1()
        .border_color(theme::border())
        .pr(px(8.0));
    for candidate in PickerCategory::all()
        .into_iter()
        .filter(|candidate| candidate.has_available(&drivers))
    {
        let selected_category = category.get() == candidate && !search_active;
        categories = categories.child(
            Button::new(picker_category_selector(candidate))
                .debug_selector(move || picker_category_selector(candidate).into())
                .w(px(152.0))
                .h(px(32.0))
                .px(px(10.0))
                .justify_start()
                .compact()
                .ghost()
                .selected(selected_category)
                .child(
                    div()
                        .flex_1()
                        .text_left()
                        .text_size(px(14.0))
                        .text_color(if selected_category {
                            theme::text()
                        } else {
                            theme::text_muted()
                        })
                        .child(candidate.label()),
                )
                .on_click({
                    let category = category.clone();
                    move |_, _, cx| {
                        category.set(candidate);
                        cx.refresh_windows();
                    }
                }),
        );
    }

    div()
        .debug_selector(|| "connection-picker".into())
        .h(height)
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(
            div()
                .debug_selector(|| "connection-picker-toolbar".into())
                .flex()
                .items_center()
                .h(px(32.0))
                .gap(px(8.0))
                .child(
                    div()
                        .debug_selector(|| "connection-picker-view-switch".into())
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .h(px(28.0))
                        .rounded(px(6.0))
                        .overflow_hidden()
                        .border_1()
                        .border_color(theme::border())
                        .child(view_toggle_button(
                            "connection-picker-grid-toggle",
                            "icons/layout-dashboard.svg",
                            "Grid view",
                            view.get() == PickerView::Grid,
                            view.clone(),
                            PickerView::Grid,
                        ))
                        .child(div().w(px(1.0)).h_full().flex_shrink_0().bg(theme::border()))
                        .child(view_toggle_button(
                            "connection-picker-list-toggle",
                            "icons/list-ordered.svg",
                            "List view",
                            view.get() == PickerView::List,
                            view.clone(),
                            PickerView::List,
                        )),
                )
                .child(
                    div()
                        .debug_selector(|| "connection-picker-search".into())
                        .w(px(260.0))
                        .child(
                            Input::new(&search)
                                .prefix(Icon::empty().path("icons/search.svg").size(px(14.0)))
                                .appearance(true),
                        ),
                ),
        )
        .child(
            div()
                .debug_selector(|| "connection-picker-body".into())
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .gap(px(18.0))
                .overflow_hidden()
                .child(categories)
                .child(results),
        )
}

fn view_toggle_button(
    id: &'static str,
    icon: &'static str,
    tooltip: &'static str,
    active: bool,
    view: Rc<Cell<PickerView>>,
    target: PickerView,
) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .icon(
            Icon::empty()
                .path(icon)
                .size(px(14.0))
                .text_color(if active { theme::text() } else { theme::text_muted() }),
        )
        .compact()
        .ghost()
        .selected(active)
        .rounded(px(0.0))
        .w(px(28.0))
        .h(px(26.0))
        .tooltip(tooltip)
        .on_click(move |_, _, cx| {
            view.set(target);
            cx.refresh_windows();
        })
}

/// The small "download" badge of a kind whose driver is not installed yet.
fn download_badge(entry: PickerEntry) -> impl IntoElement {
    div()
        .debug_selector(move || format!("connection-picker-download-{}", entry.id))
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .text_size(px(10.0))
        .text_color(theme::text_muted())
        .child("download")
}

fn picker_tile(
    entry: PickerEntry,
    download: bool,
    selected: Rc<Cell<ConnectionKind>>,
    category: Rc<Cell<PickerCategory>>,
    step: Rc<Cell<ConnectionDialogStep>>,
    on_select: OnKindSelected,
) -> Button {
    let is_selected = entry.kind == Some(selected.get());
    picker_button(entry, selected, category, step, on_select)
        .debug_selector(move || picker_selector(entry))
        .relative()
        .w(px(124.0))
        .h(px(100.0))
        .p(px(6.0))
        .outline()
        .selected(is_selected)
        .when(download, |this| {
            this.child(
                div()
                    .absolute()
                    .top(px(4.0))
                    .right(px(4.0))
                    .child(download_badge(entry)),
            )
        })
        .child(
            div()
                .w(px(108.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(
                    div()
                        .size(px(32.0))
                        .debug_selector(move || picker_logo_selector(entry))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme::surface())
                        .child(picker_logo(entry, 24.0)),
                )
                .child(
                    div()
                        .debug_selector(move || {
                            format!("connection-picker-label-{}", entry.id)
                        })
                        .w(px(108.0))
                        .h(px(30.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .line_height(px(15.0))
                        .text_center()
                        .text_color(theme::text())
                        .child(
                            div()
                                .debug_selector(move || {
                                    format!("connection-picker-label-text-{}", entry.id)
                                })
                                .w_full()
                                .whitespace_normal()
                                .child(picker_tile_label(entry.label)),
                        ),
                ),
        )
}

fn picker_row(
    entry: PickerEntry,
    download: bool,
    selected: Rc<Cell<ConnectionKind>>,
    category: Rc<Cell<PickerCategory>>,
    step: Rc<Cell<ConnectionDialogStep>>,
    on_select: OnKindSelected,
) -> Button {
    let is_selected = entry.kind == Some(selected.get());
    picker_button(entry, selected, category, step, on_select)
        .debug_selector(move || picker_selector(entry))
        .w_full()
        .h(px(40.0))
        .px(px(10.0))
        .outline()
        .selected(is_selected)
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_start()
                .gap(px(10.0))
                .child(
                    div()
                        .size(px(28.0))
                        .debug_selector(move || picker_logo_selector(entry))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .bg(theme::surface())
                        .child(picker_logo(entry, 18.0)),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme::text())
                        .child(entry.label),
                )
                .when(download, |this| this.child(download_badge(entry))),
        )
}

/// Runs when a picker entry is chosen (updates the config placeholders).
type OnKindSelected = Rc<dyn Fn(ConnectionKind, &mut Window, &mut App)>;

fn picker_button(
    entry: PickerEntry,
    selected: Rc<Cell<ConnectionKind>>,
    category: Rc<Cell<PickerCategory>>,
    step: Rc<Cell<ConnectionDialogStep>>,
    on_select: OnKindSelected,
) -> Button {
    let button = Button::new(entry.id);
    if !entry.is_supported() {
        // Listed with the catalog, but dimmed and not selectable.
        return button.disabled(true).tooltip("Not supported yet");
    }
    button.on_click(move |event, window, cx| {
        let Some(kind) = entry.kind else {
            return;
        };
        selected.set(kind);
        on_select(kind, window, cx);
        category.set(category_for_kind(kind));
        if event.click_count() >= 2 {
            step.set(ConnectionDialogStep::Config);
        }
        cx.refresh_windows();
    })
}

fn database_logo(kind: ConnectionKind, size: f32) -> impl IntoElement {
    if let Some(asset) = database_asset(kind) {
        return img(asset).w(px(size)).h(px(size)).into_any_element();
    }
    monogram_logo(kind_label(kind), size).into_any_element()
}

fn database_asset(kind: ConnectionKind) -> Option<&'static str> {
    match kind {
        ConnectionKind::Postgres => Some("icons/postgres.svg"),
        ConnectionKind::Sqlite => Some("icons/sqlite.svg"),
        ConnectionKind::Mongodb => Some("icons/mongodb.svg"),
        ConnectionKind::Clickhouse => Some("icons/clickhouse.svg"),
        ConnectionKind::Redis => Some("icons/redis.svg"),
        ConnectionKind::Mysql => Some("icons/mysql-mark.svg"),
        ConnectionKind::Mariadb => Some("icons/mariadb.svg"),
    }
}

fn picker_logo(entry: PickerEntry, size: f32) -> AnyElement {
    picker_logo_asset(entry)
        .map(|asset| img(asset).w(px(size)).h(px(size)).into_any_element())
        .unwrap_or_else(|| monogram_logo(entry.monogram, size).into_any_element())
}

fn picker_logo_asset(entry: PickerEntry) -> Option<&'static str> {
    connection_picker_catalog::logo_asset(entry.id).or_else(|| entry.kind.and_then(database_asset))
}

fn picker_tile_label(label: &'static str) -> &'static str {
    match label {
        "Custom (MySQL)" => "Custom\n(MySQL)",
        "Custom (PostgreSQL)" => "Custom\n(PostgreSQL)",
        _ => label,
    }
}

fn monogram_logo(label: &'static str, size: f32) -> impl IntoElement {
    div()
        .text_size(px((size * 0.48).min(14.0)))
        .font_semibold()
        .text_color(theme::text())
        .child(label)
}

#[cfg(test)]
fn uri_for_url_mode(form: &ConnectionFormState) -> Result<String, String> {
    if form.kind == ConnectionKind::Mongodb && is_mongodb_cluster_uri(&form.uri) {
        return Ok(crate::connections::normalize_uri_scheme(&form.uri));
    }
    form.generated_uri()
}

#[cfg(test)]
fn is_mongodb_cluster_uri(uri: &str) -> bool {
    let uri = uri.trim();
    if uri
        .get(..14)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mongodb+srv://"))
    {
        return true;
    }
    let Some((scheme, authority_and_rest)) = uri.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("mongodb") {
        return false;
    }
    let authority = authority_and_rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let hosts = authority
        .rsplit_once('@')
        .map(|(_, hosts)| hosts)
        .unwrap_or(authority);
    hosts.contains(',')
}

fn category_for_kind(kind: ConnectionKind) -> PickerCategory {
    match kind {
        ConnectionKind::Postgres | ConnectionKind::Mysql | ConnectionKind::Mariadb => {
            PickerCategory::Relational
        }
        ConnectionKind::Clickhouse => PickerCategory::Analytics,
        ConnectionKind::Sqlite => PickerCategory::Lightweight,
        ConnectionKind::Mongodb | ConnectionKind::Redis => PickerCategory::DocsCacheSearch,
    }
}

fn picker_category_selector(category: PickerCategory) -> &'static str {
    match category {
        PickerCategory::Relational => "connection-picker-category-relational",
        PickerCategory::Analytics => "connection-picker-category-analytics",
        PickerCategory::Lightweight => "connection-picker-category-lightweight",
        PickerCategory::DocsCacheSearch => "connection-picker-category-docs-cache-search",
        PickerCategory::GraphVector => "connection-picker-category-graph-vector",
        PickerCategory::TimeSeries => "connection-picker-category-time-series",
        PickerCategory::MessageQueues => "connection-picker-category-message-queues",
        PickerCategory::RegistryConfig => "connection-picker-category-registry-config",
    }
}

fn picker_logo_selector(entry: PickerEntry) -> String {
    format!("connection-picker-logo-{}", entry.id)
}

fn picker_selector(entry: PickerEntry) -> String {
    format!("connection-picker-{}", entry.id)
}

pub(crate) fn kind_label(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Postgres => "PostgreSQL",
        ConnectionKind::Sqlite => "SQLite",
        ConnectionKind::Mongodb => "MongoDB",
        ConnectionKind::Mysql => "MySQL",
        ConnectionKind::Mariadb => "MariaDB",
        ConnectionKind::Redis => "Redis",
        ConnectionKind::Clickhouse => "ClickHouse",
    }
}

const LABEL_WIDTH: f32 = 110.0;

fn config_row(
    label: &'static str,
    selector: &'static str,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .debug_selector(move || selector.into())
        .flex()
        .items_center()
        .gap(px(12.0))
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_shrink_0()
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme::text())
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.0)).child(control))
        .into_any_element()
}

fn notice(selector: &'static str, text: &'static str) -> AnyElement {
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

fn is_hex_color(text: &str) -> bool {
    text.len() == 7
        && text.starts_with('#')
        && text[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_preset_color(value: &str) -> bool {
    COLOR_SWATCHES
        .iter()
        .any(|(preset, _)| preset.eq_ignore_ascii_case(value))
}

fn parse_swatch(value: &str) -> Option<gpui::Rgba> {
    is_hex_color(value)
        .then(|| u32::from_str_radix(&value[1..], 16).ok())
        .flatten()
        .map(gpui::rgb)
}

fn config_type_selector(
    kind: ConnectionKind,
    step: Rc<Cell<ConnectionDialogStep>>,
    dialog_id: u64,
    scenario_enabled: bool,
) -> AnyElement {
    #[cfg(not(feature = "ui-scenarios"))]
    let _ = scenario_enabled;
    #[cfg(not(feature = "ui-scenarios"))]
    let _ = dialog_id;
    let button = Button::new("connection-config-type-control")
        .debug_selector(|| "connection-config-type-control".into())
        .w_full()
        .h(px(32.0))
        .px(px(10.0))
        .justify_start()
        .gap(px(8.0))
        .outline()
        .child(database_logo(kind, 18.0))
        .child(
            div()
                .flex_1()
                .text_size(px(14.0))
                .text_color(theme::text())
                .child(kind_label(kind)),
        )
        .child(
            Icon::empty()
                .path("icons/pencil.svg")
                .size(px(13.0))
                .text_color(theme::text_muted()),
        )
        .on_click(move |_, _, cx| {
            step.set(ConnectionDialogStep::Picker);
            cx.refresh_windows();
        });
    #[cfg(feature = "ui-scenarios")]
    {
        return div()
            .on_children_prepainted(move |children, window, _| {
                if !scenario_enabled {
                    return;
                }
                if let [bounds] = children.as_slice() {
                    crate::ui_scenario::emit_ui_bounds(
                        "connection-config-type-control",
                        "config",
                        dialog_id,
                        bounds.clone(),
                        true,
                        window,
                    );
                }
            })
            .child(button)
            .into_any_element();
    }
    #[cfg(not(feature = "ui-scenarios"))]
    {
        button.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn new_connection_picker_keeps_driver_availability_truthful() {
        assert!(ConnectionKind::Postgres.is_supported());
        assert!(ConnectionKind::Sqlite.is_supported());
        assert!(ConnectionKind::Mongodb.is_supported());
        assert!(ConnectionKind::Mysql.is_supported());
        assert!(ConnectionKind::Mariadb.is_supported());
        assert!(ConnectionKind::Clickhouse.is_supported());
        assert!(ConnectionKind::Redis.is_supported());
    }

    #[test]
    fn picker_uses_matching_brand_assets_for_every_available_logo() {
        for (id, asset) in [
            ("mysql", "icons/mysql.svg"),
            ("custom-mysql", "icons/mysql.svg"),
            ("postgres", "icons/postgres.svg"),
            ("custom-postgresql", "icons/postgres.svg"),
            ("mariadb", "icons/mariadb.svg"),
            ("sql-server", "icons/sql-server.svg"),
            ("oracle", "icons/oracle.svg"),
            ("firebird", "icons/firebird.svg"),
            ("cockroachdb", "icons/cockroachlabs.svg"),
            ("cloud-spanner", "icons/cloud-spanner.svg"),
            ("db2", "icons/db2.svg"),
            ("informix", "icons/informix.svg"),
            ("iris", "icons/iris.svg"),
            ("intersystems-cache", "icons/iris.svg"),
            ("dolt", "icons/dolt.svg"),
        ] {
            let entry = connection_picker_catalog::all_entries()
                .iter()
                .find(|entry| entry.id == id)
                .expect("catalog entry should exist");
            assert_eq!(picker_logo_asset(*entry), Some(asset));
        }
        // JDBCX's logo embeds a raster image, which the SVG renderer here
        // does not draw, so it keeps its monogram.
        let jdbcx = connection_picker_catalog::all_entries()
            .iter()
            .find(|entry| entry.id == "jdbcx")
            .expect("JDBCX should be catalogued");
        assert_eq!(connection_picker_catalog::logo_asset(jdbcx.id), None);
    }

    #[test]
    fn picker_tile_labels_break_before_parenthesized_subtypes() {
        assert_eq!(picker_tile_label("Custom (MySQL)"), "Custom\n(MySQL)");
        assert_eq!(
            picker_tile_label("Custom (PostgreSQL)"),
            "Custom\n(PostgreSQL)"
        );
        assert_eq!(
            picker_tile_label("InterSystems Caché"),
            "InterSystems Caché"
        );
    }

    #[gpui::test]
    fn picker_selects_redis_now_that_its_driver_route_is_enabled(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });

        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Redis);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            assert!(inputs.picker_selection_is_visible());
        });
    }

    #[gpui::test]
    fn picker_shows_only_connection_kinds_the_app_can_open(cx: &mut TestAppContext) {
        cx.update(|app| {
            gpui_component::init(app);
            app.set_reduce_motion(true);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog
                    .w(px(880.0))
                    .px(px(18.0))
                    .title("New Connection")
                    .child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("connection-picker").is_some());
        // Only kinds the app can open are offered; the rest of the catalog
        // stays hidden until its driver route is supported.
        for selector in [
            "connection-picker-postgres",
            "connection-picker-mysql",
            "connection-picker-mariadb",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "missing entry: {selector}");
        }
        for selector in [
            "connection-picker-sql-server",
            "connection-picker-oracle",
            "connection-picker-firebird",
            "connection-picker-cockroachdb",
            "connection-picker-db2",
            "connection-picker-informix",
            "connection-picker-jdbcx",
            "connection-picker-cloud-spanner",
            "connection-picker-iris",
            "connection-picker-intersystems-cache",
            "connection-picker-custom-mysql",
            "connection-picker-dolt",
            "connection-picker-custom-postgresql",
            "connection-picker-jdbc",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "unsupported entry must be hidden: {selector}"
            );
        }
        // Categories without an available kind are hidden too.
        for (category, visible) in [
            (PickerCategory::Relational, true),
            (PickerCategory::Lightweight, true),
            (PickerCategory::DocsCacheSearch, true),
            (PickerCategory::Analytics, true),
            (PickerCategory::GraphVector, false),
            (PickerCategory::TimeSeries, false),
            (PickerCategory::MessageQueues, false),
            (PickerCategory::RegistryConfig, false),
        ] {
            assert_eq!(
                cx.debug_bounds(picker_category_selector(category)).is_some(),
                visible,
                "category visibility: {}",
                category.label()
            );
        }
        // Tile and grid geometry is unchanged.
        let tile = cx
            .debug_bounds("connection-picker-postgres")
            .expect("PostgreSQL tile should render");
        assert_eq!(tile.size.width, px(124.0));
        assert_eq!(tile.size.height, px(100.0));
        let grid = cx
            .debug_bounds("connection-picker-grid")
            .expect("grid should render");
        assert_eq!(grid.size.width, px(664.0));
        let label = cx
            .debug_bounds("connection-picker-label-postgres")
            .expect("card label");
        assert!(label.origin.x >= tile.origin.x && label.right() <= tile.right());
        assert!(label.origin.y >= tile.origin.y && label.bottom() <= tile.bottom());
        let switch = cx.debug_bounds("connection-picker-view-switch").expect("view switch");
        assert_eq!(switch.size.height, px(28.0));
        assert!(switch.right() <= cx.debug_bounds("connection-picker-search").unwrap().left());
    }

    #[gpui::test]
    fn the_analytics_category_offers_clickhouse_with_its_brand_logo(cx: &mut TestAppContext) {
        cx.dispatcher.allow_parking();
        cx.update(|app| {
            gpui_component::init(app);
            app.set_reduce_motion(true);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let _ = root;
        let holder = Rc::new(RefCell::new(None));
        let holder_for_test = holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            holder_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.w(px(880.0)).title("New Connection").child(inputs.render())
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("connection-picker-clickhouse").is_none(), "Relational is shown first");

        let category = cx
            .debug_bounds(picker_category_selector(PickerCategory::Analytics))
            .expect("Analytics category");
        cx.simulate_click(category.center(), Default::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("connection-picker-clickhouse").is_some());
        assert!(cx.debug_bounds("connection-picker-logo-clickhouse").is_some());
        assert_eq!(
            connection_picker_catalog::all_entries()
                .iter()
                .find(|entry| entry.id == "clickhouse")
                .map(|entry| picker_logo_asset(*entry)),
            Some(Some("icons/clickhouse.svg"))
        );

        // Selecting the tile selects the kind, and the footer names it.
        let tile = cx.debug_bounds("connection-picker-clickhouse").unwrap();
        cx.simulate_click(tile.center(), Default::default());
        cx.run_until_parked();
        let inputs = holder.borrow().clone().expect("inputs");
        assert_eq!(inputs.kind.get(), ConnectionKind::Clickhouse);
        assert!(inputs.picker_selection_is_visible());
    }

    #[gpui::test]
    fn the_cache_category_offers_redis_and_its_form_saves_rediss_with_an_acl_user(cx: &mut TestAppContext) {
        cx.dispatcher.allow_parking();
        cx.update(|app| {
            gpui_component::init(app);
            app.set_reduce_motion(true);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let _ = root;
        let holder = Rc::new(RefCell::new(None));
        let holder_for_test = holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            holder_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.w(px(880.0)).title("New Connection").child(inputs.render())
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("connection-picker-redis").is_none(), "Relational is shown first");

        // The category, then the tile: the real Redis entry with its brand logo.
        let category = cx
            .debug_bounds(picker_category_selector(PickerCategory::DocsCacheSearch))
            .expect("Docs / cache category");
        cx.simulate_click(category.center(), Default::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("connection-picker-redis").is_some(), "Redis is offered");
        assert!(cx.debug_bounds("connection-picker-logo-redis").is_some());
        let tile = cx.debug_bounds("connection-picker-redis").unwrap();
        cx.simulate_click(tile.center(), Default::default());
        cx.run_until_parked();
        let inputs = holder.borrow().clone().expect("inputs");
        assert_eq!(inputs.kind.get(), ConnectionKind::Redis);
        assert!(inputs.picker_selection_is_visible());

        // The form: host, port, ACL user, password, db index and TLS fill the URI.
        cx.update(|window, app| {
            inputs.apply_kind_placeholders(ConnectionKind::Redis, window, app);
            for (input, text) in [
                (&inputs.name, "cache"),
                (&inputs.host, "cache.example.com"),
                (&inputs.port, "6380"),
                (&inputs.username, "app"),
                (&inputs.password, "s3cret"),
                (&inputs.database, "4"),
            ] {
                input.update(app, |state, cx| state.set_value(text, window, cx));
            }
            inputs.secure.set(true);
            inputs.use_uri.set(false);
        });
        let form = cx.read(|app| inputs.read_form(app, ConnectionFormState::new(ConnectionKind::Redis)));
        assert!(form.secure);
        form.validate().expect("the Redis form validates");
        assert_eq!(form.runtime_uri().unwrap(), "rediss://app:s3cret@cache.example.com:6380/4");
        // The password never reaches the stored URI.
        assert_eq!(form.persisted_uri().unwrap(), "rediss://app@cache.example.com:6380/4");
    }

    #[gpui::test]
    fn picker_matches_regions_and_category_selection(cx: &mut TestAppContext) {
        cx.update(|app| {
            gpui_component::init(app);
            // The dialog slides in on a wall-clock animation; bounds read
            // before it ends would miss the settled controls.
            app.set_reduce_motion(true);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("New Connection").child(inputs.render())
            });
        });
        // The dialog centers from the previous frame's size: settle two frames.
        for _ in 0..2 {
            app.update(cx, |_, cx| cx.notify());
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }

        for selector in [
            "connection-picker",
            "connection-picker-toolbar",
            "connection-picker-categories",
            "connection-picker-results",
            "connection-picker-grid",
            "connection-picker-logo-postgres",
            "connection-picker-grid-toggle",
            "connection-picker-list-toggle",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing selector: {selector}"
            );
        }

        let category = cx
            .debug_bounds("connection-picker-category-docs-cache-search")
            .expect("document category should render");
        cx.simulate_click(category.center(), Default::default());
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(
            inputs_holder
                .borrow()
                .as_ref()
                .expect("picker inputs should be retained")
                .kind
                .get(),
            ConnectionKind::Postgres
        );
        assert!(cx.debug_bounds("connection-picker-mongodb").is_some());
        assert!(!inputs_holder
            .borrow()
            .as_ref()
            .expect("picker inputs should be retained")
            .picker_selection_is_visible());
    }

    #[gpui::test]
    fn picker_switches_between_grid_and_list_views(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("New Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let list_toggle = cx
            .debug_bounds("connection-picker-list-toggle")
            .expect("list view toggle should render");
        cx.simulate_click(list_toggle.center(), Default::default());
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("connection-picker-list").is_some());
        assert_eq!(
            inputs_holder
                .borrow()
                .as_ref()
                .expect("picker inputs should be retained")
                .picker_view
                .get(),
            PickerView::List
        );
    }

    #[gpui::test]
    fn double_clicking_a_picker_tile_advances_to_config(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("New Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let tile = cx
            .debug_bounds("connection-picker-postgres")
            .expect("PostgreSQL tile should render");
        cx.simulate_event(gpui::MouseDownEvent {
            position: tile.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            position: tile.center(),
            modifiers: gpui::Modifiers::none(),
            button: gpui::MouseButton::Left,
            click_count: 2,
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(!inputs_holder
            .borrow()
            .as_ref()
            .expect("picker inputs should be retained")
            .is_picker());
    }

    #[gpui::test]
    fn config_renders_ordered_connection_fields(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        cx.update(|window, app| {
            let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
            form.id = Some(uuid::Uuid::nil());
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("Edit Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let selectors = [
            "connection-config",
            "connection-config-url",
            "connection-config-name",
            "connection-config-type",
            "connection-config-password",
            "connection-config-save-password",
        ];
        for selector in selectors {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "missing selector: {selector}"
            );
        }
        assert!(
            cx.debug_bounds("connection-config-url").unwrap().origin.y
                < cx.debug_bounds("connection-config-name").unwrap().origin.y
        );
        assert!(
            cx.debug_bounds("connection-config-name").unwrap().origin.y
                < cx.debug_bounds("connection-config-type").unwrap().origin.y
        );
    }


    /// Opens the config step for `kind` in a plain dialog and returns the inputs.
    fn open_config(
        cx: &mut TestAppContext,
        kind: ConnectionKind,
    ) -> (&mut gpui::VisualTestContext, ConnectionDialogInputs) {
        cx.update(|app| {
            gpui_component::init(app);
            crate::theme::init(app);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let _ = root;
        let holder = Rc::new(RefCell::new(None));
        let holder_for_test = holder.clone();
        cx.update(|window, app| {
            let mut form = ConnectionFormState::new(kind);
            form.id = Some(uuid::Uuid::nil());
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            holder_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.w(px(560.0)).title("Edit Connection").child(inputs.render())
            });
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let inputs = holder.borrow().clone().expect("inputs");
        (cx, inputs)
    }

    #[gpui::test]
    fn type_row_is_read_only_and_advanced_timeout_maps_to_url_params(cx: &mut TestAppContext) {
        let (cx, inputs) = open_config(cx, ConnectionKind::Postgres);
        let type_row = cx.debug_bounds("connection-config-type-control").unwrap();
        assert_eq!(type_row.size.height, px(32.0));
        cx.update(|window, app| {
            replace_input(&inputs.host, "h", window, app);
            replace_input(&inputs.name, "n", window, app);
            replace_input(&inputs.connect_timeout, "15", window, app);
        });
        let form = cx.read(|app| inputs.read_form(app, ConnectionFormState::new(ConnectionKind::Postgres)));
        assert_eq!(form.connect_timeout, "15");
        assert!(form.generated_uri().unwrap().contains("connect_timeout=15"));
    }

    #[gpui::test]
    fn clickhouse_config_offers_secure_ca_cert_and_settings_and_secure_moves_the_default_port(
        cx: &mut TestAppContext,
    ) {
        cx.dispatcher.allow_parking();
        let (cx, inputs) = open_config(cx, ConnectionKind::Clickhouse);
        for selector in [
            "connection-config-host",
            "connection-config-username",
            "connection-config-password",
            "connection-config-database",
            "connection-config-secure",
            "connection-config-ca-cert",
            "connection-config-uri-options",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "missing selector: {selector}");
        }
        // ClickHouse has no mode or client certificate: TLS is on the Connection tab, no TLS tab.
        assert!(cx.debug_bounds("connection-config-tab-tls").is_none());
        assert!(cx.debug_bounds("connection-config-tab-tunnel").is_some());

        let click_secure = |cx: &mut gpui::VisualTestContext| {
            // A freshly opened dialog centers itself from the previous frame.
            for _ in 0..2 {
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
            }
            let row = cx.debug_bounds("connection-config-secure").unwrap();
            // On the checkbox's own label, right of the row label.
            let checkbox = gpui::point(
                row.origin.x + px(LABEL_WIDTH + 12.0 + 40.0),
                row.origin.y + row.size.height / 2.0,
            );
            cx.simulate_click(checkbox, Default::default());
            cx.run_until_parked();
        };
        cx.update(|window, app| replace_input(&inputs.port, "8123", window, app));
        click_secure(cx);
        assert!(inputs.secure.get());
        assert_eq!(cx.read(|app| inputs.port.read(app).value().to_string()), "8443");
        click_secure(cx);
        assert!(!inputs.secure.get());
        assert_eq!(cx.read(|app| inputs.port.read(app).value().to_string()), "8123");
        // A custom port is the user's: the toggle leaves it alone.
        cx.update(|window, app| replace_input(&inputs.port, "9440", window, app));
        click_secure(cx);
        assert_eq!(cx.read(|app| inputs.port.read(app).value().to_string()), "9440");

        cx.update(|window, app| {
            replace_input(&inputs.host, "ch.example.com", window, app);
            replace_input(&inputs.name, "n", window, app);
            replace_input(&inputs.ca_cert, "/etc/ca.pem", window, app);
            replace_input(&inputs.uri_options, "max_execution_time=30", window, app);
        });
        let form = cx.read(|app| inputs.read_form(app, ConnectionFormState::new(ConnectionKind::Clickhouse)));
        assert!(form.secure);
        assert_eq!(
            form.generated_uri().unwrap(),
            "clickhouse://ch.example.com:9440?max_execution_time=30&secure=true&ca_cert=%2Fetc%2Fca.pem"
        );

        // Parsing a URL into the fields fills Secure and the certificate too.
        cx.update(|window, app| {
            replace_input(
                &inputs.uri,
                "clickhouse://u@h2:8443/db?secure=false&ca_cert=%2Ftmp%2Fother.pem&async_insert=1",
                window,
                app,
            );
            inputs.apply_uri_to_fields(window, app).unwrap();
        });
        assert!(!inputs.secure.get());
        assert_eq!(cx.read(|app| inputs.ca_cert.read(app).value().to_string()), "/tmp/other.pem");
        assert_eq!(cx.read(|app| inputs.uri_options.read(app).value().to_string()), "async_insert=1");
    }

    #[gpui::test]
    fn applying_a_url_fills_the_tls_tab_and_keeps_the_url_params_free_of_tls(cx: &mut TestAppContext) {
        use crate::connection_tls::TlsMode;
        cx.dispatcher.allow_parking();
        let (cx, inputs) = open_config(cx, ConnectionKind::Postgres);
        cx.update(|window, app| {
            replace_input(
                &inputs.uri,
                "postgresql://u@h/db?application_name=x&sslmode=verify-ca&sslrootcert=%2Fetc%2Fca.pem",
                window,
                app,
            );
            inputs.apply_uri_to_fields(window, app).unwrap();
        });
        let tls = cx.read(|app| inputs.tls.read(app));
        assert_eq!((tls.mode, tls.ca_file.as_str()), (TlsMode::VerifyCa, "/etc/ca.pem"));
        assert_eq!(cx.read(|app| inputs.uri_options.read(app).value().to_string()), "application_name=x");
        assert_eq!(cx.read(|app| inputs.tls.shown_for_test(app)), (Some("verify-ca".into()), None));
        // A URL without TLS parameters resets the tab.
        cx.update(|window, app| {
            replace_input(&inputs.uri, "postgresql://u@h/db", window, app);
            inputs.apply_uri_to_fields(window, app).unwrap();
        });
        assert_eq!(cx.read(|app| inputs.tls.read(app)).mode, TlsMode::Default);
        assert_eq!(cx.read(|app| inputs.tls.shown_for_test(app)).0.as_deref(), Some("Default (prefer)"));
    }

    #[gpui::test]
    fn config_type_control_returns_to_picker(cx: &mut TestAppContext) {
        cx.update(|app| {
            gpui_component::init(app);
            app.set_reduce_motion(true);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let mut form = ConnectionFormState::new(ConnectionKind::Postgres);
            form.id = Some(uuid::Uuid::nil());
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("Edit Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let type_control = cx
            .debug_bounds("connection-config-type-control")
            .expect("type selector should render");
        cx.simulate_click(type_control.center(), Default::default());
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(inputs_holder
            .borrow()
            .as_ref()
            .expect("config inputs should be retained")
            .is_picker());
        assert!(cx.debug_bounds("connection-picker-postgres").is_some());
    }

    #[gpui::test]
    fn sqlite_config_only_renders_file_connection_controls(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        cx.update(|window, app| {
            let mut form = ConnectionFormState::new(ConnectionKind::Sqlite);
            form.id = Some(uuid::Uuid::nil());
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("Edit Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("connection-config-file").is_some());
        assert!(cx.debug_bounds("connection-config-password").is_none());
        assert!(cx.debug_bounds("connection-config-save-password").is_none());
        assert!(cx.debug_bounds("connection-config-username").is_none());
    }

    #[test]
    fn mongodb_cluster_url_is_preserved_when_switching_to_url_mode() {
        let form = ConnectionFormState {
            kind: ConnectionKind::Mongodb,
            uri: "mongodb+srv://cluster.example.com/app".to_string(),
            host: "old-host.example.com".to_string(),
            database: "old-app".to_string(),
            ..ConnectionFormState::new(ConnectionKind::Mongodb)
        };

        assert_eq!(uri_for_url_mode(&form).unwrap(), form.uri);

        let uppercase_form = ConnectionFormState {
            uri: "MONGODB+SRV://cluster.example.com/app".to_string(),
            ..form.clone()
        };
        assert_eq!(
            uri_for_url_mode(&uppercase_form).unwrap(),
            "mongodb+srv://cluster.example.com/app"
        );

        let seed_list_form = ConnectionFormState {
            uri: "mongodb://cluster-a:27017,cluster-b:27017/app".to_string(),
            ..form
        };
        assert_eq!(
            uri_for_url_mode(&seed_list_form).unwrap(),
            seed_list_form.uri
        );
        assert!(!is_mongodb_cluster_uri("postgres://host-a,host-b/app"));
    }

    #[gpui::test]
    fn switching_from_url_to_fields_preserves_password_without_uri_password(
        cx: &mut TestAppContext,
    ) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState {
                name: "Password preservation".to_string(),
                kind: ConnectionKind::Postgres,
                uri: "postgresql://alice@localhost/app".to_string(),
                password: "p@ss:word".to_string(),
                use_uri: true,
                ..Default::default()
            };
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            inputs.set_uri_mode(false, window, app).unwrap();
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("Edit Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());

        let password = cx.cx.read(|app| {
            inputs_holder
                .borrow()
                .as_ref()
                .expect("inputs should be retained")
                .password
                .read(app)
                .value()
                .to_string()
        });
        assert_eq!(password, "p@ss:word");
    }

    #[gpui::test]
    fn picker_search_filters_database_buttons(cx: &mut TestAppContext) {
        cx.update(|app| {
                gpui_component::init(app);
                // Dialog slide-in is a wall-clock animation; end it immediately.
                app.set_reduce_motion(true);
            });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(crate::app::AppRoot::new_for_test);
            gpui_component::Root::new(view, window, cx)
        });
        let app = cx.cx.read(|app| {
            root.read(app)
                .view()
                .clone()
                .downcast::<crate::app::AppRoot>()
                .expect("root should contain AppRoot")
        });
        let inputs_holder = Rc::new(RefCell::new(None));
        let inputs_for_test = inputs_holder.clone();
        cx.update(|window, app| {
            let form = ConnectionFormState::new(ConnectionKind::Postgres);
            let inputs = ConnectionDialogInputs::new(window, app, &form, &Arc::new(Settings::default()));
            inputs_for_test.borrow_mut().replace(inputs.clone());
            window.open_dialog(app, move |dialog, _, _| {
                dialog.title("New Connection").child(inputs.render())
            });
        });
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let search = cx
            .debug_bounds("connection-picker-search")
            .expect("picker search should render");
        cx.simulate_click(search.center(), Default::default());
        cx.simulate_keystrokes("Mongo");
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let search_value = inputs_holder
            .borrow()
            .as_ref()
            .expect("picker inputs should be retained")
            .picker_search
            .clone();
        assert_eq!(
            cx.cx.read(|app| search_value.read(app).value().to_string()),
            "mongo"
        );
        assert_eq!(
            inputs_holder
                .borrow()
                .as_ref()
                .unwrap()
                .picker_filter
                .borrow()
                .as_str(),
            "mongo"
        );
        assert!(!inputs_holder
            .borrow()
            .as_ref()
            .unwrap()
            .picker_selection_is_visible());
        let mongodb = cx
            .debug_bounds("connection-picker-mongodb")
            .expect("MongoDB should match the search");
        cx.simulate_click(mongodb.center(), Default::default());
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(inputs_holder
            .borrow()
            .as_ref()
            .unwrap()
            .picker_selection_is_visible());

        cx.simulate_keystrokes("secondary-a");
        cx.simulate_keystrokes("missing");
        app.update(cx, |_, cx| cx.notify());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("connection-picker-empty").is_some());
        assert!(!inputs_holder
            .borrow()
            .as_ref()
            .unwrap()
            .picker_selection_is_visible());
    }
}
