//! SSH tunnels in the app: the state of the live ones, the dialogs ("Trust this host?", the
//! changed-key error, the password and one-time-code prompts), the dialog's "Test tunnel"
//! button and the "tunnel is down" detection that feeds the existing reconnect banner.
//!
//! One tunnel per database session: the main session's lives in [`TunnelUi::live`] and is dropped
//! (after the driver session is disconnected) with it; the diagram's apply session opens and
//! drops its own. Credentials of an unsaved password or passphrase stay in [`TunnelUi::secrets`]
//! until the connection is disconnected, so the extra sessions and reconnects do not ask again.
//! Nothing here polls: a dead tunnel is noticed when a request on its connection fails.

use super::connection_ui::runtime_uri_for_form;
use super::*;
use crate::connection_dialog::TestStatus;
use crate::error_log::{Report, Severity};
use crate::ssh_tunnel::{
    self, ChannelPrompts, ConnectFailure, HostKeyProblem, HostKeyRequest, KiPrompt, SecretHop,
    SecretNeed, SecretRequest, SshSecrets, SshSettings, TunnelAccess, TunnelOpener,
};
use engine_connect::connect::{accounts_of, Connector, HostKeyAnswer, OpenRequest, TunnelPrompts};
use engine_connect::records::RuntimeUri;
use engine_net::{KeyboardInteractiveRequest, Secret};
use gpui::{Entity, SharedString};
use gpui_component::dialog::DialogFooter;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// The tunnel of a connection's main session and what opened it (what the connection's other
/// sessions, the diagram's apply session, open their own with).
pub(super) use engine_connect::connect::LiveTunnel;

/// A callback run with the window it was triggered in.
type WindowCallback = Rc<dyn Fn(&mut Window, &mut gpui::App)>;
/// A step run on the app root (starts a connection test again once a question is answered).
pub(super) type RootCallback = Rc<dyn Fn(&mut AppRoot, &mut Context<AppRoot>)>;

/// The app's answers to a connect's questions. The app asks with dialogs and parks the attempt,
/// so it answers "not now" to a host key and to a missing secret (the connect then fails with the
/// typed [`ConnectFailure`] the dialogs act on); only the bastion's keyboard-interactive questions
/// are asked while the connect waits, through the dialog channel.
pub(super) struct AppPrompts {
    sender: smol::channel::Sender<KiPrompt>,
}

impl TunnelPrompts for AppPrompts {
    fn trust_host(&self, _request: &HostKeyRequest) -> HostKeyAnswer {
        HostKeyAnswer::Refuse
    }

    fn secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }

    fn keyboard_interactive(
        &self,
        user: &str,
        request: &KeyboardInteractiveRequest,
    ) -> Option<Vec<Secret>> {
        use engine_net::Prompts as _;
        ChannelPrompts::new(self.sender.clone(), user.to_owned()).keyboard_interactive(request)
    }
}

pub(super) struct TunnelUi {
    pub(super) opener: Arc<dyn TunnelOpener>,
    /// `None` when the app data directory is unknown: tunnels then fail with a typed error.
    pub(super) known_hosts: Option<PathBuf>,
    prompts: smol::channel::Sender<KiPrompt>,
    /// Settings that win over the stored record (UI scenarios and unit tests have no storage).
    pub(super) settings_override: HashMap<String, SshSettings>,
    /// Secrets of the connections that are open, memory only.
    pub(super) secrets: HashMap<String, SshSecrets>,
    pub(super) live: HashMap<String, LiveTunnel>,
    /// The agent's view of the saved connections (its own driver sessions and tunnels). No agent
    /// session exists in the app yet; the connection save / delete paths already revoke through it.
    pub(super) agent: Option<Arc<engine_connect::connect::AgentConnections>>,
    /// Drops every tunnel (and wipes the secrets) when the app quits.
    pub(super) _quit: Option<gpui::Subscription>,
    /// Which dialogs were opened, oldest first (tests).
    #[cfg(test)]
    pub(super) opened_dialogs: Vec<&'static str>,
}

impl TunnelUi {
    pub(super) fn new(scenario: bool) -> (Self, smol::channel::Receiver<KiPrompt>) {
        let (prompts, receiver) = smol::channel::unbounded();
        let opener: Arc<dyn TunnelOpener> = if scenario || cfg!(test) {
            #[cfg(any(test, feature = "ui-scenarios"))]
            {
                ssh_tunnel::fake::scenario(
                    std::env::var("TABLEX_SEED_TUNNEL")
                        .unwrap_or_default()
                        .as_str(),
                )
            }
            #[cfg(not(any(test, feature = "ui-scenarios")))]
            {
                Arc::new(ssh_tunnel::EngineOpener)
            }
        } else {
            Arc::new(ssh_tunnel::EngineOpener)
        };
        let known_hosts = if scenario || cfg!(test) {
            // Never the user's own app data.
            Some(std::env::temp_dir().join(format!("tablex-known_hosts-{}", uuid::Uuid::new_v4())))
        } else {
            crate::platform::ssh_known_hosts_path()
        };
        (
            Self {
                opener,
                known_hosts,
                prompts,
                settings_override: HashMap::new(),
                secrets: HashMap::new(),
                live: HashMap::new(),
                agent: None,
                _quit: None,
                #[cfg(test)]
                opened_dialogs: Vec::new(),
            },
            receiver,
        )
    }

    /// Builds the agent's connections over the app's driver store, tunnel opener and prompts.
    pub(super) fn attach_agent(&mut self, store: Arc<engine_driver_host::InstalledDriverStore>) {
        let connector =
            Connector::new(store).with_tunnels(Arc::clone(&self.opener), self.known_hosts.clone());
        let prompts = Arc::new(AppPrompts {
            sender: self.prompts.clone(),
        });
        self.agent = Some(Arc::new(engine_connect::connect::AgentConnections::new(
            connector, prompts,
        )));
    }

