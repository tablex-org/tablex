//! The AI assistant inside the app, without its panel: the tool layer over the app's agent
//! connections, the API key, the models list, and the chat the panel renders.
//!
//! * [`ChatSession`] (an entity) is the chat: a display model to render and a few methods to call.
//! * [`AssistantUi`] (a field of `AppRoot`) holds what the chats share: the one `ToolLayer` (built
//!   on first use, audit lines under `front_end = "assistant"`, the default limits and the
//!   [`AppApprover`]: a write waits for the user's click on a card in the chat), the key store,
//!   the transport.
//! * The `assistant_*` methods of `AppRoot` are what a view calls: they build a run from the
//!   current Settings and the active tab, and keep the chat's status (key, access) in step.
//!
//! Everything blocking (the model, statements, the credential store, the models list) runs off the
//! UI thread; a cancel is `cancel_detached`, never `cancel`.

mod approval;
mod ask;
mod blocks;
mod dock;
mod history;
mod layers;
mod markup;
mod model;
pub mod panel;
mod records;
mod run;
#[cfg(any(test, feature = "ui-scenarios"))]
mod scenario;
#[cfg(any(test, feature = "ui-scenarios"))]
pub(in crate::app) mod scripted;
mod session;
pub(in crate::app) mod store;
#[cfg(test)]
pub(super) mod tests;

#[cfg(any(test, feature = "ui-scenarios"))]
pub use approval::AppApprover;
#[cfg(test)]
pub use approval::{Answer, Question, APPROVAL_DELAY};
#[cfg(test)]
pub(in crate::app) use ask::{compose, parse_composed, FIX_UNSURE_INSTRUCTION, TABLE_INSTRUCTION};
#[cfg(test)]
pub(in crate::app) use ask::{compose_fix_withheld, mask_error};
pub(in crate::app) use ask::{editor_menu_rows, native_menu, Editing, EditorAsk, EditorAsks};
pub(in crate::app) use dock::{is_sql_editor, DockState};
pub use layers::LayerSource;
pub use model::ChatError;
#[cfg(test)]
pub use model::{ChatItem, ChatNotice, ErrorAction, ErrorKind, StepState, ToolStep};
#[cfg(test)]
pub use records::MemoryRecords;
pub use records::{AgentRecords, RecordsError, StorageRecords};
pub use run::RunSetup;
#[cfg(test)]
pub use session::Opened;
pub use session::{ChatEvent, ChatSession, ChatStatus, Readiness, SendOutcome};
pub(in crate::app) use store::ConversationStore;

use super::*;
use crate::ai::keys::ApiKeyStore;
use crate::error_log::Area;
use crate::settings::{AiProvider, AiSettings, Choice};
use engine_agent::CancelToken;
#[cfg(any(test, feature = "ui-scenarios"))]
use engine_agent::ToolLayer;
use engine_ai::{
    AnthropicConfig, AnthropicProvider, ChatProvider, ConfigError, HttpTransport, KeySource,
    OpenAiConfig, OpenAiProvider, Scope,
};
use engine_connect::connect::AgentConnections;
use engine_connect::profile::AgentLevel;
use gpui::{Entity, Subscription};
use history::{DeleteAll, History};
use layers::{LayerCell, LayerError};
use std::collections::HashMap;
use std::sync::Mutex;

/// The `front_end` of every audit line the app writes (the standalone MCP server writes `mcp`).
pub const FRONT_END: &str = "assistant";

/// How long a quit waits for the agent sessions to close: under GPUI's own limit for quit tasks
/// (200 ms), so the wait is ours and not a timeout of the framework.
pub(in crate::app) const QUIT_WAIT: Duration = Duration::from_millis(150);

/// What the credential store said about the API key, as far as it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KeyState {
    Unknown,
    Checking,
    Set,
    NotSet,
    Unavailable(String),
}

/// Why a connection's secrets could not be had: the record or the credential store could not be
/// read. The text never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsUnavailable(pub String);

impl std::fmt::Display for SecretsUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretsUnavailable {}

/// Where the exact secrets of a connection (its password, the SSH password and passphrase) come
/// from, to mask them in a database's error text before it goes into a message. The production
/// source reads the connection's record and the credential store (a blocking call: not for the
/// UI thread); tests and scenarios have none or a fixed list. A source that cannot say is an
/// error, never an empty list: the caller leaves the error text out then.
pub trait SecretSource: Send + Sync {
    fn secrets(
        &self,
        connection_id: &str,
    ) -> Result<Vec<engine_agent::SecretText>, SecretsUnavailable>;
}

/// No secrets known (scenarios and tests): the error text is only cleaned and pattern-masked.
#[cfg(any(test, feature = "ui-scenarios"))]
pub struct NoSecrets;

#[cfg(any(test, feature = "ui-scenarios"))]
impl SecretSource for NoSecrets {
    fn secrets(&self, _: &str) -> Result<Vec<engine_agent::SecretText>, SecretsUnavailable> {
        Ok(Vec::new())
    }
}

/// The app has no connection layer to read secrets with: nothing can be said to be safe.
struct NoAgent;

impl SecretSource for NoAgent {
    fn secrets(&self, _: &str) -> Result<Vec<engine_agent::SecretText>, SecretsUnavailable> {
        Err(SecretsUnavailable(
            "the connection layer is not available".to_owned(),
        ))
    }
}

/// The secrets the agent layer itself masks for a connection, read without leaving them in the
/// layer (see `AgentConnections::mask_secrets`).
struct AgentSecrets(Arc<AgentConnections>);

impl SecretSource for AgentSecrets {
    fn secrets(
        &self,
        connection_id: &str,
    ) -> Result<Vec<engine_agent::SecretText>, SecretsUnavailable> {
        self.0
            .mask_secrets(connection_id)
            .map_err(|error| SecretsUnavailable(error.to_string()))
    }
}

/// What the app keeps of the assistant.
pub(super) struct AssistantUi {
    agent: Option<Arc<AgentConnections>>,
    /// The tool layer, built by the first run on its own thread.
    layers: Arc<LayerCell>,
    /// Where a connection's own agent access is saved.
    pub(in crate::app) records: Arc<dyn AgentRecords>,
    /// Where a connection's secrets come from, to mask an error text (see [`SecretSource`]).
    pub(in crate::app) secrets: Arc<dyn SecretSource>,
    /// The credential store (on Anthropic's account; `for_provider` gives the others').
    pub(super) keys: Arc<ApiKeyStore>,
    transport: Arc<dyn HttpTransport>,
    /// What the credential store said about each provider's key, as far as it was asked.
    key_states: HashMap<AiProvider, KeyState>,
    /// The key row's last failure per provider, shown until the next attempt: one provider's
    /// failure is never shown under another's row.
    key_errors: HashMap<AiProvider, String>,
    /// The checks of the credential store ("is a key saved?"), one per provider.
    key_check_tasks: HashMap<AiProvider, gpui::Task<()>>,
    /// A save or a removal of a provider's key in flight (the provider's row is busy meanwhile).
    key_save_tasks: HashMap<AiProvider, gpui::Task<()>>,
    /// A change of the connection's agent access is being saved.
    pub(super) access_busy: bool,
    /// Why the last change of the agent access failed.
    pub(super) access_error: Option<String>,
    access_task: Option<gpui::Task<()>>,
    /// The model ids fetched from each provider (the picker offers them after the built-in ones).
    fetched_models: HashMap<AiProvider, Vec<String>>,
    /// Counts changes of `fetched_models`, so the picker knows when to reload its items.
    pub(super) models_version: u64,
    /// The last failure of each provider's model list request.
    models_errors: HashMap<AiProvider, String>,
    /// The model list requests in flight, one per provider: a request is its provider's, so it
    /// neither blocks another provider's Refresh nor fills another provider's list.
    model_requests: HashMap<AiProvider, ModelRequest>,
    chat: Option<Entity<ChatSession>>,
    /// Where conversations are kept (`None`: nowhere; scenarios and tests start without one).
    pub(super) store: Option<Arc<ConversationStore>>,
    /// The History list of the chat's connection.
    pub(super) history: History,
    /// Settings: "Delete all conversations".
    pub(super) delete_all: DeleteAll,
    /// The temporary folder of a scenario's conversations, removed with it.
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) scenario_folder: Option<scenario::ScenarioFolder>,
    /// How many `agent-revoke` threads were started (tests).
    #[cfg(test)]
    pub(super) revoke_threads: std::sync::atomic::AtomicUsize,
    /// A scenario wants the History menu open on the first paint.
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) scenario_open_history: bool,
    /// A scripted provider instead of the real one (tests, scenarios).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(in crate::app) provider_override: Option<Arc<dyn ChatProvider>>,
    /// The dock on the right of the main area.
    pub(in crate::app) dock: DockState,
    /// A write waits for the user's answer (the toolbar shows it; kept here so the root's render
    /// does not have to read the chat).
    pub(in crate::app) asking: bool,
    _chat_events: Option<Subscription>,
    _quit: Option<Subscription>,
    _release: Option<Subscription>,
}