    /// Forgets a connection's tunnel and secrets. The tunnel is returned: drop it after the
    /// driver session is closed.
    pub(super) fn release(&mut self, connection: &str, keep_secrets: bool) -> Option<LiveTunnel> {
        if !keep_secrets {
            self.secrets.remove(connection);
        }
        self.live.remove(connection)
    }
}

/// Capture seed (`TABLEX_SEED_TUNNEL=…,connected`): every fixture connection behind a bastion.
#[cfg(feature = "ui-scenarios")]
impl AppRoot {
    pub(super) fn seed_connected_tunnels(&mut self) {
        let wants = std::env::var("TABLEX_SEED_TUNNEL")
            .is_ok_and(|seed| seed.split(',').any(|token| token == "connected"));
        if !wants {
            return;
        }
        let mut settings = SshSettings {
            enabled: true,
            host: "bastion.example.com".into(),
            user: "deploy".into(),
            ..SshSettings::default()
        };
        if std::env::var("TABLEX_SEED_TUNNEL").is_ok_and(|seed| seed.split(',').any(|token| token == "chain")) {
            use crate::ssh_tunnel::{JumpSettings, ProxySettings};
            settings.jumps = vec![
                JumpSettings { host: "gateway.example.com".into(), user: "ops".into(), ..JumpSettings::default() },
                JumpSettings { host: "jump.internal.example.com".into(), user: "hop".into(), ..JumpSettings::default() },
            ];
            settings.proxy = ProxySettings {
                enabled: true,
                host: "proxy.corp.example.com".into(),
                ..ProxySettings::default()
            };
        }
        // A file has no network layer: SQLite is never behind a bastion.
        let ids: Vec<String> = self
            .saved_connections
            .iter()
            .filter(|saved| saved.kind != ConnectionKind::Sqlite)
            .map(|saved| saved.id.to_string())
            .collect();
        for id in ids {
            self.seed_tunnel(id, &settings);
        }
    }

}

/// Scenarios and tests: a connection behind a fake bastion.
#[cfg(any(test, feature = "ui-scenarios"))]
impl AppRoot {
    /// `connection` behind the fake bastion `settings` names.
    pub(super) fn seed_tunnel(&mut self, connection: String, settings: &SshSettings) {
        use crate::ssh_tunnel::SessionTunnel;
        let handle: Arc<dyn ssh_tunnel::TunnelHandle> = Arc::new(ssh_tunnel::fake::FakeTunnel {
            via: "deploy@bastion.example.com:22".into(),
            addrs: Vec::new(),
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            probe_error: None,
            drops: None,
            drop_delay_ms: None,
        });
        let tunnel = SessionTunnel {
            handle,
            targets: Vec::new(),
            interactive: false,
        };
        let access = self.connector().tunnel_access(
            settings.clone(),
            SshSecrets::default(),
            &self.tunnel_prompts(),
        );
        self.tunnel_ui
            .live
            .insert(connection, LiveTunnel { tunnel, access });
    }
}