impl AssistantUi {
    pub(super) fn new(
        agent: Option<Arc<AgentConnections>>,
        audit_path: Option<std::path::PathBuf>,
        keys: Arc<ApiKeyStore>,
        transport: Arc<dyn HttpTransport>,
        records: Arc<dyn AgentRecords>,
        store: Option<Arc<ConversationStore>>,
        secrets: Arc<dyn SecretSource>,
    ) -> Self {
        Self {
            layers: Arc::new(LayerCell::new(agent.clone(), audit_path)),
            agent,
            records,
            secrets,
            keys,
            transport,
            key_states: HashMap::new(),
            key_errors: HashMap::new(),
            key_check_tasks: HashMap::new(),
            key_save_tasks: HashMap::new(),
            access_busy: false,
            access_error: None,
            access_task: None,
            fetched_models: HashMap::new(),
            models_version: 0,
            models_errors: HashMap::new(),
            model_requests: HashMap::new(),
            chat: None,
            store,
            history: History::default(),
            delete_all: DeleteAll::default(),
            #[cfg(any(test, feature = "ui-scenarios"))]
            scenario_folder: None,
            #[cfg(test)]
            revoke_threads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "ui-scenarios"))]
            scenario_open_history: false,
            #[cfg(any(test, feature = "ui-scenarios"))]
            provider_override: None,
            dock: DockState::default(),
            asking: false,
            _chat_events: None,
            _quit: None,
            _release: None,
        }
    }

    /// The production wiring: the app's HTTP client, the OS credential store and the audit log
    /// under the app data directory. Scenarios and tests use an in-memory key store and no log.
    pub(super) fn production(agent: Option<Arc<AgentConnections>>, isolated: bool) -> Self {
        let user_agent = format!("TableX/{}", env!("CARGO_PKG_VERSION"));
        let transport = crate::ai::transport::ReqwestTransport::new(user_agent);
        // Tests talk to 127.0.0.1 and must not depend on a machine-wide proxy.
        #[cfg(test)]
        let transport = transport.direct();
        let transport: Arc<dyn HttpTransport> = Arc::new(transport);
        #[cfg(any(test, feature = "ui-scenarios"))]
        if isolated {
            return Self::new(
                agent,
                None,
                Arc::new(ApiKeyStore::memory()),
                transport,
                Arc::new(records::MemoryRecords::default()),
                None,
                Arc::new(NoSecrets),
            );
        }
        #[cfg(not(any(test, feature = "ui-scenarios")))]
        let _ = isolated;
        let secrets: Arc<dyn SecretSource> = match &agent {
            Some(agent) => Arc::new(AgentSecrets(Arc::clone(agent))),
            None => Arc::new(NoAgent),
        };
        Self::new(
            agent,
            engine_connect::platform::audit_log_path(),
            Arc::new(ApiKeyStore::system()),
            transport,
            Arc::new(StorageRecords),
            // `<app data dir>/assistant/conversations` (`TABLEX_DATA_DIR` moves it with the rest).
            engine_connect::platform::app_data_dir().map(|dir| {
                Arc::new(ConversationStore::new(
                    dir.join("assistant").join("conversations"),
                ))
            }),
            secrets,
        )
    }

    /// Where a scenario's conversations are (tests).
    #[cfg(test)]
    pub(in crate::app) fn scenario_folder_path(&self) -> Option<std::path::PathBuf> {
        self.scenario_folder
            .as_ref()
            .map(|folder| folder.path().to_path_buf())
    }

    /// Lets go of a scenario's folder, which removes it (tests).
    #[cfg(test)]
    pub(in crate::app) fn scenario_folder_for_test_drop(&mut self) {
        self.scenario_folder = None;
    }

    /// The tool layer a test put in (tests).
    #[cfg(test)]
    pub(super) fn layer_for_test(&self) -> Arc<ToolLayer> {
        self.layers.built().expect("a layer was set")
    }

    /// A tool layer over fakes instead of the real one (tests, scenarios).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) fn set_layer_for_test(&self, layer: Arc<ToolLayer>) {
        self.layers.preset(layer);
    }

    /// The provider for `settings`: the transport, the key source of the selected provider's own
    /// account and its address. Anthropic's fallback and betas are Anthropic's only.
    pub(super) fn provider(
        &self,
        settings: &AiSettings,
    ) -> Result<Arc<dyn ChatProvider>, ConfigError> {
        let base_url = settings.selected().base_url.trim().to_owned();
        let keys: Arc<dyn KeySource> = self.keys.for_provider(settings.provider);
        let transport = Arc::clone(&self.transport);
        // Settings the provider would refuse are refused whichever provider is plugged in.
        let provider: Arc<dyn ChatProvider> = match settings.provider.flavor() {
            None => Arc::new(AnthropicProvider::new(
                transport,
                keys,
                AnthropicConfig {
                    base_url,
                    server_fallback: settings.fallback,
                    ..AnthropicConfig::default()
                },
            )?),
            Some(flavor) => Arc::new(OpenAiProvider::new(
                transport,
                keys,
                OpenAiConfig {
                    base_url,
                    ..OpenAiConfig::for_flavor(flavor)
                },
            )?),
        };
        #[cfg(any(test, feature = "ui-scenarios"))]
        if let Some(provider) = &self.provider_override {
            return Ok(Arc::clone(provider));
        }
        Ok(provider)
    }

    /// What the credential store said about `provider`'s key.
    pub(super) fn key_state(&self, provider: AiProvider) -> &KeyState {
        static UNKNOWN: KeyState = KeyState::Unknown;
        self.key_states.get(&provider).unwrap_or(&UNKNOWN)
    }

    pub(super) fn set_key_state(&mut self, provider: AiProvider, state: KeyState) {
        self.key_states.insert(provider, state);
    }

    /// The models the Settings picker offers for `provider`: its built-in ones, then the ones
    /// fetched from it.
    pub(super) fn models_for(&self, provider: AiProvider) -> Vec<String> {
        crate::ai::merge_models(
            provider,
            self.fetched_models
                .get(&provider)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// `provider`'s model list is being asked.
    pub(super) fn models_refreshing(&self, provider: AiProvider) -> bool {
        self.model_requests.contains_key(&provider)
    }

    /// Why the last request for `provider`'s model list failed.
    pub(super) fn models_error(&self, provider: AiProvider) -> Option<&str> {
        self.models_errors.get(&provider).map(String::as_str)
    }

    /// `provider`'s key is being saved or removed.
    pub(super) fn key_busy(&self, provider: AiProvider) -> bool {
        self.key_save_tasks.contains_key(&provider)
    }

    /// Why the last attempt to save or remove `provider`'s key failed.
    pub(super) fn key_error(&self, provider: AiProvider) -> Option<&str> {
        self.key_errors.get(&provider).map(String::as_str)
    }

    /// Ends what the agent has open on `connection`, off the calling thread: see [`Self::revoke_all`].
    pub(super) fn revoke(&self, connection: &str) {
        self.revoke_all(vec![connection.to_owned()]);
    }

    /// Ends what the agent has open on each of `connections`, on ONE thread (a cancel is a network
    /// round trip, and several lowered connections are not worth several threads): the tool layer
    /// disconnects the session (and cancels a statement that runs, over the network), then the agent
    /// connections drop the tunnel, as `AgentConnections::revoke` documents. Nothing at all when no
    /// tool layer has been built: no agent session can exist before the first run builds it (the
    /// startup comparison against default settings lands here too).
    pub(super) fn revoke_all(&self, connections: Vec<String>) {
        if self.agent.is_none() || connections.is_empty() || !self.layers.exists() {
            return;
        }
        #[cfg(test)]
        self.revoke_threads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let layers = Arc::clone(&self.layers);
        let agent = self.agent.clone();
        let work = move || {
            for connection in &connections {
                if let Some(layer) = layers.built() {
                    layer.revoke(connection);
                }
                if let Some(agent) = &agent {
                    agent.revoke(connection);
                }
            }
        };
        let slot = Arc::new(Mutex::new(Some(work)));
        let handed = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("agent-revoke".into())
            .spawn(move || {
                if let Some(work) = handed.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    work();
                }
            });
        if spawned.is_err() {
            // No thread to be had: the revoke still has to happen.
            if let Some(work) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                work();
            }
        }
    }

    /// What the app does on quit: sessions closed and the audit log flushed, on a thread.
    fn shutdown_work(&self) -> impl FnOnce() + Send + 'static {
        let layers = Arc::clone(&self.layers);
        move || layers.shut_down()
    }

    pub(super) fn chat(&self) -> Option<&Entity<ChatSession>> {
        self.chat.as_ref()
    }

    /// The word the Settings key row shows for what the credential store said about `provider`.
    pub(super) fn key_state_label(&self, provider: AiProvider) -> &'static str {
        match self.key_state(provider) {
            KeyState::Unknown | KeyState::Checking => "Checking\u{2026}",
            KeyState::Set => "Saved",
            KeyState::NotSet if !provider.requires_key() => "Not set (optional)",
            KeyState::NotSet => "Not set",
            KeyState::Unavailable(_) => "Unavailable",
        }
    }
}

impl AppRoot {
    /// The assistant's hooks into the app's life: a running chat is cancelled (without waiting) and
    /// the agent sessions closed, within a short bound, when the app quits.
    pub(super) fn init_assistant(&mut self, cx: &mut Context<Self>) {
        self.assistant_check_store_root(cx);
        self.assistant._quit = Some(cx.on_app_quit(|this, cx| {
            this.flush_assistant_layout();
            #[cfg(any(test, feature = "ui-scenarios"))]
            {
                this.assistant.scenario_folder = None;
            }
            if let Some(chat) = this.assistant.chat().cloned() {
                chat.update(cx, |chat, _| chat.cancel_for_quit());
            }
            let work = this.assistant.shutdown_work();
            async move {
                let (done, finished) = smol::channel::bounded::<()>(1);
                let spawned = std::thread::Builder::new()
                    .name("assistant-quit".into())
                    .spawn(move || {
                        work();
                        let _ = done.send_blocking(());
                    });
                if spawned.is_err() {
                    return;
                }
                // Shorter than GPUI's own wait for quit tasks, however slow a server is to answer.
                smol::future::or(
                    async {
                        let _ = finished.recv().await;
                    },
                    async {
                        smol::Timer::after(QUIT_WAIT).await;
                    },
                )
                .await;
            }
        }));
        // The window can also go without a quit: then the chat is dropped with the app state
        // (which cancels its run) and the agent sessions are closed on a thread nobody waits for.
        // A layout change still waiting for its quiet time is written first.
        self.assistant._release = Some(cx.on_release(|this, _| {
            this.flush_assistant_layout();
            let work = this.assistant.shutdown_work();
            let _ = std::thread::Builder::new()
                .name("assistant-release".into())
                .spawn(work);
        }));
    }