/// Listens for the bastion's questions for the lifetime of the app, and closes every tunnel
/// when the app quits (their SSH connections end with them, not with the process table).
pub(super) fn listen_for_prompts(
    receiver: smol::channel::Receiver<KiPrompt>,
    cx: &mut Context<AppRoot>,
) -> gpui::Subscription {
    let quit = cx.on_app_quit(|this, _| {
        // Closing a tunnel can wait on the network: never on the UI thread.
        let live = std::mem::take(&mut this.tunnel_ui.live);
        this.tunnel_ui.secrets.clear();
        async move {
            // One worker closes every tunnel so the SSH goodbye usually goes out, and the quit
            // waits for it at most this long in total, however many tunnels there are.
            let (done, finished) = smol::channel::bounded::<()>(1);
            std::thread::spawn(move || {
                drop(live);
                let _ = done.send_blocking(());
            });
            smol::future::or(
                async {
                    let _ = finished.recv().await;
                },
                async {
                    smol::Timer::after(Duration::from_millis(500)).await;
                },
            )
            .await;
        }
    });
    cx.spawn(async move |this, cx| {
        while let Ok(prompt) = receiver.recv().await {
            if this
                .update(cx, |this, cx| this.show_keyboard_interactive(prompt, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
    quit
}

/// A modal with a decision: the primary button, an optional other one, and Escape = cancel.
struct Decision {
    title: String,
    width: f32,
    body: Rc<dyn Fn() -> AnyElement>,
    /// `(label, selector)`; `None` for an error that can only be closed.
    ok: Option<(&'static str, &'static str)>,
    cancel: (&'static str, &'static str),
    on_ok: WindowCallback,
    on_cancel: WindowCallback,
    /// What Enter does: submit like the primary button, or (for the trust question) decline.
    /// Escape always cancels.
    enter_accepts: bool,
    /// Runs once the decision is made, whichever it was (empties the dialog's inputs).
    on_done: WindowCallback,
}

fn open_decision(window: &mut Window, cx: &mut gpui::App, decision: Decision) {
    let decided = Rc::new(Cell::new(false));
    let decision = Rc::new(decision);
    window.open_dialog(cx, move |dialog, _, _| {
        let finish = {
            let (decided, decision) = (decided.clone(), decision.clone());
            Rc::new(
                move |accept: bool, window: &mut Window, cx: &mut gpui::App| {
                    if decided.replace(true) {
                        return;
                    }
                    if accept {
                        (decision.on_ok)(window, cx);
                    } else {
                        (decision.on_cancel)(window, cx);
                    }
                    (decision.on_done)(window, cx);
                },
            )
        };
        let mut footer = DialogFooter::new();
        let (cancel_label, cancel_selector) = decision.cancel;
        footer = footer.child({
            let finish = finish.clone();
            Button::new(cancel_selector)
                .debug_selector(move || cancel_selector.into())
                .label(cancel_label)
                .on_click(move |_, window, cx| {
                    finish(false, window, cx);
                    window.close_dialog(cx);
                })
        });
        if let Some((label, selector)) = decision.ok {
            footer = footer.child({
                let finish = finish.clone();
                Button::new(selector)
                    .debug_selector(move || selector.into())
                    .label(label)
                    .primary()
                    .on_click(move |_, window, cx| {
                        finish(true, window, cx);
                        window.close_dialog(cx);
                    })
            });
        }
        dialog
            .w(px(decision.width))
            .title(decision.title.clone())
            .overlay_closable(false)
            // Enter is gpui's default confirm, which would only close the dialog and strand the
            // parked attempt: it decides like one of the buttons instead.
            .on_ok({
                let finish = finish.clone();
                let accepts = decision.enter_accepts;
                move |_, window, cx| {
                    finish(accepts, window, cx);
                    true
                }
            })
            .on_cancel({
                let finish = finish.clone();
                move |_, window, cx| {
                    finish(false, window, cx);
                    true
                }
            })
            .child((decision.body)())
            .footer(footer)
    });
}

fn fact_row(label: &'static str, value: String, selector: &'static str) -> AnyElement {
    // Key types and recorded locations come from the server and from files: shown as plain text.
    let value = ssh_tunnel::plain_text(&value, 160);
    div()
        .flex()
        .gap(px(10.0))
        .child(
            div()
                .w(px(96.0))
                .flex_shrink_0()
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(label),
        )
        .child(
            div()
                .debug_selector(move || selector.into())
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(12.0))
                .text_color(theme::text())
                .font_family("Menlo")
                .child(value),
        )
        .into_any_element()
}

fn paragraph(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text.into())
        .into_any_element()
}

fn clear_input(input: &Entity<InputState>, window: &mut Window, cx: &mut gpui::App) {
    input.update(cx, |state, cx| state.set_value("", window, cx));
}

/// Server-supplied text in a bordered box, so it reads as quoted.
fn quoted_box(text: String, selector: String) -> AnyElement {
    div()
        .debug_selector(move || selector.clone())
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

/// What the "Trust this host?" dialog says: for the SSH server (the bastion or a jump host) at
/// exactly the `host:port` of the key it was shown.
pub(super) struct TrustTexts {
    pub(super) intro: String,
    /// `(label, value, selector)`.
    pub(super) rows: Vec<(&'static str, String, &'static str)>,
    pub(super) outro: String,
}

pub(super) fn trust_texts(request: &HostKeyRequest, known_hosts: &str) -> TrustTexts {
    TrustTexts {
        intro: format!(
            "TableX has not connected to this SSH server before, so it cannot tell whether {} really is the server you expect.",
            host_port_always(&request.host, request.port)
        ),
        rows: vec![
            ("Host", request.host.clone(), "ssh-trust-host"),
            ("Port", request.port.to_string(), "ssh-trust-port"),
            ("Login", request.user.clone(), "ssh-trust-user"),
            ("Key type", request.algorithm.clone(), "ssh-trust-algorithm"),
            ("SHA-256", request.fingerprint.clone(), "ssh-trust-fingerprint"),
        ],
        outro: format!(
            "Compare the fingerprint with the one your administrator gave you. If you trust it, the key is recorded in {known_hosts} and checked on every connection; your ~/.ssh/known_hosts is never changed."
        ),
    }
}

/// The title is fixed app text and the first line always names the server and login; the
/// server's name, instructions and questions are quoted below it (capped, control characters
/// stripped, newlines collapsed).
pub(super) const KI_TITLE: &str = "SSH server asks";
const SERVER_TEXT_CAP: usize = 200;

pub(super) struct KiTexts {
    pub(super) title: &'static str,
    pub(super) host_line: String,
    /// The quoted name and instructions, when the server sent any.
    pub(super) server_text: Vec<String>,
    /// One quoted question per input.
    pub(super) prompts: Vec<String>,
}

pub(super) fn ki_texts(prompt: &KiPrompt) -> KiTexts {
    let quote = |text: &str| {
        format!(
            "\u{201c}{}\u{201d}",
            ssh_tunnel::plain_text(text, SERVER_TEXT_CAP)
        )
    };
    KiTexts {
        title: KI_TITLE,
        host_line: format!(
            "{}@{} asks:",
            ssh_tunnel::plain_text(&prompt.user, 64),
            host_port_always(&ssh_tunnel::plain_text(&prompt.host, 255), prompt.port)
        ),
        server_text: [&prompt.name, &prompt.instructions]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .map(|text| quote(text))
            .collect(),
        prompts: prompt
            .prompts
            .iter()
            .map(|item| quote(&item.text))
            .collect(),
    }
}

fn host_port_always(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

fn column(children: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .children(children)
        .into_any_element()
}

fn host_port(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

/// The status text of a route (see [`AppRoot::via_ssh`]). `via` is the tunnel's own name for itself
/// (`user@bastion:port` for SSH).
pub(super) fn route_label(
    settings: &SshSettings,
    via: &str,
    target: Option<&engine_net::Target>,
    shared: bool,
) -> String {
    if !settings.enabled {
        return format!("via {}", settings.proxy.describe());
    }
    let mut label = format!("via SSH {}", ssh_label(via));
    match settings.jumps.len() {
        0 => {}
        1 => label.push_str(" (1 jump)"),
        count => label.push_str(&format!(" ({count} jumps)")),
    }
    if settings.proxy.enabled {
        label.push_str(&format!(" through {}", settings.proxy.describe()));
    }
    // The bastion reaches the database under another name or port than the URI says.
    if !settings.remote_host.is_empty() || !settings.remote_port.is_empty() {
        if let Some(target) = target {
            label.push_str(&format!(" (forwards to {}:{})", target.host, target.port));
        }
    }
    if shared {
        label.push_str(" (one tunnel shared by all sessions)");
    }
    label
}

/// `user@host:22` -> `user@host`.
pub(super) fn ssh_label(via: &str) -> String {
    via.strip_suffix(":22").unwrap_or(via).to_owned()
}

impl AppRoot {
    /// Runs `f` with the window hosting this root (dialogs from a background completion).
    pub(super) fn with_ui_window(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Window, &mut gpui::App) + 'static,
    ) -> bool {
        let Some(window) = self.driver_runtime.window else {
            return false;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| f(window, cx));
        });
        true
    }

    // --- opening ------------------------------------------------------------------------------

    /// The connector of this app: the driver store, tunnel opener and `known_hosts` it started with.
    pub(super) fn connector(&self) -> Connector {
        Connector::new(Arc::clone(&self.driver_ui.store)).with_tunnels(
            Arc::clone(&self.tunnel_ui.opener),
            self.tunnel_ui.known_hosts.clone(),
        )
    }

    /// A connection was edited or deleted (or its agent access may have changed): whatever an agent
    /// (the assistant) has open on it ends. The tool layer disconnects the session and cancels a
    /// statement that runs, then the agent connections drop the tunnel; both run off the UI thread
    /// (a cancel is a network round trip). The layer also notices an edit or a lowered level on
    /// its next call, through the connection's revision and effective access.
    pub(super) fn revoke_agent_access(&self, connection: &str) {
        self.assistant.revoke(connection);
    }

    /// The answers the app gives while a connect runs (see [`AppPrompts`]).
    pub(super) fn tunnel_prompts(&self) -> Arc<dyn TunnelPrompts> {
        Arc::new(AppPrompts {
            sender: self.tunnel_ui.prompts.clone(),
        })
    }

    /// The connect of `saved`: its record decides whether a tunnel is required (never an id lookup
    /// in the app's own list); typed secrets and the settings override of tests / scenarios ride
    /// along. The tunnel itself is resolved off the UI thread, inside the connector.
    pub(super) fn connect_request(
        &self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        connect_timeout_secs: u32,
    ) -> OpenRequest {
        let key = saved.id.to_string();
        let secrets = self
            .tunnel_ui
            .secrets
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let stored = self.tunnel_ui.settings_override.get(&key).cloned();
        OpenRequest::saved(saved, uri_override, connect_timeout_secs, secrets)
            .with_stored_ssh(stored)
    }

    /// The tunnel tab of the dialog's form: what is typed wins over what is saved. `None`: the tab
    /// is off.
    pub(super) fn form_tunnel(
        &self,
        form: &ConnectionFormState,
        original: Option<&engine_storage::connection::SavedConnectionRecord>,
    ) -> Option<(SshSettings, SshSecrets, Vec<String>)> {
        let settings = form.ssh.clone().filter(SshSettings::routed)?;
        let connection = original
            .map(|record| record.id.clone())
            .or_else(|| form.id.map(|id| id.to_string()))
            .unwrap_or_default();
        // A secret saved for an old hop is not offered to a new one: when any hop of the route
        // moved, none of the saved secrets are used for this test.
        // Settings that cannot be read say nothing about whom a saved secret was entered for.
        let moved = original.is_some_and(|record| {
            SshSettings::parse(&record.network_settings_json)
                .map_or(true, |old| SshSettings::route_changed(&old, &settings))
        });
        let accounts = if connection.is_empty() || moved {
            Vec::new()
        } else {
            accounts_of(&connection)
        };
        let secrets = form.ssh_secrets.secrets(&settings);
        Some((settings, secrets, accounts))
    }

    /// The dialog's Test for `form`, with the URI it was typed or resolved to.
    pub(super) fn form_request(
        &self,
        form: &ConnectionFormState,
        original: Option<&engine_storage::connection::SavedConnectionRecord>,
        uri: RuntimeUri,
    ) -> OpenRequest {
        let (ssh, secrets, accounts) = match self.form_tunnel(form, original) {
            Some((settings, secrets, accounts)) => (Some(settings), secrets, accounts),
            None => (None, SshSecrets::default(), Vec::new()),
        };
        OpenRequest::form(form.kind, uri, ssh, secrets, accounts)
    }

    /// The access the connection's other sessions open their own tunnel with. A bastion that
    /// asked questions shares the main session's tunnel instead.
    pub(super) fn secondary_tunnel_access(&self, connection: &str) -> Option<TunnelAccess> {
        let live = self.tunnel_ui.live.get(connection)?;
        // The secrets this tunnel was opened with, for the settings it was opened with. Never the
        // ones in `TunnelUi::secrets`: after a Save on a connected connection those belong to the
        // new route (and are keyed by hop position), while this access still has the old one.
        let mut access = live.access.clone();
        access.shared = live.tunnel.interactive.then(|| live.tunnel.clone());
        Some(access)
    }

    // --- status -------------------------------------------------------------------------------

    /// What the connection goes through, for the status bar, tabs and tooltips:
    /// `via SSH user@bastion (2 jumps) through SOCKS5 proxy host:port`, `via SOCKS5 proxy host:port`.
    pub(super) fn via_ssh(&self, connection: &str) -> Option<String> {
        let live = self.tunnel_ui.live.get(connection)?;
        Some(route_label(
            &live.access.settings,
            &live.tunnel.via(),
            live.tunnel.targets.first(),
            live.tunnel.interactive,
        ))
    }

    /// The sidebar badge: `SSH` when an SSH tunnel is involved, else `Proxy`.
    pub(super) fn route_badge(&self, connection: &str) -> &'static str {
        match self.tunnel_ui.live.get(connection) {
            Some(live) if !live.access.settings.enabled => "Proxy",
            _ => "SSH",
        }
    }

    // --- a dead tunnel ------------------------------------------------------------------------

    /// The tunnel of `connection` ended: its stable code and the text.
    pub(super) fn tunnel_failure(&self, connection: &str) -> Option<(&'static str, String)> {
        self.tunnel_ui.live.get(connection)?.tunnel.failure()
    }

    /// Flips the connection to the reconnect state. A dead SSH tunnel names itself in the banner
    /// and in the Error Log (with its `tunnel.*` code); the reconnect opens a fresh tunnel, never
    /// reusing a local port of the dead one.
    pub(super) fn mark_connection_lost(&mut self, connection: &str) -> bool {
        let tunnel = self.tunnel_failure(connection);
        let reason = match &tunnel {
            Some((code, message)) => format!(
                "{} (the SSH tunnel is down: {message} [{code}])",
                crate::sessions::CONNECTION_LOST_REASON
            ),
            None => crate::sessions::CONNECTION_LOST_REASON.to_string(),
        };
        if !self.sessions.mark_lost_because(connection, reason.clone()) {
            return false;
        }
        if let Some((code, _)) = tunnel {
            let mut report = Report::new(Severity::Error, Area::Connect, reason);
            report.code = Some(code.to_string());
            report.retryable = true;
            self.report_event(Some(connection), report);
        }
        true
    }

    // --- connect: dialogs ---------------------------------------------------------------------

    fn known_hosts_label(&self) -> String {
        self.tunnel_ui.known_hosts.as_ref().map_or_else(
            || "(no app data directory)".to_string(),
            |path| path.display().to_string(),
        )
    }

    fn attempt_is_current(&self, connection: &str, attempt: u64) -> bool {
        self.sessions.generation(connection) == Some(attempt)
            && self.sessions.status(connection) == ConnectionStatus::Connecting
    }

    /// A connect attempt ended in `failure`. Questions the user can answer park the attempt
    /// (the connection stays `Connecting`); everything else fails it.
    pub(super) fn handle_connect_failure(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        failure: ConnectFailure,
        cx: &mut Context<Self>,
    ) {
        let key = saved.id.to_string();
        if !self.attempt_is_current(&key, attempt) {
            self.fail_driver_connect(&key, attempt, failure, cx);
            return;
        }
        match failure {
            ConnectFailure::HostKeyUnknown(request) => {
                let root = cx.entity().downgrade();
                let (trusted_saved, trusted_uri) = (saved.clone(), uri_override.clone());
                let (declined_key, declined_request) = (key.clone(), request.clone());
                let parked = self.ask_to_trust(
                    request.clone(),
                    {
                        let root = root.clone();
                        move |_, cx| {
                            let (saved, uri, request) =
                                (trusted_saved.clone(), trusted_uri.clone(), request.clone());
                            let _ = root.update(cx, |this, cx| {
                                this.trust_then_connect(saved, uri, attempt, request, cx)
                            });
                        }
                    },
                    move |_, cx| {
                        let key = declined_key.clone();
                        let message = format!(
                            "The SSH host key of {} was not trusted",
                            host_port(&declined_request.host, declined_request.port)
                        );
                        let _ = root.update(cx, |this, cx| {
                            this.fail_driver_connect(
                                &key,
                                attempt,
                                ConnectFailure::Message(message),
                                cx,
                            )
                        });
                    },
                    cx,
                );
                if !parked {
                    self.fail_driver_connect(
                        &key,
                        attempt,
                        ConnectFailure::Message(
                            "The SSH host key is not trusted yet and there is no window to ask in"
                                .into(),
                        ),
                        cx,
                    );
                }
            }
            ConnectFailure::SecretNeeded(request) => {
                let parked = self.ask_for_secret(saved.clone(), uri_override, attempt, request, cx);
                if !parked {
                    self.fail_driver_connect(
                        &key,
                        attempt,
                        ConnectFailure::Message(
                            "An SSH secret is needed and there is no window to ask in".into(),
                        ),
                        cx,
                    );
                }
            }
            other => self.fail_driver_connect(&key, attempt, other, cx),
        }
    }

    #[allow(clippy::result_large_err)] // the closure's `Err` is engine-connect's `ConnectFailure`; boxing its variants touches every consumer
    /// Records the key (app known_hosts only, off the UI thread), then connects again.
    fn trust_then_connect(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        request: HostKeyRequest,
        cx: &mut Context<Self>,
    ) {
        let key = saved.id.to_string();
        let opener = Arc::clone(&self.tunnel_ui.opener);
        let known_hosts = match ssh_tunnel::no_known_hosts(self.tunnel_ui.known_hosts.as_deref()) {
            Ok(path) => path.to_path_buf(),
            Err(failure) => {
                self.fail_driver_connect(&key, attempt, failure, cx);
                return;
            }
        };
        let spec = ssh_tunnel::trust_spec(&request, &known_hosts);
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                opener.trust_host_key(&spec, &request.host, request.port, &request.public_key)
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) if this.attempt_is_current(&key, attempt) => {
                    this.run_driver_connect(saved, uri_override, attempt, cx)
                }
                Ok(()) => {}
                Err(error) => {
                    let failure = ssh_tunnel::classify(error, &SshSettings::default());
                    this.fail_driver_connect(&key, attempt, failure, cx)
                }
            });
        })
        .detach();
    }

    /// The "Trust this host?" dialog. `false` when there is no window to show it in.
    pub(super) fn ask_to_trust(
        &mut self,
        request: HostKeyRequest,
        on_trust: impl Fn(&mut Window, &mut gpui::App) + 'static,
        on_cancel: impl Fn(&mut Window, &mut gpui::App) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("trust-host");
        let known_hosts = self.known_hosts_label();
        self.with_ui_window(cx, move |window, cx| {
            let body_request = request.clone();
            open_decision(
                window,
                cx,
                Decision {
                    title: "Trust this host?".into(),
                    width: 540.0,
                    body: Rc::new(move || {
                        let texts = trust_texts(&body_request, &known_hosts);
                        let mut children = vec![paragraph(texts.intro)];
                        for (label, value, selector) in texts.rows {
                            children.push(fact_row(label, value, selector));
                        }
                        children.push(paragraph(texts.outro));
                        column(children)
                    }),
                    ok: Some(("Trust and continue", "ssh-trust-accept")),
                    cancel: ("Cancel", "ssh-trust-cancel"),
                    on_ok: Rc::new(on_trust),
                    on_cancel: Rc::new(on_cancel),
                    // Trusting a key takes a deliberate click: Enter declines, like Escape.
                    enter_accepts: false,
                    on_done: Rc::new(|_, _| {}),
                },
            );
        })
    }

    /// The error for a recorded key that does not match: no accept button, ever.
    pub(super) fn show_host_key_problem(
        &mut self,
        problem: HostKeyProblem,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("host-key-problem");
        let known_hosts = self.known_hosts_label();
        self.with_ui_window(cx, move |window, cx| {
            let (title, host, rows, explanation): (&str, String, Vec<(&'static str, String, &'static str)>, &str) =
                match problem {
                    HostKeyProblem::Changed { host, port, algorithm, fingerprint, recorded_at } => (
                        "SSH host key changed",
                        host_port(&host, port),
                        vec![
                            ("Key type", algorithm, "ssh-problem-algorithm"),
                            ("SHA-256", fingerprint, "ssh-problem-fingerprint"),
                            ("Recorded at", recorded_at, "ssh-problem-recorded"),
                        ],
                        "The server presented a different key than the one recorded for it.",
                    ),
                    HostKeyProblem::TypeChanged { host, port, presented, fingerprint, recorded, location } => (
                        "SSH host key type changed",
                        host_port(&host, port),
                        vec![
                            ("Presented", format!("{presented} {fingerprint}"), "ssh-problem-fingerprint"),
                            ("Recorded", recorded.join(", "), "ssh-problem-algorithm"),
                            ("Recorded at", location, "ssh-problem-recorded"),
                        ],
                        "This server is recorded with keys of another type, which is also what a downgrade attack looks like.",
                    ),
                    HostKeyProblem::Revoked { host, port, algorithm, fingerprint } => (
                        "SSH host key revoked",
                        host_port(&host, port),
                        vec![
                            ("Key type", algorithm, "ssh-problem-algorithm"),
                            ("SHA-256", fingerprint, "ssh-problem-fingerprint"),
                        ],
                        "This key is marked @revoked in a known_hosts file.",
                    ),
                };
            open_decision(
                window,
                cx,
                Decision {
                    title: title.into(),
                    width: 560.0,
                    body: Rc::new(move || {
                        let mut children = vec![paragraph(format!(
                            "The connection to {host} was stopped. {explanation}"
                        ))];
                        children.extend(
                            rows.iter()
                                .map(|(label, value, selector)| fact_row(label, value.clone(), selector)),
                        );
                        children.push(paragraph(
                            "This could be a man-in-the-middle attack, or the server was reinstalled. TableX never accepts a changed key by itself and offers no button to do so.",
                        ));
                        children.push(paragraph(format!(
                            "If the change is expected, ask the server's administrator, then remove the old entry for this host from the known_hosts file named above (TableX's own file is {known_hosts}) and connect again."
                        )));
                        column(children)
                    }),
                    ok: None,
                    cancel: ("Close", "ssh-hostkey-close"),
                    on_ok: Rc::new(|_, _| {}),
                    on_cancel: Rc::new(|_, _| {}),
                    enter_accepts: false,
                    on_done: Rc::new(|_, _| {}),
                },
            );
        });
    }

    /// The password / passphrase prompt of an unsaved secret. `false` when there is no window.
    fn ask_for_secret(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        request: SecretRequest,
        cx: &mut Context<Self>,
    ) -> bool {
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("ssh-secret");
        let root = cx.entity().downgrade();
        let key = saved.id.to_string();
        self.with_ui_window(cx, move |window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).masked(true));
            // The hop is named in the text (`jump 1 of 2 user@host:port`, `proxy host:port`), so the
            // user knows which server the secret goes to.
            let (title, label) = match (request.hop, request.need) {
                (SecretHop::Proxy, _) => (
                    "Proxy password".to_string(),
                    format!("Password for {}", request.who),
                ),
                (_, SecretNeed::Password) => (
                    "SSH password".to_string(),
                    format!("Password for {}", request.who),
                ),
                (_, SecretNeed::Passphrase) => (
                    "SSH key passphrase".to_string(),
                    format!(
                        "{} for the key {} ({})",
                        if request.retry {
                            "Wrong passphrase, try again"
                        } else {
                            "Passphrase"
                        },
                        request.key_path,
                        request.who
                    ),
                ),
            };
            let body_input = input.clone();
            let (need, hop) = (request.need, request.hop);
            let ok_root = root.clone();
            let submit = {
                let input = input.clone();
                move |_: &mut Window, cx: &mut gpui::App| {
                    let text = Secret::new(input.read(cx).value().to_string());
                    let (saved, uri) = (saved.clone(), uri_override.clone());
                    let root = ok_root.clone();
                    let _ = root.update(cx, |this, cx| {
                        this.provide_ssh_secret(saved, uri, attempt, hop, need, text, cx)
                    });
                }
            };
            open_decision(
                window,
                cx,
                Decision {
                    title,
                    width: 460.0,
                    body: Rc::new(move || {
                        column(vec![
                            paragraph(label.clone()),
                            div()
                                .debug_selector(|| "ssh-secret-input".into())
                                .child(Input::new(&body_input).mask_toggle())
                                .into_any_element(),
                            paragraph("It is kept in memory only while this connection is open."),
                        ])
                    }),
                    ok: Some(("Connect", "ssh-secret-ok")),
                    cancel: ("Cancel", "ssh-secret-cancel"),
                    enter_accepts: true,
                    on_done: {
                        let input = input.clone();
                        Rc::new(move |window, cx| clear_input(&input, window, cx))
                    },
                    on_ok: Rc::new(submit),
                    on_cancel: Rc::new(move |_, cx| {
                        let key = key.clone();
                        let _ = root.update(cx, |this, cx| {
                            this.fail_driver_connect(
                                &key,
                                attempt,
                                ConnectFailure::Message(
                                    "Connection cancelled: no SSH secret entered".into(),
                                ),
                                cx,
                            )
                        });
                    }),
                },
            );
        })
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    /// The user typed the secret: keep it for this connection's lifetime and connect again.
    fn provide_ssh_secret(
        &mut self,
        saved: SavedConnection,
        uri_override: Option<RuntimeUri>,
        attempt: u64,
        hop: SecretHop,
        need: SecretNeed,
        secret: Secret,
        cx: &mut Context<Self>,
    ) {
        let key = saved.id.to_string();
        if !self.attempt_is_current(&key, attempt) {
            return;
        }
        // Only the hop that asked gets the answer.
        let typed = SshSecrets::only(hop, need, secret);
        let entry = self.tunnel_ui.secrets.remove(&key).unwrap_or_default();
        self.tunnel_ui.secrets.insert(key, typed.or(entry));
        self.run_driver_connect(saved, uri_override, attempt, cx);
    }

    /// The bastion's keyboard-interactive questions as a dialog (echo respected). Everything the
    /// server wrote is shown as quoted plain text under a fixed line that names the server: it
    /// cannot retitle the dialog or pass for TableX.
    pub(super) fn show_keyboard_interactive(&mut self, prompt: KiPrompt, cx: &mut Context<Self>) {
        if prompt.cancel.is_cancelled() {
            let _ = prompt.reply.send(None);
            return;
        }
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("keyboard-interactive");
        let reply = prompt.reply.clone();
        let opened = self.with_ui_window(cx, move |window, cx| {
            let inputs: Vec<Entity<InputState>> = prompt
                .prompts
                .iter()
                .map(|item| {
                    let masked = !item.echo;
                    cx.new(|cx| InputState::new(window, cx).masked(masked))
                })
                .collect();
            let texts = ki_texts(&prompt);
            let (answer_inputs, ok_reply) = (inputs.clone(), prompt.reply.clone());
            let cancel_reply = prompt.reply.clone();
            let cleared = inputs.clone();
            open_decision(
                window,
                cx,
                Decision {
                    title: texts.title.to_string(),
                    width: 480.0,
                    body: Rc::new(move || {
                        let mut children = vec![div()
                            .debug_selector(|| "ssh-ki-host-line".into())
                            .text_size(px(13.0))
                            .text_color(theme::text())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(texts.host_line.clone())
                            .into_any_element()];
                        if !texts.server_text.is_empty() {
                            children.push(paragraph(
                                "What follows is text the server sent. It is not from TableX.",
                            ));
                            for (index, quoted) in texts.server_text.iter().enumerate() {
                                children.push(quoted_box(
                                    quoted.clone(),
                                    format!("ssh-ki-quoted-{index}"),
                                ));
                            }
                        }
                        for (index, (question, input)) in
                            texts.prompts.iter().zip(&inputs).enumerate()
                        {
                            let selector: SharedString = format!("ssh-ki-input-{index}").into();
                            children.push(quoted_box(
                                question.clone(),
                                format!("ssh-ki-prompt-{index}"),
                            ));
                            children.push(
                                div()
                                    .debug_selector(move || selector.to_string())
                                    .child(Input::new(input))
                                    .into_any_element(),
                            );
                        }
                        column(children)
                    }),
                    ok: Some(("Continue", "ssh-ki-ok")),
                    cancel: ("Cancel", "ssh-ki-cancel"),
                    enter_accepts: true,
                    on_done: Rc::new(move |window, cx| {
                        for input in &cleared {
                            clear_input(input, window, cx);
                        }
                    }),
                    on_ok: Rc::new(move |_, cx| {
                        let answers = answer_inputs
                            .iter()
                            .map(|input| Secret::new(input.read(cx).value().to_string()))
                            .collect();
                        let _ = ok_reply.send(Some(answers));
                    }),
                    on_cancel: Rc::new(move |_, _| {
                        let _ = cancel_reply.send(None);
                    }),
                },
            );
        });
        if !opened {
            let _ = reply.send(None);
        }
    }

    // --- failed connects ----------------------------------------------------------------------

    /// The connect attempt failed for good: status, Error Log (with the tunnel's code), queued
    /// runs and navigator loads. A changed host key also gets its blocking dialog.
    pub(super) fn fail_driver_connect(
        &mut self,
        connection_key: &str,
        attempt: u64,
        failure: ConnectFailure,
        cx: &mut Context<Self>,
    ) {
        let message = failure.message();
        if !self
            .sessions
            .finish_connect(connection_key, attempt, Err(message.clone()))
        {
            self.discard_pending_runs(
                connection_key,
                attempt,
                "Connection changed while connecting; run again",
                cx,
            );
            cx.notify();
            return;
        }
        self.sync_sidebar_connections(cx);
        match failure.code() {
            Some(code) => {
                let mut report = Report::new(
                    Severity::Error,
                    Area::Connect,
                    crate::sessions::redact_secrets(&message),
                );
                report.code = Some(code.to_string());
                report.severity = Severity::Error;
                self.report_event(Some(connection_key), report);
            }
            None => self.report_connection_failure(connection_key, &message),
        }
        if let ConnectFailure::HostKeyRejected(problem, _) = failure {
            self.show_host_key_problem(problem, cx);
        }
        self.discard_pending_runs(connection_key, attempt, &message, cx);
        if let Some(requests) = self.navigator_ui.pending_loads.remove(connection_key) {
            for (request, generation) in requests {
                self.navigator
                    .apply_load(&request, generation, Err(message.clone()));
            }
        }
        self.navigator_ui.generation = self.navigator.generation();
        cx.notify();
    }

    // --- "Test tunnel" ------------------------------------------------------------------------

    #[allow(clippy::result_large_err)] // the closure's `Err` is engine-connect's `ConnectFailure`; boxing its variants touches every consumer
    /// The dialog's "Test tunnel": open the tunnel the form describes, probe the database
    /// endpoint through it, drop it. Inline result, no connection is made.
    pub(super) fn start_tunnel_test(
        &mut self,
        form: ConnectionFormState,
        original: Option<engine_storage::connection::SavedConnectionRecord>,
        window: gpui::AnyWindowHandle,
        status: Rc<RefCell<TestStatus>>,
        cx: &mut Context<Self>,
    ) {
        let refresh = move |cx: &mut gpui::App| {
            let _ = window.update(cx, |_, window, _| window.refresh());
        };
        if *status.borrow() == TestStatus::Running {
            return;
        }
        let Some((ssh, secrets, accounts)) = self.form_tunnel(&form, original.as_ref()) else {
            *status.borrow_mut() = TestStatus::Failure("Turn the SSH tunnel or the proxy on first".into());
            refresh(cx);
            return;
        };
        let uri = match runtime_uri_for_form(&form, original.as_ref()) {
            Ok(uri) => uri,
            Err(error) => {
                *status.borrow_mut() = TestStatus::Failure(error);
                refresh(cx);
                return;
            }
        };
        *status.borrow_mut() = TestStatus::Running;
        refresh(cx);
        let kind = form.kind;
        let started = Instant::now();
        let (connector, prompts) = (self.connector(), self.tunnel_prompts());
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                connector.test_tunnel(kind, &uri, ssh, secrets, accounts, &prompts)
            })
            .await
        });
        let (retry_form, retry_original, retry_status) =
            (form.clone(), original.clone(), status.clone());
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        *status.borrow_mut() = TestStatus::Success {
                            millis: started.elapsed().as_millis() as u64,
                        };
                    }
                    Err(ConnectFailure::HostKeyUnknown(request)) => {
                        // Ask, then test again with the key trusted.
                        let again: RootCallback = {
                            let (form, original, status) = (
                                retry_form.clone(),
                                retry_original.clone(),
                                retry_status.clone(),
                            );
                            Rc::new(move |this, cx| {
                                this.start_tunnel_test(
                                    form.clone(),
                                    original.clone(),
                                    window,
                                    status.clone(),
                                    cx,
                                )
                            })
                        };
                        this.ask_to_trust_for_test(request, window, status.clone(), again, cx);
                    }
                    Err(failure) => {
                        *status.borrow_mut() = TestStatus::Failure(
                            crate::sessions::redact_secrets(&failure.message()),
                        );
                        this.log_tunnel_test_failure(&failure);
                        if let ConnectFailure::HostKeyRejected(problem, _) = failure {
                            this.show_host_key_problem(problem, cx);
                        }
                    }
                }
                cx.notify();
            });
            cx.update(|cx| refresh(cx));
        })
        .detach();
    }

    /// The "Trust this host?" dialog of a dialog-side test (connection or tunnel): the result
    /// line says the key is not trusted yet; trusting runs `again`.
    pub(super) fn ask_to_trust_for_test(
        &mut self,
        request: HostKeyRequest,
        window: gpui::AnyWindowHandle,
        status: Rc<RefCell<TestStatus>>,
        again: RootCallback,
        cx: &mut Context<Self>,
    ) {
        *status.borrow_mut() = TestStatus::Failure(format!(
            "The SSH host key of {} is not trusted yet",
            host_port(&request.host, request.port)
        ));
        let root = cx.entity().downgrade();
        let on_trust = {
            let (root, request, status) = (root.clone(), request.clone(), status.clone());
            move |_: &mut Window, cx: &mut gpui::App| {
                let (request, status, again) = (request.clone(), status.clone(), again.clone());
                let _ = root.update(cx, |this, cx| {
                    this.trust_then(request, window, status, again, cx)
                });
            }
        };
        self.ask_to_trust(request, on_trust, |_, _| {}, cx);
    }

    #[allow(clippy::result_large_err)] // the closure's `Err` is engine-connect's `ConnectFailure`; boxing its variants touches every consumer
    /// Records the key, then runs `again`; a failure to record it is the result line.
    fn trust_then(
        &mut self,
        request: HostKeyRequest,
        window: gpui::AnyWindowHandle,
        status: Rc<RefCell<TestStatus>>,
        again: RootCallback,
        cx: &mut Context<Self>,
    ) {
        let opener = Arc::clone(&self.tunnel_ui.opener);
        let known_hosts = match ssh_tunnel::no_known_hosts(self.tunnel_ui.known_hosts.as_deref()) {
            Ok(path) => path.to_path_buf(),
            Err(failure) => {
                *status.borrow_mut() = TestStatus::Failure(failure.message());
                cx.notify();
                return;
            }
        };
        let spec = ssh_tunnel::trust_spec(&request, &known_hosts);
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                opener.trust_host_key(&spec, &request.host, request.port, &request.public_key)
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => again(this, cx),
                Err(error) => {
                    *status.borrow_mut() = TestStatus::Failure(error.to_string());
                    cx.notify();
                }
            });
            cx.update(|cx| {
                let _ = window.update(cx, |_, window, _| window.refresh());
            });
        })
        .detach();
    }

    /// A failed test goes to the Error Log with the tunnel's code, like a failed connect.
    pub(super) fn log_tunnel_test_failure(&mut self, failure: &ConnectFailure) {
        let message = crate::sessions::redact_secrets(&failure.message());
        let mut report = Report::new(
            Severity::Warning,
            Area::Connect,
            format!("Tunnel test failed: {message}"),
        );
        report.code = failure.code().map(str::to_string);
        self.report_event(None, report);
    }
}