    // ---- what a view reads and calls ---------------------------------------------------------

    /// The chat, created on first use with the active tab's connection and namespace.
    pub(super) fn assistant_chat(&mut self, cx: &mut Context<Self>) -> Entity<ChatSession> {
        if let Some(chat) = self.assistant.chat() {
            return chat.clone();
        }
        let (scope, label) = self.assistant_active_scope();
        let chat = cx.new(|_| ChatSession::new(scope, label));
        self.assistant._chat_events =
            Some(cx.subscribe(&chat, |this, _, event: &ChatEvent, cx| {
                match event {
                    // The sentence the chat shows; it holds neither the key nor the request.
                    ChatEvent::Failed(error) => {
                        this.report_error(Area::App, None, format!("Assistant: {}", error.message));
                    }
                    ChatEvent::Asking(asking) => {
                        this.assistant.asking = *asking;
                        // The toolbar's mark, whether or not the dock is open.
                        cx.notify();
                    }
                    ChatEvent::Saved => this.assistant_refresh_history(true, cx),
                    ChatEvent::NotKept(why) => {
                        this.report_error(
                            Area::App,
                            None,
                            format!("Assistant: the conversation could not be kept ({why})"),
                        );
                    }
                }
            }));
        self.assistant.chat = Some(chat.clone());
        self.assistant_sync(cx);
        chat
    }

    /// The active tab's connection and namespace, and the connection's name.
    pub(super) fn assistant_active_scope(&self) -> (Option<Scope>, String) {
        let Some(tab) = self
            .workspace
            .active_tab()
            .filter(|tab| !tab.connection_id.is_empty())
        else {
            return (None, String::new());
        };
        let label = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))
            .map(|saved| saved.name.clone())
            .unwrap_or_default();
        let non_empty = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        (
            Some(Scope {
                connection_id: tab.connection_id.clone(),
                database: non_empty(&tab.database),
                schema: tab.schema.as_deref().and_then(non_empty),
            }),
            label,
        )
    }

    /// The active tab is on another connection or namespace than a chat that has history: a new
    /// chat would follow it. (An empty chat follows the active tab by itself.)
    pub(super) fn assistant_scope_differs(&self, cx: &gpui::App) -> bool {
        let Some(chat) = self.assistant.chat() else {
            return false;
        };
        let chat = chat.read(cx);
        // No active tab on a connection: nothing to differ from.
        !chat.is_empty()
            && self
                .assistant_active_scope()
                .0
                .is_some_and(|active| chat.scope() != Some(&active))
    }

    /// An empty chat (nothing said yet) is about whatever the active tab is about; one with
    /// messages never switches by itself (the scope bar offers a new chat).
    pub(super) fn assistant_follow_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.assistant.chat().cloned() else {
            return;
        };
        let (active, label) = self.assistant_active_scope();
        let Some(active) = active else {
            return;
        };
        let follow = {
            let chat = chat.read(cx);
            chat.is_empty()
                && !chat.is_pinned()
                && !chat.is_running()
                && chat.scope() != Some(&active)
        };
        if follow {
            chat.update(cx, |chat, cx| chat.new_chat(Some(active), label, cx));
        }
    }

    /// The chat's readiness and the connection's access, from the key state, the saved
    /// connections and the connection types of Settings.
    fn assistant_status(&self, scope: Option<&Scope>) -> ChatStatus {
        let access = scope.map_or(engine_agent::AgentAccess::Off, |scope| {
            self.effective_for(&scope.connection_id).agent_access
        });
        let ai = &self.settings.ai;
        let readiness = if scope.is_none() {
            Readiness::NoConnection
        } else if access == engine_agent::AgentAccess::Off {
            Readiness::AccessOff
        } else {
            // The key comes first: with OpenAI the model list cannot be asked without one.
            let model_or_ready = || {
                if ai.selected().model.trim().is_empty() {
                    Readiness::NoModel
                } else {
                    Readiness::Ready
                }
            };
            match self.assistant.key_state(ai.provider) {
                KeyState::Set => model_or_ready(),
                // Ollama and a compatible server work without a key.
                KeyState::NotSet if !ai.provider.requires_key() => model_or_ready(),
                KeyState::NotSet => Readiness::NoApiKey,
                KeyState::Unknown | KeyState::Checking => Readiness::CheckingKey,
                KeyState::Unavailable(why) => Readiness::KeyUnavailable(why.clone()),
            }
        };
        ChatStatus { readiness, access }
    }

    /// Brings the chat's status in step with the app. Called when the key, a connection or the
    /// settings change.
    pub(super) fn assistant_sync(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.assistant.chat().cloned() else {
            return;
        };
        // The applied provider's key (asked once; a provider chosen in Settings is asked here).
        self.assistant_refresh_key(self.settings.ai.provider, cx);
        self.assistant_follow_active_tab(cx);
        let status = self.assistant_status(chat.read(cx).scope());
        // Kept when the setting is on, there is a place for it and the connection exists; decided
        // again whenever the chat is synced and read when a run ends.
        let keeping = self.settings.ai.keep_conversations
            && self
                .assistant
                .store
                .as_ref()
                .is_some_and(|store| !store.root_is_link())
            && chat
                .read(cx)
                .scope()
                .is_some_and(|scope| self.connection_name(&scope.connection_id).is_some());
        chat.update(cx, |chat, cx| {
            chat.set_keeping(keeping);
            chat.set_status(status, cx)
        });
        self.assistant_push_context(cx);
        // The list follows the chat's connection (free while the dock is closed or when the
        // connection is the one already listed).
        self.assistant_refresh_history(false, cx);
    }

    /// Asks the credential store whether `provider`'s key is saved (off the UI thread), once.
    pub(super) fn assistant_refresh_key(&mut self, provider: AiProvider, cx: &mut Context<Self>) {
        if *self.assistant.key_state(provider) != KeyState::Unknown {
            return;
        }
        self.assistant.set_key_state(provider, KeyState::Checking);
        let keys = self.assistant.keys.for_provider(provider);
        let task = cx.spawn(async move |this, cx| {
            let state = cx
                .background_executor()
                .spawn(async move {
                    match keys.is_set() {
                        Ok(true) => KeyState::Set,
                        Ok(false) => KeyState::NotSet,
                        Err(error) => KeyState::Unavailable(error.to_string()),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.set_key_state(provider, state);
                this.assistant.key_check_tasks.remove(&provider);
                this.assistant_sync(cx);
                cx.notify();
            });
        });
        self.assistant.key_check_tasks.insert(provider, task);
    }

    /// Asks the credential store again after it could not be read (the panel opens, a key is
    /// saved): `Unavailable` is not a final answer.
    pub(super) fn assistant_recheck_key(&mut self, cx: &mut Context<Self>) {
        let provider = self.settings.ai.provider;
        if matches!(self.assistant.key_state(provider), KeyState::Unavailable(_)) {
            self.assistant.set_key_state(provider, KeyState::Unknown);
        }
        self.assistant_refresh_key(provider, cx);
    }

    /// Saves `key` as `provider`'s in the credential store (off the UI thread). The text is not
    /// kept.
    pub(super) fn assistant_save_key(
        &mut self,
        provider: AiProvider,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if self.assistant.key_busy(provider) {
            return;
        }
        self.assistant.key_errors.remove(&provider);
        // A check still waiting for the store would answer with what was before.
        self.assistant.key_check_tasks.remove(&provider);
        let keys = self.assistant.keys.for_provider(provider);
        let task = cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move { keys.save(&key) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant_key_changed(provider, saved.map(|()| KeyState::Set), cx)
            });
        });
        self.assistant.key_save_tasks.insert(provider, task);
        cx.notify();
    }

    /// Removes `provider`'s saved key (off the UI thread).
    pub(super) fn assistant_remove_key(&mut self, provider: AiProvider, cx: &mut Context<Self>) {
        if self.assistant.key_busy(provider) {
            return;
        }
        self.assistant.key_errors.remove(&provider);
        self.assistant.key_check_tasks.remove(&provider);
        let keys = self.assistant.keys.for_provider(provider);
        let task = cx.spawn(async move |this, cx| {
            let removed = cx
                .background_executor()
                .spawn(async move { keys.remove() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant_key_changed(provider, removed.map(|()| KeyState::NotSet), cx)
            });
        });
        self.assistant.key_save_tasks.insert(provider, task);
        cx.notify();
    }

    fn assistant_key_changed(
        &mut self,
        provider: AiProvider,
        outcome: Result<KeyState, crate::ai::keys::KeyStoreError>,
        cx: &mut Context<Self>,
    ) {
        self.assistant.key_save_tasks.remove(&provider);
        match outcome {
            Ok(state) => self.assistant.set_key_state(provider, state),
            Err(error) => {
                self.assistant
                    .key_errors
                    .insert(provider, error.to_string());
                // What is in the store is unknown now: ask it, rather than stay on "checking".
                self.assistant.set_key_state(provider, KeyState::Unknown);
                self.assistant_refresh_key(provider, cx);
            }
        }
        self.assistant_sync(cx);
        cx.notify();
    }

    /// What a run needs from Settings and the app, or why the assistant cannot be set up.
    fn assistant_setup(&mut self) -> Result<RunSetup, LayerError> {
        let ai = &self.settings.ai;
        Ok(RunSetup {
            provider: self.assistant.provider(ai).map_err(LayerError::Config)?,
            // Built by the run, on its own thread.
            layer: LayerSource::Cell(Arc::clone(&self.assistant.layers)),
            model: ai.selected().model.clone(),
            provider_id: ai.provider.id(),
            effort: ai.effort,
            // Nothing is written with "Keep conversations on this computer" off.
            persist: self
                .assistant
                .store
                .clone()
                .filter(|_| self.settings.ai.keep_conversations),
        })
    }

    /// Sends `text` in the chat.
    pub(super) fn assistant_send(&mut self, text: String, cx: &mut Context<Self>) -> SendOutcome {
        let chat = self.assistant_chat(cx);
        // The access may have changed since the chat was last synced (a type's level edited, a
        // connection's override): decided from the app's state now, not from the last sync.
        let status = self.assistant_status(chat.read(cx).scope());
        chat.update(cx, |chat, cx| chat.set_status(status, cx));
        let readiness = chat.read(cx).status().readiness.clone();
        if readiness != Readiness::Ready {
            return SendOutcome::NotReady(readiness);
        }
        match self.assistant_setup() {
            Ok(setup) => chat.update(cx, |chat, cx| chat.send(text, setup, cx)),
            Err(error) => {
                chat.update(cx, |chat, cx| {
                    chat.fail(ChatError::config(error.to_string()), cx)
                });
                SendOutcome::SetupFailed
            }
        }
    }

    /// Stops the answer (never blocks).
    pub(super) fn assistant_stop(&mut self, cx: &mut Context<Self>) {
        if let Some(chat) = self.assistant.chat().cloned() {
            chat.update(cx, |chat, cx| chat.stop(cx));
        }
    }

    /// Sends the last request again after an error or a stop.
    pub(super) fn assistant_retry(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(chat) = self.assistant.chat().cloned() else {
            return false;
        };
        let status = self.assistant_status(chat.read(cx).scope());
        chat.update(cx, |chat, cx| chat.set_status(status, cx));
        match self.assistant_setup() {
            Ok(setup) => chat.update(cx, |chat, cx| chat.retry(setup, cx)),
            Err(error) => {
                chat.update(cx, |chat, cx| {
                    chat.fail(ChatError::config(error.to_string()), cx)
                });
                false
            }
        }
    }

    /// Drops the conversation and starts over on the active tab's connection and namespace.
    pub(super) fn assistant_new_chat(&mut self, cx: &mut Context<Self>) {
        let chat = self.assistant_chat(cx);
        let (scope, label) = self.assistant_active_scope();
        chat.update(cx, |chat, cx| chat.new_chat(scope, label, cx));
        self.assistant_sync(cx);
    }

    /// Sets the chat's connection's own agent access (the first-use disclosure's choice) the way
    /// the connection dialog does: it goes into the record's overrides (off the UI thread), the
    /// in-memory policy follows, an agent session opened under the old level is closed, and the
    /// chat's status is recomputed (the connection type may allow less than was asked).
    pub(super) fn assistant_set_agent_access(&mut self, level: AgentLevel, cx: &mut Context<Self>) {
        let Some(connection) = self.assistant.chat().and_then(|chat| {
            chat.read(cx)
                .scope()
                .map(|scope| scope.connection_id.clone())
        }) else {
            return;
        };
        if self.assistant.access_busy {
            return;
        }
        // Only what the connection's type lets the user choose.
        if !self.assistant_offered_levels(&connection).contains(&level) {
            self.assistant.access_error = Some(format!(
                "\u{201C}{}\u{201D} is not available for this connection.",
                level.label()
            ));
            self.assistant_sync(cx);
            cx.notify();
            return;
        }
        self.assistant.access_busy = true;
        self.assistant.access_error = None;
        let records = Arc::clone(&self.assistant.records);
        self.assistant.access_task = Some(cx.spawn(async move |this, cx| {
            let id = connection.clone();
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let current = records.overrides_json(&id)?;
                    let next = overrides_with_level(&current, level);
                    records.save_overrides_json(&id, &next)?;
                    Ok::<_, RecordsError>(next)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.access_busy = false;
                this.assistant.access_task = None;
                match saved {
                    Ok(json) => {
                        let mut profile = this
                            .connection_policy
                            .profile(&connection)
                            .cloned()
                            .unwrap_or_default();
                        profile.overrides = engine_connect::profile::Overrides::from_json(&json);
                        this.connection_policy.set_profile(&connection, profile);
                        this.revoke_agent_access(&connection);
                    }
                    Err(error) => {
                        let message = format!("Could not change the agent access: {error}.");
                        this.report_error(Area::Connect, Some(&connection), &message);
                        this.assistant.access_error = Some(message);
                    }
                }
                this.assistant_sync(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Revokes the agent session of every connection whose effective agent access is lower under
    /// the settings now than under `old` (Settings changed a connection type).
    pub(in crate::app) fn revoke_agent_access_lowered_since(
        &self,
        old: &crate::settings::Settings,
    ) {
        if !self.assistant.layers.exists() {
            return;
        }
        let lowered: Vec<String> = self
            .connection_policy
            .profiles()
            .filter(|(_, profile)| {
                self.settings.for_connection(profile).agent_access
                    < old.for_connection(profile).agent_access
            })
            .map(|(id, _)| id)
            .collect();
        self.assistant.revoke_all(lowered);
    }

    /// The levels the chooser may offer for the chat's connection: the ones the connection's type
    /// grants as asked (`AgentAccess::effective` caps a read-only type at `read`, so "Read and
    /// write" is not offered there).
    pub(super) fn assistant_offered_levels(&self, connection: &str) -> Vec<AgentLevel> {
        let type_id = self
            .connection_policy
            .profile(connection)
            .map(|profile| profile.type_id.clone())
            .unwrap_or_default();
        let info = engine_connect::profile::ConnectionType::agent_info_in(
            &self.settings.connection_types,
            &type_id,
        );
        [AgentLevel::Schema, AgentLevel::Read, AgentLevel::ReadWrite]
            .into_iter()
            .filter(|level| {
                engine_agent::AgentAccess::effective(Some(level.access()), &info) == level.access()
            })
            .collect()
    }

    /// Loads the selected provider's models off the UI thread and merges them into that provider's
    /// picker list. One request per provider: another provider's Refresh is not blocked by it, and
    /// its answer is only ever put into its own provider's list.
    pub(super) fn assistant_refresh_models(&mut self, ai: AiSettings, cx: &mut Context<Self>) {
        let which = ai.provider;
        if self.assistant.models_refreshing(which) {
            return;
        }
        self.assistant.models_errors.remove(&which);
        let provider = match self.assistant.provider(&ai) {
            Ok(provider) => provider,
            Err(error) => {
                self.assistant
                    .models_errors
                    .insert(which, format!("The address cannot be used: {error}."));
                cx.notify();
                return;
            }
        };
        // The request has a thread of its own and a token: it can be cancelled (the Settings
        // page closing), and it does not hold a thread of the shared background pool.
        let cancel = CancelToken::new();
        let (done, answer) = smol::channel::bounded(1);
        let started = std::thread::Builder::new()
            .name("assistant-models".into())
            .spawn({
                let cancel = cancel.clone();
                move || {
                    let _ = done.send_blocking(provider.list_models(&cancel));
                }
            });
        if let Err(error) = started {
            self.assistant
                .models_errors
                .insert(which, format!("Could not start the request: {error}."));
            cx.notify();
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(listed) = answer.recv().await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.assistant.model_requests.remove(&which);
                match listed {
                    Ok(models) => {
                        this.assistant
                            .fetched_models
                            .insert(which, models.into_iter().map(|model| model.id).collect());
                        this.assistant.models_version += 1;
                    }
                    Err(error) => {
                        this.assistant
                            .models_errors
                            .insert(which, ChatError::from_provider(&error).message);
                    }
                }
                cx.notify();
            });
        });
        self.assistant.model_requests.insert(
            which,
            ModelRequest {
                _task: task,
                cancel,
            },
        );
        cx.notify();
    }

    /// The Settings page that asked for the model lists is gone: stop the requests (never blocks).
    pub(super) fn assistant_cancel_models_refresh(&mut self) {
        for (_, request) in self.assistant.model_requests.drain() {
            request.cancel.cancel_detached();
        }
    }
}

/// A model list request in flight: dropping it drops the task that would deliver the answer.
struct ModelRequest {
    _task: gpui::Task<()>,
    cancel: CancelToken,
}

/// `overrides_json` with the connection's own agent level set; every other override stays.
fn overrides_with_level(overrides_json: &str, level: AgentLevel) -> String {
    use engine_connect::profile::{OverrideValue, Overrides, AGENT_ACCESS};
    let mut overrides = Overrides::from_json(overrides_json);
    overrides.set(AGENT_ACCESS, OverrideValue::Text(level.id().to_owned()));
    overrides.to_json()
}
