//! The Settings tab: left navigation, a search box over every setting, and the
//! rows of the chosen page. Edits go to a *draft*; Apply copies the draft into
//! the app (`apply_settings`), Apply and Close does that and closes the tab,
//! and closing with pending changes asks first. Every row is drawn from its
//! `SettingDescriptor`, with the official controls (Switch, Select, Input,
//! NumberInput, Button).

use super::*;
use crate::agent_access::{agent_select, AgentChoice};
use crate::settings::connection::{self, AgentLevel, ConnectionType, PresetFlag};
use crate::settings::registry::{self, Kind, Section, SettingDescriptor, Value};
use crate::settings::Settings;
use gpui::{Entity, SharedString, Subscription};
use gpui_component::input::{InputEvent, NumberInput};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::IndexPath;

const NAV_WIDTH: f32 = 208.0;
const CONTROL_WIDTH: f32 = 220.0;
const NUMBER_WIDTH: f32 = 150.0;
const RESET_SLOT: f32 = 28.0;
/// The label of the empty font family (the inherited / platform font).
const DEFAULT_FONT_LABEL: &str = "Default";

/// A page of the Settings tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Section(Section),
    Shortcuts,
    Drivers,
    About,
}

impl Page {
    const ALL: [Self; 9] = [
        Self::Section(Section::Appearance),
        Self::Section(Section::Editor),
        Self::Section(Section::Results),
        Self::Section(Section::Query),
        Self::Section(Section::Navigation),
        Self::Section(Section::Ai),
        Self::Shortcuts,
        Self::Drivers,
        Self::About,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Section(section) => section.title(),
            Self::Shortcuts => "Shortcuts",
            Self::Drivers => "Drivers",
            Self::About => "About",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Section(section) => section.key(),
            Self::Shortcuts => "shortcuts",
            Self::Drivers => "drivers",
            Self::About => "about",
        }
    }
}

type ChoiceSelect = Entity<SelectState<Vec<SharedString>>>;

/// The name and color inputs of the connection type being edited. They are
/// rebuilt when another type is picked, which drops their subscriptions.
struct TypeEditor {
    id: String,
    name: Entity<InputState>,
    color: Entity<InputState>,
    /// The type's default agent access (never above Read).
    agent: Entity<SelectState<Vec<AgentChoice>>>,
    _subscriptions: Vec<Subscription>,
}

/// What a capture scenario opens the Settings tab with.
pub(super) struct ScenarioSeed {
    pub(super) draft: Settings,
    page: Page,
    shortcut_errors: Vec<(&'static str, String)>,
}

/// A shortcut being recorded: the next keystroke becomes its chord. The
/// interceptor swallows keys (so a bound chord does not run its action)
/// until the chord is taken, Escape cancels, or the tab is left.
struct Recording {
    id: &'static str,
    /// The chord slot to replace; `None` adds one.
    slot: Option<usize>,
    _intercept: Subscription,
}

/// The draft and the control entities of an open Settings tab.
pub(super) struct SettingsPage {
    pub(super) draft: Settings,
    page: Page,
    search: Entity<InputState>,
    query: String,
    /// Number and text inputs by setting id.
    inputs: HashMap<&'static str, Entity<InputState>>,
    /// Choice and font selects by setting id.
    selects: HashMap<&'static str, ChoiceSelect>,
    /// Invalid typed values by setting id; the draft keeps the last valid one.
    errors: HashMap<&'static str, String>,
    /// Controls whose text must be rewritten from the draft (after a reset).
    resync: HashSet<&'static str>,
    /// The row a search result jumped to.
    highlight: Option<&'static str>,
    /// Id of the connection type shown in the editor (Query page).
    selected_type: String,
    type_editor: Option<TypeEditor>,
    recording: Option<Recording>,
    /// Why the last change to a shortcut was refused, by action id.
    shortcut_errors: HashMap<&'static str, String>,
    scroll: ScrollHandle,
    pub(super) focus: FocusHandle,
    /// The AI page's key input and picker state (see `settings_ai`).
    pub(super) ai: super::settings_ai::AiPage,
    fonts: Vec<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    fn new(saved: &Settings, window: &mut Window, cx: &mut Context<AppRoot>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this: &mut AppRoot,
             input: &Entity<InputState>,
             event: &InputEvent,
             window: &mut Window,
             cx: &mut Context<AppRoot>| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.settings_search_changed(text, cx);
                }
                InputEvent::PressEnter { .. } => this.settings_jump_to_first_result(window, cx),
                _ => {}
            },
        );
        let mut fonts: Vec<SharedString> = vec![DEFAULT_FONT_LABEL.into()];
        fonts.extend(
            cx.text_system()
                .all_font_names()
                .into_iter()
                .map(SharedString::from),
        );
        Self {
            draft: saved.clone(),
            page: Page::Section(Section::Appearance),
            search,
            query: String::new(),
            inputs: HashMap::new(),
            selects: HashMap::new(),
            errors: HashMap::new(),
            resync: HashSet::new(),
            highlight: None,
            selected_type: String::new(),
            type_editor: None,
            recording: None,
            shortcut_errors: HashMap::new(),
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            ai: Default::default(),
            fonts,
            _subscriptions: vec![subscription],
        }
    }

    /// The control of setting `id` is rewritten from the draft on its next paint.
    pub(super) fn request_resync(&mut self, id: &'static str) {
        self.errors.remove(id);
        self.resync.insert(id);
    }

    /// Edits the draft hold, the saved settings differ from.
    pub(super) fn is_dirty(&self, saved: &Settings) -> bool {
        self.draft != *saved
    }

    pub(super) fn has_errors(&self) -> bool {
        !self.errors.is_empty()
            || !registry::cross_field_errors(&self.draft).is_empty()
            || !connection::type_errors(&self.draft.connection_types).is_empty()
    }

    /// The type the editor shows: the picked one, else the first.
    fn selected_connection_type(&self) -> Option<&ConnectionType> {
        let types = &self.draft.connection_types;
        types
            .iter()
            .find(|ty| ty.id == self.selected_type)
            .or_else(|| types.first())
    }

    /// The saved settings moved from `old` to `new` behind the page's back
    /// (a sidebar toggle): untouched draft values follow, edited ones stay.
    pub(super) fn rebase(&mut self, old: &Settings, new: &Settings) {
        for id in registry::changed_ids(old, new) {
            let Some(descriptor) = registry::find(id) else {
                continue;
            };
            if descriptor.get(&self.draft) == descriptor.get(old) {
                let _ = descriptor.set(&mut self.draft, descriptor.get(new));
                self.errors.remove(id);
                self.resync.insert(id);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn is_on_section(&self, section: Section) -> bool {
        self.page == Page::Section(section)
    }

    #[cfg(test)]
    pub(super) fn highlighted(&self) -> Option<&'static str> {
        self.highlight
    }

    #[cfg(test)]
    pub(super) fn search_text(&self, cx: &gpui::App) -> String {
        self.search.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(super) fn shortcut_error(&self, id: &str) -> Option<String> {
        self.shortcut_errors.get(id).cloned()
    }

    #[cfg(test)]
    pub(super) fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// The level the open connection type's Agent access select shows (tests).
    #[cfg(test)]
    pub(super) fn type_agent_for_test(&self, cx: &gpui::App) -> Option<AgentLevel> {
        self.type_editor
            .as_ref()?
            .agent
            .read(cx)
            .selected_value()
            .copied()
    }

    #[cfg(test)]
    pub(super) fn select_for_test(&self, id: &str) -> ChoiceSelect {
        self.selects
            .get(id)
            .expect("a select for the setting")
            .clone()
    }

    /// What the text box of setting `id` shows (tests).
    #[cfg(test)]
    pub(super) fn input_text_for_test(&self, id: &str, cx: &gpui::App) -> String {
        self.inputs
            .get(id)
            .expect("an input for the setting")
            .read(cx)
            .value()
            .to_string()
    }

    fn rows(&self) -> Vec<&'static SettingDescriptor> {
        if !self.query.trim().is_empty() {
            return registry::search(&self.query);
        }
        match self.page {
            Page::Section(section) => registry::visible(section).collect(),
            Page::Shortcuts | Page::Drivers | Page::About => Vec::new(),
        }
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        let text = format!("{value:.3}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

impl AppRoot {
    // ---- opening and closing --------------------------------------------------

    /// Title-bar gear, `secondary-,` and the palette.
    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.open_settings_tab();
        self.ensure_settings_page(window, cx);
        if let Some(page) = self.settings_ui.page.as_ref() {
            window.focus(&page.focus, cx);
        }
        cx.notify();
    }

    /// "Open Settings -> AI": the assistant's missing-key and rejected-key states link here.
    pub(super) fn open_ai_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
        self.settings_go_to_page(Page::Section(Section::Ai), cx);
    }

    /// Creates the draft and controls of the page on first use.
    fn ensure_settings_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_ui.page.is_some() {
            return;
        }
        let mut page = SettingsPage::new(&self.settings, window, cx);
        if let Some(seed) = self.settings_ui.scenario_seed.take() {
            page.draft = seed.draft;
            page.page = seed.page;
            page.shortcut_errors.extend(seed.shortcut_errors);
        }
        self.settings_ui.page = Some(page);
    }

    /// The Query & Connections page on the Development type with `level` as its default agent
    /// access (offscreen captures).
    #[cfg(all(test, feature = "bench"))]
    pub(super) fn show_connection_types_for_capture(
        &mut self,
        level: AgentLevel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(window, cx);
        if let Some(page) = self.settings_ui.page.as_mut() {
            page.page = Page::Section(Section::Query);
            page.selected_type = connection::DEVELOPMENT.into();
            if let Some(ty) = page
                .draft
                .connection_types
                .iter_mut()
                .find(|ty| ty.id == connection::DEVELOPMENT)
            {
                ty.agent_access = level;
            }
            page.type_editor = None;
        }
        cx.notify();
    }

    /// The open type's Agent access select, once rendered (offscreen captures open its dropdown).
    #[cfg(all(test, feature = "bench"))]
    pub(super) fn type_agent_select_for_capture(
        &self,
    ) -> Option<Entity<SelectState<Vec<AgentChoice>>>> {
        self.settings_ui
            .page
            .as_ref()?
            .type_editor
            .as_ref()
            .map(|editor| editor.agent.clone())
    }

    /// `--ui-scenario settings`: the Editor page with unapplied edits.
    pub(super) fn seed_settings_scenario(&mut self) {
        self.workspace.open_settings_tab();
        let mut draft = (*self.settings).clone();
        draft.editor.soft_wrap = true;
        draft.editor.tab_size = 4;
        draft.editor.run_scope = crate::settings::RunScope::Current;
        self.settings_ui.scenario_seed = Some(ScenarioSeed {
            draft,
            page: Page::Section(Section::Editor),
            shortcut_errors: Vec::new(),
        });
    }

    /// `--ui-scenario settings-ai`: the AI Assistant page with a saved key and unapplied edits.
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) fn seed_settings_ai_scenario(&mut self) {
        self.workspace.open_settings_tab();
        let mut draft = (*self.settings).clone();
        draft.ai.selected_mut().model = "claude-sonnet-5-5".into();
        draft.ai.effort = engine_ai::Effort::High;
        // The in-memory key store of a scenario; the page then shows "Saved".
        let _ = self.assistant.keys.save("sk-ant-scenario-key");
        self.assistant.set_key_state(
            crate::settings::AiProvider::Anthropic,
            super::assistant::KeyState::Set,
        );
        // Conversations on disk (a temporary folder): the delete button is on.
        self.seed_conversation_store();
        self.settings_ui.scenario_seed = Some(ScenarioSeed {
            draft,
            page: Page::Section(Section::Ai),
            shortcut_errors: Vec::new(),
        });
    }

    /// `--ui-scenario settings-ai-ollama`: the AI Assistant page with Ollama selected, a model
    /// chosen and no key (it is optional there).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(super) fn seed_settings_ai_ollama_scenario(&mut self) {
        self.workspace.open_settings_tab();
        let mut draft = (*self.settings).clone();
        draft.ai.provider = crate::settings::AiProvider::Ollama;
        draft.ai.selected_mut().model = "qwen3:8b".into();
        // The in-memory key store of a scenario holds nothing for Ollama: "Not set (optional)".
        self.assistant.set_key_state(
            crate::settings::AiProvider::Ollama,
            super::assistant::KeyState::NotSet,
        );
        self.seed_conversation_store();
        self.settings_ui.scenario_seed = Some(ScenarioSeed {
            draft,
            page: Page::Section(Section::Ai),
            shortcut_errors: Vec::new(),
        });
    }

    /// `--ui-scenario settings-shortcuts`: the Shortcuts page with unapplied
    /// edits and a refused recording.
    pub(super) fn seed_settings_shortcuts_scenario(&mut self) {
        self.workspace.open_settings_tab();
        let mut draft = (*self.settings).clone();
        draft
            .shortcuts
            .insert("workspace.toggle_error_log".into(), Vec::new());
        draft.shortcuts.insert(
            "workspace.open_settings".into(),
            vec!["secondary-shift-o".into()],
        );
        draft.shortcuts.insert(
            "workspace.open_command_palette".into(),
            vec!["secondary-k".into(), "secondary-shift-p".into()],
        );
        self.settings_ui.scenario_seed = Some(ScenarioSeed {
            draft,
            page: Page::Shortcuts,
            shortcut_errors: vec![(
                "app.quit",
                "Already used by \u{201C}Close Tab\u{201D}".to_owned(),
            )],
        });
    }

    fn settings_tab_id(&self) -> Option<u64> {
        self.workspace
            .tabs()
            .iter()
            .find(|tab| tab.kind == TabKind::Settings)
            .map(|tab| tab.id)
    }

    /// Pending (unapplied) edits exist.
    pub(super) fn settings_dirty(&self) -> bool {
        self.settings_ui
            .page
            .as_ref()
            .is_some_and(|page| page.is_dirty(&self.settings))
    }

    /// Close button, Escape and the tab's ✕: asks first when edits are pending.
    pub(super) fn request_close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab_id) = self.settings_tab_id() else {
            return;
        };
        self.confirm_close_tab(tab_id, window, cx);
    }

    /// "Discard changes?" with Discard / Cancel.
    pub(super) fn open_discard_settings_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .width(px(440.0))
                .title("Discard changes?")
                .child(
                    div()
                        .debug_selector(|| "settings-discard-body".into())
                        .child("Some settings were changed but not applied."),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Discard"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "settings-discard-cancel".into())
                                .child(
                                    gpui_component::dialog::DialogClose::new()
                                        .trigger(|button| button.label("Cancel")),
                                ),
                        )
                        .child(
                            div()
                                .debug_selector(|| "settings-discard-confirm".into())
                                .child(gpui_component::dialog::DialogAction::new().child(
                                    Button::new("settings-discard").label("Discard").danger(),
                                )),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| {
                        this.close_tab(tab_id, cx);
                        cx.notify();
                    });
                    true
                })
        });
    }

    // ---- draft edits ----------------------------------------------------------

    fn settings_page_mut(&mut self) -> Option<&mut SettingsPage> {
        self.settings_ui.page.as_mut()
    }

    /// Sets one draft value from a typed control; an invalid value is kept as
    /// an inline error and the draft keeps the last valid one.
    pub(super) fn settings_set(&mut self, id: &'static str, value: Value, cx: &mut Context<Self>) {
        let Some(descriptor) = registry::find(id) else {
            return;
        };
        let Some(page) = self.settings_page_mut() else {
            return;
        };
        match descriptor.set(&mut page.draft, value) {
            Ok(()) => {
                page.errors.remove(id);
            }
            Err(error) => {
                page.errors.insert(id, error.to_string());
            }
        }
        self.notify_pane_views(cx);
    }

    fn settings_edit_text(&mut self, id: &'static str, text: String, cx: &mut Context<Self>) {
        let Some(descriptor) = registry::find(id) else {
            return;
        };
        let value = match descriptor.kind {
            Kind::Number { .. } => match text.trim().parse::<f64>() {
                Ok(number) => Value::Number(number),
                Err(_) => {
                    if let Some(page) = self.settings_page_mut() {
                        page.errors.insert(id, "Enter a number".into());
                    }
                    self.notify_pane_views(cx);
                    return;
                }
            },
            Kind::Model => Value::Text(text.trim().to_owned()),
            _ => Value::Text(text),
        };
        self.settings_set(id, value, cx);
    }

    /// A Select was confirmed: `label` is an option label or a font family.
    fn settings_choose(&mut self, id: &'static str, label: &str, cx: &mut Context<Self>) {
        let Some(descriptor) = registry::find(id) else {
            return;
        };
        // The formatter tool can be chosen once it is installed.
        if id == "editor.formatter"
            && label == <crate::settings::FormatterChoice as crate::settings::Choice>::label(crate::settings::FormatterChoice::Sqruff)
            && self.formatter_tool_missing()
        {
            if let Some(page) = self.settings_page_mut() {
                page.errors.insert(id, "Install SQL Formatter (sqruff) from the Driver Manager first".into());
            }
            self.notify_pane_views(cx);
            return;
        }
        let value = match &descriptor.kind {
            Kind::Choice { options } => options
                .iter()
                .find(|(_, option_label)| *option_label == label)
                .map(|(option, _)| Value::Choice(option)),
            Kind::Font => Some(Value::Text(if label == DEFAULT_FONT_LABEL {
                String::new()
            } else {
                label.to_owned()
            })),
            Kind::Model => Some(Value::Text(label.to_owned())),
            _ => None,
        };
        if let Some(value) = value {
            self.settings_set(id, value, cx);
        }
        if id == "ai.provider" {
            self.settings_provider_changed();
        } else if id == "ai.model" {
            // Picked from the list: the typed id box shows it.
            if let Some(page) = self.settings_page_mut() {
                page.resync.insert(id);
            }
        }
    }

    /// The Model and Base URL rows now show the entry of the provider just selected: a typed error
    /// belonged to the old one, and the address box takes the new text.
    fn settings_provider_changed(&mut self) {
        if let Some(page) = self.settings_page_mut() {
            for id in ["ai.model", "ai.base_url"] {
                page.errors.remove(id);
                page.resync.insert(id);
            }
            // A key typed for the old provider must not be saved as the new one's.
            page.ai.key_input = None;
        }
    }

    pub(super) fn settings_reset_one(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(descriptor) = registry::find(id) else {
            return;
        };
        if let Some(page) = self.settings_page_mut() {
            descriptor.reset(&mut page.draft);
            page.errors.remove(id);
            page.resync.insert(id);
        }
        if id == "ai.provider" {
            self.settings_provider_changed();
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn settings_reset_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            registry::reset_section(&mut page.draft, section);
            if section == Section::Query {
                page.draft.connection_types = ConnectionType::builtins();
                page.type_editor = None;
            }
            for descriptor in registry::visible(section) {
                page.errors.remove(descriptor.id);
                page.resync.insert(descriptor.id);
            }
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn settings_reset_defaults(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            registry::reset_all(&mut page.draft);
            page.draft.shortcuts.clear();
            page.shortcut_errors.clear();
            page.recording = None;
            page.draft.connection_types = ConnectionType::builtins();
            page.type_editor = None;
            page.errors.clear();
            page.resync
                .extend(registry::registry().iter().map(|descriptor| descriptor.id));
        }
        self.notify_pane_views(cx);
    }

    /// Apply: the draft becomes the app's settings. Refused while a value is
    /// invalid. Returns whether it was applied.
    pub(super) fn settings_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(page) = self.settings_ui.page.as_ref() else {
            return false;
        };
        if page.has_errors() {
            return false;
        }
        let draft = page.draft.clone();
        self.apply_settings(draft, Some(window), cx);
        self.notify_pane_views(cx);
        true
    }

    /// Apply and Close.
    pub(super) fn settings_apply_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_apply(window, cx) {
            return;
        }
        if let Some(tab_id) = self.settings_tab_id() {
            self.close_tab(tab_id, cx);
            cx.notify();
        }
    }

    // ---- search and navigation --------------------------------------------------

    fn settings_search_changed(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            // Clearing the box (a jump, a nav click) keeps the highlight and
            // the scroll position; a new query starts over.
            if !text.trim().is_empty() {
                page.highlight = None;
                page.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            }
            page.query = text;
        }
        self.notify_pane_views(cx);
    }

    /// Enter in the search box: jump to the first result.
    fn settings_jump_to_first_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = self
            .settings_ui
            .page
            .as_ref()
            .and_then(|page| registry::search(&page.query).first().map(|d| d.id));
        if let Some(id) = first {
            self.settings_jump_to(id, window, cx);
        }
    }

    /// Opens the page of setting `id`, clears the search and highlights the row.
    pub(super) fn settings_jump_to(
        &mut self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(descriptor) = registry::find(id) else {
            return;
        };
        self.settings_clear_search(window, cx);
        let Some(page) = self.settings_page_mut() else {
            return;
        };
        page.page = Page::Section(descriptor.section);
        page.highlight = Some(id);
        page.scroll.scroll_to_item(flat_index(descriptor));
        self.notify_pane_views(cx);
    }

    fn settings_go_to_page(&mut self, target: Page, cx: &mut Context<Self>) {
        // What is installed may have changed in the Driver Manager since the page was last seen.
        if target == Page::Section(Section::Editor) {
            self.scan_format_tools(cx);
        }
        if let Some(page) = self.settings_page_mut() {
            page.recording = None;
            page.page = target;
            page.highlight = None;
            page.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        }
        self.notify_pane_views(cx);
    }

    // ---- controls ---------------------------------------------------------------

    /// Creates the control entities of `rows` that do not exist yet and
    /// rewrites the ones a reset touched.
    fn ensure_setting_controls(
        &mut self,
        rows: &[&'static SettingDescriptor],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.settings_ui.page.as_mut() else {
            return;
        };
        let mut new_subscriptions = Vec::new();
        for &descriptor in rows {
            let id = descriptor.id;
            let value = descriptor.get(&page.draft);
            match (&descriptor.kind, &value) {
                (Kind::Number { min, max, step }, Value::Number(number)) => {
                    let input = match page.inputs.get(id) {
                        Some(input) => input.clone(),
                        None => {
                            let (min, max, step, text) =
                                (*min, *max, *step, format_number(*number));
                            let input = cx.new(|cx| {
                                InputState::new(window, cx)
                                    .default_value(text)
                                    .step(step)
                                    .min(min)
                                    .max(max)
                            });
                            new_subscriptions.push(subscribe_text(&input, id, window, cx));
                            page.inputs.insert(id, input.clone());
                            input
                        }
                    };
                    if page.resync.remove(id) {
                        let text = format_number(*number);
                        input.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                }
                (Kind::Text { .. }, Value::Text(text)) => {
                    let input = match page.inputs.get(id) {
                        Some(input) => input.clone(),
                        None => {
                            let text = text.clone();
                            let input =
                                cx.new(|cx| InputState::new(window, cx).default_value(text));
                            new_subscriptions.push(subscribe_text(&input, id, window, cx));
                            page.inputs.insert(id, input.clone());
                            input
                        }
                    };
                    if page.resync.remove(id) {
                        let text = text.clone();
                        input.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                }
                (Kind::Choice { options }, Value::Choice(choice)) => {
                    let labels: Vec<SharedString> = options
                        .iter()
                        .map(|(_, label)| SharedString::from(*label))
                        .collect();
                    let current = options
                        .iter()
                        .find(|(option, _)| option == choice)
                        .map(|(_, label)| SharedString::from(*label));
                    ensure_select(
                        page,
                        id,
                        labels,
                        current,
                        false,
                        window,
                        cx,
                        &mut new_subscriptions,
                    );
                }
                (Kind::Model, Value::Text(model)) => {
                    let provider = page.draft.ai.provider;
                    // Anthropic's row is the list alone. The others type the id (a server may not
                    // list its models), with the list as a helper to pick from.
                    let typed = provider.default_model().is_empty();
                    let mut labels: Vec<SharedString> = self
                        .assistant
                        .models_for(provider)
                        .into_iter()
                        .map(SharedString::from)
                        .collect();
                    // A provider that has no model chosen yet shows none.
                    let current =
                        (!model.trim().is_empty()).then(|| SharedString::from(model.clone()));
                    if typed {
                        let input = match page.inputs.get(id) {
                            Some(input) => input.clone(),
                            None => {
                                let text = model.clone();
                                let input = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .default_value(text)
                                        .placeholder("Model id")
                                });
                                new_subscriptions.push(subscribe_text(&input, id, window, cx));
                                page.inputs.insert(id, input.clone());
                                input
                            }
                        };
                        if page.resync.remove(id) {
                            let text = model.clone();
                            input.update(cx, |state, cx| state.set_value(text, window, cx));
                        }
                    } else if let Some(current) =
                        current.as_ref().filter(|current| !labels.contains(current))
                    {
                        // A model typed into the settings file stays selectable.
                        labels.push(current.clone());
                    }
                    // The helper shows the typed id only when the list has it.
                    let current = current.filter(|current| labels.contains(current));
                    // The list is the provider's own: reload it when the provider or its models changed.
                    let seen = (provider, self.assistant.models_version);
                    let reload = page.ai.models_seen != seen;
                    page.ai.models_seen = seen;
                    ensure_select(
                        page,
                        id,
                        labels.clone(),
                        current.clone(),
                        true,
                        window,
                        cx,
                        &mut new_subscriptions,
                    );
                    if reload {
                        if let Some(select) = page.selects.get(id) {
                            select.update(cx, |state, cx| {
                                state.set_items(labels, window, cx);
                                match &current {
                                    Some(current) => state.set_selected_value(current, window, cx),
                                    None => state.set_selected_index(None, window, cx),
                                }
                            });
                        }
                    }
                }
                (Kind::Font, Value::Text(family)) => {
                    let mut labels = page.fonts.clone();
                    let current = if family.is_empty() {
                        SharedString::from(DEFAULT_FONT_LABEL)
                    } else {
                        SharedString::from(family.clone())
                    };
                    if !labels.contains(&current) {
                        labels.push(current.clone());
                    }
                    ensure_select(
                        page,
                        id,
                        labels,
                        Some(current),
                        true,
                        window,
                        cx,
                        &mut new_subscriptions,
                    );
                }
                _ => {}
            }
        }
        page._subscriptions.extend(new_subscriptions);
        // What is left (a row that is not on this page) waits for the page it is on: the rows that
        // use `resync` drop their own entry.
    }

    fn render_setting_control(
        &self,
        descriptor: &'static SettingDescriptor,
        value: &Value,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = descriptor.id;
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        match (&descriptor.kind, value) {
            (Kind::Toggle, Value::Bool(checked)) => div()
                .debug_selector(move || format!("setting-control-{id}"))
                .child(
                    Switch::new(SharedString::from(format!("setting-switch-{id}")))
                        .checked(*checked)
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            this.settings_set(id, Value::Bool(*checked), cx);
                        })),
                )
                .into_any_element(),
            (Kind::Number { .. }, _) => match page.inputs.get(id) {
                Some(input) => div()
                    .debug_selector(move || format!("setting-control-{id}"))
                    .w(px(NUMBER_WIDTH))
                    .child(NumberInput::new(input).small())
                    .into_any_element(),
                None => div().into_any_element(),
            },
            (Kind::Text { .. }, _) => match page.inputs.get(id) {
                Some(input) => div()
                    .debug_selector(move || format!("setting-control-{id}"))
                    .w(px(CONTROL_WIDTH))
                    .child(Input::new(input).small())
                    .into_any_element(),
                None => div().into_any_element(),
            },
            // The other providers' model: the id typed, and the list to pick it from.
            (Kind::Model, _) if page.draft.ai.provider.default_model().is_empty() => {
                match (page.inputs.get(id), page.selects.get(id)) {
                    (Some(input), Some(select)) => div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .debug_selector(move || format!("setting-control-{id}"))
                                .w(px(CONTROL_WIDTH))
                                .child(Input::new(input).small()),
                        )
                        .child(
                            div()
                                .debug_selector(move || format!("setting-pick-{id}"))
                                .w(px(CONTROL_WIDTH))
                                .child(
                                    Select::new(select)
                                        .small()
                                        .w(px(CONTROL_WIDTH))
                                        .placeholder("Pick from the list")
                                        .search_placeholder("Search models")
                                        .menu_max_h(px(320.0)),
                                ),
                        )
                        .into_any_element(),
                    _ => div().into_any_element(),
                }
            }
            (Kind::Choice { .. } | Kind::Font | Kind::Model, _) => match page.selects.get(id) {
                Some(select) => div()
                    .debug_selector(move || format!("setting-control-{id}"))
                    .w(px(CONTROL_WIDTH))
                    .child(
                        Select::new(select)
                            .small()
                            .w(px(CONTROL_WIDTH))
                            .search_placeholder(if matches!(descriptor.kind, Kind::Model) {
                                "Search models"
                            } else {
                                "Search fonts"
                            })
                            .when(matches!(descriptor.kind, Kind::Model), |select| {
                                select.placeholder("Choose a model")
                            })
                            .menu_max_h(px(320.0)),
                    )
                    .into_any_element(),
                None => div().into_any_element(),
            },
            _ => div().into_any_element(),
        }
    }

    fn render_setting_row(
        &self,
        descriptor: &'static SettingDescriptor,
        breadcrumb: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = descriptor.id;
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let value = descriptor.get(&page.draft);
        let modified = value != descriptor.saved_value_for(&self.settings, &page.draft);
        let not_default = value != descriptor.default_in(&page.draft);
        let unavailable = descriptor.unavailable_reason(&page.draft);
        let highlighted = page.highlight == Some(id);
        let error = page.errors.get(id).cloned().or_else(|| {
            registry::cross_field_errors(&page.draft)
                .into_iter()
                .find(|(error_id, _)| *error_id == id)
                .map(|(_, message)| message.to_owned())
        });
        let control = self.render_setting_control(descriptor, &value, unavailable.is_some(), cx);
        div()
            .id(SharedString::from(format!("setting-row-{id}")))
            .debug_selector(move || format!("setting-{id}"))
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if highlighted {
                theme::accent()
            } else {
                gpui::rgba(0x0000_0000)
            })
            .when(highlighted, |this| this.bg(theme::accent_surface()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .debug_selector(move || format!("setting-modified-{id}"))
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .when(modified, |this| this.bg(theme::accent())),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme::text())
                                    .child(descriptor.title),
                            )
                            .when(breadcrumb, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!("setting-goto-{id}")))
                                        .debug_selector(move || format!("setting-goto-{id}"))
                                        .label(format!(
                                            "{} \u{203A} {}",
                                            descriptor.section.title(),
                                            descriptor.group
                                        ))
                                        .compact()
                                        .ghost()
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.settings_jump_to(id, window, cx);
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(descriptor.description),
                    )
                    .when_some(unavailable, |this, reason| {
                        this.child(
                            div()
                                .debug_selector(move || format!("setting-unavailable-{id}"))
                                .text_size(px(12.0))
                                .text_color(theme::text_faint())
                                .child(reason),
                        )
                    })
                    .when_some(error, |this, message| {
                        this.child(
                            div()
                                .debug_selector(move || format!("setting-error-{id}"))
                                .text_size(px(12.0))
                                .text_color(theme::danger())
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_shrink_0()
                    .child(control)
                    .child(
                        div()
                            .w(px(RESET_SLOT))
                            .flex_shrink_0()
                            .when(not_default, |this| {
                                this.child(
                                    Button::new(SharedString::from(format!("setting-reset-{id}")))
                                        .debug_selector(move || format!("setting-reset-{id}"))
                                        .icon(Icon::empty().path("icons/undo-2.svg").size(px(14.0)))
                                        .compact()
                                        .ghost()
                                        .tooltip("Reset to default")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.settings_reset_one(id, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let searching = !page.query.trim().is_empty();
        div()
            .debug_selector(|| "settings-nav".into())
            .w(px(NAV_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .border_r_1()
            .border_color(theme::border())
            .bg(theme::sidebar())
            .child(
                div().debug_selector(|| "settings-search".into()).child(
                    Input::new(&page.search)
                        .small()
                        .prefix(Icon::empty().path("icons/search.svg").size(px(14.0)))
                        .cleanable(true),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(Page::ALL.into_iter().map(|target| {
                        let key = target.key();
                        Button::new(SharedString::from(format!("settings-nav-{key}")))
                            .debug_selector(move || format!("settings-nav-{key}"))
                            // The label fills the button so it reads from the left
                            // (a plain `.label()` is centered).
                            .child(
                                div()
                                    .debug_selector(move || format!("settings-nav-label-{key}"))
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_left()
                                    .child(target.title()),
                            )
                            .w_full()
                            .ghost()
                            .selected(!searching && page.page == target)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.settings_clear_search(window, cx);
                                this.settings_go_to_page(target, cx);
                            }))
                    })),
            )
            .into_any_element()
    }

    /// Empties the search box (a nav click leaves the results view).
    fn settings_clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.settings_page_mut() else {
            return;
        };
        if page.query.is_empty() {
            return;
        }
        page.query.clear();
        let search = page.search.clone();
        search.update(cx, |state, cx| state.set_value("", window, cx));
    }

    fn render_section_heading(
        &self,
        title: &'static str,
        section: Option<Section>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(18.0))
                    .text_color(theme::text())
                    .child(title),
            )
            .when_some(section, |this, section| {
                this.child(
                    Button::new(SharedString::from(format!(
                        "settings-reset-section-{}",
                        section.key()
                    )))
                    .debug_selector(move || format!("settings-reset-section-{}", section.key()))
                    .label("Reset section")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_reset_section(section, cx);
                    })),
                )
            })
            .into_any_element()
    }

    fn render_settings_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let rows = page.rows();
        let searching = !page.query.trim().is_empty();
        let target = page.page;
        let query = page.query.trim().to_owned();
        self.ensure_setting_controls(&rows, window, cx);
        if !searching && target == Page::Section(Section::Query) {
            self.ensure_type_editor(window, cx);
        }
        // The first time the Editor page is painted, look for the formatters (later visits look again).
        if !searching
            && target == Page::Section(Section::Editor)
            && self.format_ui.scan.is_none()
            && self.format_ui.scan_task.is_none()
        {
            self.scan_format_tools(cx);
        }
        let ai_page = !searching && target == Page::Section(Section::Ai);

        let mut content: Vec<AnyElement> = Vec::new();
        if searching {
            content.push(self.render_section_heading("Search results", None, cx));
            content.push(
                div()
                    .debug_selector(|| "settings-result-count".into())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(format!(
                        "{} for \u{201C}{query}\u{201D}",
                        match rows.len() {
                            1 => "1 setting".to_owned(),
                            count => format!("{count} settings"),
                        }
                    ))
                    .into_any_element(),
            );
            for descriptor in &rows {
                content.push(self.render_setting_row(descriptor, true, cx));
            }
            let found = self.matching_shortcuts(&query);
            if !found.is_empty() {
                content.push(
                    div()
                        .debug_selector(|| "settings-shortcut-results".into())
                        .pt(px(12.0))
                        .text_size(px(11.0))
                        .text_color(theme::text_faint())
                        .child(format!("SHORTCUTS ({})", found.len()))
                        .into_any_element(),
                );
                for def in found {
                    content.push(self.render_shortcut_row(def, cx));
                }
            }
        } else {
            match target {
                Page::Section(section) => {
                    content.push(self.render_section_heading(section.title(), Some(section), cx));
                    if ai_page {
                        content.push(
                            div()
                                .pt(px(12.0))
                                .text_size(px(11.0))
                                .text_color(theme::text_faint())
                                .child("API KEY")
                                .into_any_element(),
                        );
                        content.push(self.render_ai_key_row(window, cx));
                    }
                    let mut groups: Vec<(&'static str, Vec<&'static SettingDescriptor>)> =
                        Vec::new();
                    for descriptor in &rows {
                        match groups
                            .iter_mut()
                            .find(|(group, _)| *group == descriptor.group)
                        {
                            Some((_, members)) => members.push(descriptor),
                            None => groups.push((descriptor.group, vec![descriptor])),
                        }
                    }
                    for (group, descriptors) in groups {
                        content.push(
                            div()
                                .pt(px(12.0))
                                .text_size(px(11.0))
                                .text_color(theme::text_faint())
                                .child(group.to_uppercase())
                                .into_any_element(),
                        );
                        for descriptor in descriptors {
                            content.push(self.render_setting_row(descriptor, false, cx));
                        }
                        if section == Section::Ai && group == "Model" {
                            content.push(self.render_ai_models_row(cx));
                        }
                        if section == Section::Ai && group == "Conversations" {
                            content.push(self.render_ai_conversations_row(cx));
                        }
                        if section == Section::Editor && group == "Format SQL" {
                            content.push(self.render_format_tools_row(cx));
                        }
                    }
                    if section == Section::Query {
                        content.push(self.render_connection_types(cx));
                    }
                }
                Page::Shortcuts => content.extend(self.render_shortcuts_page(cx)),
                Page::Drivers => {
                    content.push(self.render_section_heading("Drivers", None, cx));
                    content.push(self.render_drivers_page(cx));
                }
                Page::About => {
                    content.push(self.render_section_heading("About", None, cx));
                    content.push(self.render_about_page());
                }
            }
        }
        let scroll = self
            .settings_ui
            .page
            .as_ref()
            .map(|page| page.scroll.clone())
            .unwrap_or_default();
        div()
            .id("settings-content")
            .debug_selector(|| "settings-content".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(20.0))
            .py(px(16.0))
            .track_scroll(&scroll)
            .overflow_y_scrollbar()
            .children(content)
            .into_any_element()
    }

    fn render_drivers_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let developer = self.driver_ui.developer_mode;
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(16.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(13.0)).text_color(theme::text()).child("Developer mode"))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme::text_muted())
                                    .child("Accept unsigned local drivers and manifests. Takes effect immediately; it is stored with the drivers, not in this Settings file."),
                            ),
                    )
                    .child(
                        div().debug_selector(|| "settings-developer-mode".into()).child(
                            Switch::new("settings-developer-switch")
                                .checked(developer)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.request_developer_mode(window, cx);
                                })),
                        ),
                    ),
            )
            .child(
                div().px(px(12.0)).py(px(6.0)).child(
                    Button::new("settings-open-driver-manager")
                        .debug_selector(|| "settings-open-driver-manager".into())
                        .label("Open Driver Manager")
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
                ),
            )
            .into_any_element()
    }

    fn render_about_page(&self) -> AnyElement {
        let path = self.settings_ui.path.as_ref().map_or_else(
            || "not saved (no config folder)".to_owned(),
            |path| path.display().to_string(),
        );
        let line = |label: &'static str, value: String| {
            div()
                .flex()
                .gap(px(12.0))
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(12.0))
                .child(
                    div()
                        .w(px(110.0))
                        .text_color(theme::text_muted())
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme::text())
                        .child(value),
                )
        };
        div()
            .debug_selector(|| "settings-about".into())
            .flex()
            .flex_col()
            .child(line("Version", env!("CARGO_PKG_VERSION").to_owned()))
            .child(line("Settings file", path))
            .into_any_element()
    }

    fn render_settings_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let dirty = page.is_dirty(&self.settings);
        let invalid = page.has_errors();
        div()
            .debug_selector(|| "settings-footer".into())
            .h(px(48.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .child(
                Button::new("settings-reset-defaults")
                    .debug_selector(|| "settings-reset-defaults".into())
                    .label("Reset Defaults")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.settings_reset_defaults(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.0))
                    .text_color(if invalid {
                        theme::danger()
                    } else {
                        theme::text_muted()
                    })
                    .debug_selector(|| "settings-status".into())
                    .child(if invalid {
                        "Fix the highlighted values to apply"
                    } else if dirty {
                        "Changes not applied"
                    } else {
                        ""
                    }),
            )
            .child(
                Button::new("settings-close")
                    .debug_selector(|| "settings-close".into())
                    .label("Close")
                    .compact()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.request_close_settings(window, cx)),
                    ),
            )
            .child(
                Button::new("settings-apply")
                    .debug_selector(|| "settings-apply".into())
                    .label("Apply")
                    .compact()
                    .disabled(!dirty || invalid)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.settings_apply(window, cx);
                    })),
            )
            .child(
                Button::new("settings-apply-close")
                    .debug_selector(|| "settings-apply-close".into())
                    .label("Apply and Close")
                    .compact()
                    .primary()
                    .disabled(invalid)
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.settings_apply_and_close(window, cx)
                        }),
                    ),
            )
            .into_any_element()
    }

    /// The content of the Settings tab.
    pub(super) fn render_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_settings_page(window, cx);
        let focus = self
            .settings_ui
            .page
            .as_ref()
            .map(|page| page.focus.clone())
            .expect("settings page created above");
        // A dismissed dialog (Discard changes?) leaves nothing focused; Escape
        // and the other page shortcuts need the page back in the focus path.
        if window.focused(cx).is_none() && !window.has_active_dialog(cx) {
            window.focus(&focus, cx);
        }
        let nav = self.render_settings_nav(cx);
        let content = self.render_settings_content(window, cx);
        let footer = self.render_settings_footer(cx);
        div()
            .key_context("settings")
            .track_focus(&focus)
            .debug_selector(|| "settings-page".into())
            .on_action(cx.listener(|this, _: &CloseSettings, window, cx| {
                this.request_close_settings(window, cx);
            }))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(div().flex().flex_1().min_h_0().child(nav).child(content))
            .child(footer)
            .into_any_element()
    }
}

// ---- connection types ------------------------------------------------------------

impl AppRoot {
    /// Builds the name / color inputs for the type the editor shows.
    fn ensure_type_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.settings_ui.page.as_ref() else {
            return;
        };
        let Some(ty) = page.selected_connection_type() else {
            return;
        };
        if page
            .type_editor
            .as_ref()
            .is_some_and(|editor| editor.id == ty.id)
        {
            return;
        }
        let (id, name, color, agent_level) = (
            ty.id.clone(),
            ty.name.clone(),
            ty.color.clone(),
            ty.agent_access,
        );
        let name_input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let color_input = cx.new(|cx| InputState::new(window, cx).default_value(color));
        let subscribe = |input: &Entity<InputState>,
                         edit: fn(&mut AppRoot, &str, String, &mut Context<AppRoot>),
                         window: &mut Window,
                         cx: &mut Context<AppRoot>| {
            let type_id = id.clone();
            cx.subscribe_in(
                input,
                window,
                move |this: &mut AppRoot, input: &Entity<InputState>, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        edit(this, &type_id, text, cx);
                    }
                },
            )
        };
        // A hand-edited `read_write` preset is shown as the Read it is capped to.
        let shown = agent_level.type_preset();
        let agent = agent_select(&AgentLevel::TYPE_PRESETS, Some(shown), window, cx);
        let agent_subscription = {
            let type_id = id.clone();
            cx.subscribe_in(
                &agent,
                window,
                move |this: &mut AppRoot, _, event: &SelectEvent<Vec<AgentChoice>>, _, cx| {
                    if let SelectEvent::Confirm(Some(level)) = event {
                        this.settings_type_set_agent(&type_id, *level, cx);
                    }
                },
            )
        };
        let subscriptions = vec![
            subscribe(&name_input, Self::settings_type_set_name, window, cx),
            subscribe(&color_input, Self::settings_type_set_color, window, cx),
            agent_subscription,
        ];
        if let Some(page) = self.settings_ui.page.as_mut() {
            page.type_editor = Some(TypeEditor {
                id,
                name: name_input,
                color: color_input,
                agent,
                _subscriptions: subscriptions,
            });
        }
    }

    fn settings_type_mut(&mut self, id: &str) -> Option<&mut ConnectionType> {
        self.settings_ui
            .page
            .as_mut()?
            .draft
            .connection_types
            .iter_mut()
            .find(|ty| ty.id == id)
    }

    fn settings_type_set_name(&mut self, id: &str, text: String, cx: &mut Context<Self>) {
        if let Some(ty) = self.settings_type_mut(id) {
            ty.name = text;
        }
        self.notify_pane_views(cx);
    }

    fn settings_type_set_color(&mut self, id: &str, text: String, cx: &mut Context<Self>) {
        if let Some(ty) = self.settings_type_mut(id) {
            ty.color = text.trim().to_ascii_lowercase();
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn settings_type_set_agent(
        &mut self,
        id: &str,
        level: AgentLevel,
        cx: &mut Context<Self>,
    ) {
        if let Some(ty) = self.settings_type_mut(id) {
            ty.agent_access = level;
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn settings_type_set_flag(
        &mut self,
        id: &str,
        flag: PresetFlag,
        value: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(ty) = self.settings_type_mut(id) {
            flag.set(ty, value);
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn settings_select_type(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            page.selected_type = id.to_owned();
        }
        self.notify_pane_views(cx);
    }

    /// Adds a custom type (copying Development's presets) and selects it.
    pub(super) fn settings_add_type(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            let types = &mut page.draft.connection_types;
            let ty = ConnectionType {
                id: ConnectionType::new_custom_id(types),
                name: ConnectionType::new_custom_name(types),
                ..ConnectionType::default()
            };
            page.selected_type = ty.id.clone();
            types.push(ty);
        }
        self.notify_pane_views(cx);
    }

    /// Removes a custom type; connections that used it fall back to no type.
    pub(super) fn settings_delete_type(&mut self, id: &str, cx: &mut Context<Self>) {
        if connection::is_builtin_id(id) {
            return;
        }
        if let Some(page) = self.settings_page_mut() {
            page.draft.connection_types.retain(|ty| ty.id != id);
            page.selected_type.clear();
            page.type_editor = None;
        }
        self.notify_pane_views(cx);
    }

    fn render_connection_types(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let selected_id = page.selected_connection_type().map(|ty| ty.id.clone());
        let errors = connection::type_errors(&page.draft.connection_types);
        let list = div()
            .debug_selector(|| "connection-types-list".into())
            .w(px(190.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(page.draft.connection_types.iter().map(|ty| {
                let id = ty.id.clone();
                let modified = self.settings.connection_type(&ty.id) != Some(ty);
                let selector_id = id.clone();
                let click_id = id.clone();
                Button::new(SharedString::from(format!("connection-type-{id}")))
                    .debug_selector(move || format!("connection-type-row-{selector_id}"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .size(px(8.0))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(connection_policy::type_color(ty)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_left()
                                    .child(if ty.name.trim().is_empty() {
                                        "(unnamed)".to_owned()
                                    } else {
                                        ty.name.clone()
                                    }),
                            )
                            .when(modified, |this| {
                                this.child(div().size(px(6.0)).rounded_full().bg(theme::accent()))
                            }),
                    )
                    .w_full()
                    .ghost()
                    .selected(selected_id.as_deref() == Some(id.as_str()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_select_type(&click_id, cx);
                    }))
            }))
            .child(
                Button::new("connection-type-add")
                    .debug_selector(|| "connection-type-add".into())
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(Icon::new(IconName::Plus).size(px(14.0)))
                            .child("Add type"),
                    )
                    .w_full()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.settings_add_type(cx))),
            );
        let editor = match page
            .selected_connection_type()
            .zip(page.type_editor.as_ref())
        {
            Some((ty, editor)) if editor.id == ty.id => {
                self.render_type_editor(ty, editor, &errors, cx)
            }
            _ => div().into_any_element(),
        };
        div()
            .debug_selector(|| "connection-types".into())
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .pt(px(12.0))
                    .text_size(px(11.0))
                    .text_color(theme::text_faint())
                    .child("CONNECTION TYPES"),
            )
            .child(
                div()
                    .px(px(12.0))
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child("A connection type gives its connections a color and safety presets. Pick one in the connection dialog (Environment); connections without a type follow the global settings only."),
            )
            .child(div().flex().items_start().gap(px(16.0)).px(px(12.0)).child(list).child(editor))
            .into_any_element()
    }

    fn render_type_editor(
        &self,
        ty: &ConnectionType,
        editor: &TypeEditor,
        errors: &[(String, connection::TypeError)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let builtin = ty.is_builtin();
        let error = errors
            .iter()
            .find(|(id, _)| *id == ty.id)
            .map(|(_, error)| error.to_string());
        let field = |label: &'static str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .w(px(70.0))
                        .flex_shrink_0()
                        .text_size(px(13.0))
                        .text_color(theme::text())
                        .child(label),
                )
                .child(control)
        };
        let id = ty.id.clone();
        div()
            .debug_selector(|| "connection-type-editor".into())
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(field(
                "Name",
                div()
                    .debug_selector(|| "connection-type-name".into())
                    .w(px(CONTROL_WIDTH))
                    .child(Input::new(&editor.name).small().disabled(builtin))
                    .into_any_element(),
            ))
            .child(field(
                "Color",
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .debug_selector(|| "connection-type-color".into())
                            .w(px(120.0))
                            .child(Input::new(&editor.color).small()),
                    )
                    .child(
                        div()
                            .debug_selector(|| "connection-type-swatch".into())
                            .size(px(18.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(theme::border())
                            .bg(connection_policy::type_color(ty)),
                    )
                    .into_any_element(),
            ))
            .when_some(error, |this, message| {
                this.child(
                    div()
                        .debug_selector(|| "connection-type-error".into())
                        .text_size(px(12.0))
                        .text_color(theme::danger())
                        .child(message),
                )
            })
            .children(PresetFlag::ALL.into_iter().map(|flag| {
                let id = id.clone();
                div()
                    .debug_selector(move || format!("connection-type-preset-{}", flag.key()))
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(16.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .text_size(px(13.0))
                                    .text_color(theme::text())
                                    .child(flag.title()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme::text_muted())
                                    .child(flag.description()),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("connection-type-flag-{}", flag.key()))
                            .flex_shrink_0()
                            .child(
                                Switch::new(SharedString::from(format!("connection-type-switch-{}", flag.key())))
                                    // Returning to auto-commit is part of smart commit: without it the switch is off (what
                                    // was chosen is kept, and shows again when smart commit is on).
                                    .checked(flag.get(ty) && (flag != PresetFlag::SmartCommitRecover || ty.smart_commit))
                                    .disabled(flag == PresetFlag::SmartCommitRecover && !ty.smart_commit)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.settings_type_set_flag(&id, flag, *checked, cx);
                                    })),
                            ),
                    )
            }))
            .child(
                div()
                    .debug_selector(|| "connection-type-agent".into())
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(16.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(13.0)).text_color(theme::text()).child("Agent access"))
                            .child(
                                div().text_size(px(12.0)).text_color(theme::text_muted()).child(
                                    "What an AI agent (the in-app assistant, MCP clients) may do on connections of this type \
                                     that set nothing themselves. Never more than Read; a Production type always needs the \
                                     connection's own setting, and a read-only type caps every connection at Read.",
                                ),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "connection-type-agent-explanation".into())
                                    .text_size(px(12.0))
                                    .text_color(theme::text_faint())
                                    .child(ty.agent_access.type_preset().description()),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "connection-type-agent-select".into())
                            .flex_shrink_0()
                            .w(px(160.0))
                            .child(Select::new(&editor.agent).menu_width(px(400.0))),
                    ),
            )
            .when(!builtin, |this| {
                let id = id.clone();
                this.child(
                    div().child(
                        Button::new("connection-type-delete")
                            .debug_selector(|| "connection-type-delete".into())
                            .label("Delete type")
                            .compact()
                            .danger()
                            .on_click(cx.listener(move |this, _, _, cx| this.settings_delete_type(&id, cx))),
                    ),
                )
            })
            .into_any_element()
    }
}

/// Position of a setting among the children of its page (the section heading
/// first, then per group a header followed by its rows).
fn flat_index(descriptor: &SettingDescriptor) -> usize {
    let mut index = 1;
    let mut groups: Vec<&str> = Vec::new();
    for other in registry::visible(descriptor.section) {
        if !groups.contains(&other.group) {
            groups.push(other.group);
            index += 1;
        }
        if other.id == descriptor.id {
            return index;
        }
        index += 1;
    }
    0
}

fn subscribe_text(
    input: &Entity<InputState>,
    id: &'static str,
    window: &mut Window,
    cx: &mut Context<AppRoot>,
) -> Subscription {
    cx.subscribe_in(
        input,
        window,
        move |this: &mut AppRoot, input: &Entity<InputState>, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                this.settings_edit_text(id, text, cx);
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn ensure_select(
    page: &mut SettingsPage,
    id: &'static str,
    labels: Vec<SharedString>,
    current: Option<SharedString>,
    searchable: bool,
    window: &mut Window,
    cx: &mut Context<AppRoot>,
    subscriptions: &mut Vec<Subscription>,
) {
    let position = current
        .as_ref()
        .and_then(|current| labels.iter().position(|label| label == current));
    let select = match page.selects.get(id) {
        Some(select) => select.clone(),
        None => {
            let select = cx.new(|cx| {
                SelectState::new(
                    labels,
                    position.map(|row| IndexPath::default().row(row)),
                    window,
                    cx,
                )
                .searchable(searchable)
            });
            subscriptions.push(cx.subscribe_in(
                &select,
                window,
                move |this: &mut AppRoot,
                      _: &Entity<SelectState<Vec<SharedString>>>,
                      event: &SelectEvent<Vec<SharedString>>,
                      _,
                      cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    this.settings_choose(id, label, cx);
                },
            ));
            page.selects.insert(id, select.clone());
            return;
        }
    };
    // A reset, a rebase or another provider moved the draft: show it.
    match current {
        Some(current) if select.read(cx).selected_value() != Some(&current) => {
            select.update(cx, |state, cx| {
                state.set_selected_value(&current, window, cx)
            });
        }
        None if select.read(cx).selected_value().is_some() => {
            select.update(cx, |state, cx| state.set_selected_index(None, window, cx));
        }
        _ => {}
    }
}

// ---- shortcuts ---------------------------------------------------------------------

impl AppRoot {
    /// The actions whose title, category or chord matches `query`, judged on
    /// the draft.
    fn matching_shortcuts(&self, query: &str) -> Vec<&'static shortcuts::ShortcutDef> {
        let Some(page) = self.settings_ui.page.as_ref() else {
            return Vec::new();
        };
        shortcuts::all()
            .filter(|def| shortcuts::matches(def, &page.draft.shortcuts, query))
            .collect()
    }

    /// Starts recording a chord for `id` (replacing `slot`, or adding one).
    /// The next keystroke is taken instead of running its action.
    pub(super) fn shortcut_start_record(
        &mut self,
        id: &'static str,
        slot: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event, _, cx| {
            let stroke = event.keystroke.clone();
            let taken = weak
                .update(cx, |this, cx| this.shortcut_captured(&stroke, cx))
                .unwrap_or(false);
            if taken {
                cx.stop_propagation();
            }
        });
        let Some(page) = self.settings_page_mut() else {
            return;
        };
        page.shortcut_errors.remove(id);
        page.recording = Some(Recording {
            id,
            slot,
            _intercept: intercept,
        });
        // Keys go to the page, not to a text field that would also type them.
        let focus = page.focus.clone();
        window.focus(&focus, cx);
        self.notify_pane_views(cx);
    }

    /// A keystroke arrived while recording. Returns whether it was consumed
    /// (a lone modifier is, so the chord can still be completed). Leaving the
    /// Settings tab ends the recording without consuming anything.
    pub(super) fn shortcut_captured(
        &mut self,
        stroke: &gpui::Keystroke,
        cx: &mut Context<Self>,
    ) -> bool {
        let on_settings = self
            .workspace
            .active_tab()
            .is_some_and(|tab| tab.kind == TabKind::Settings);
        let Some(page) = self.settings_page_mut() else {
            return false;
        };
        let Some((id, slot)) = page
            .recording
            .as_ref()
            .map(|recording| (recording.id, recording.slot))
        else {
            return false;
        };
        if !on_settings {
            page.recording = None;
            return false;
        }
        match shortcuts::record(stroke) {
            Ok(shortcuts::Recorded::Modifier) => return true,
            Ok(shortcuts::Recorded::Cancel) => page.recording = None,
            Ok(shortcuts::Recorded::Chord(chord)) => {
                page.recording = None;
                if let Some(def) = shortcuts::find(id) {
                    match shortcuts::assign(&mut page.draft.shortcuts, def, chord, slot) {
                        Ok(()) => {
                            page.shortcut_errors.remove(id);
                        }
                        Err(error) => {
                            page.shortcut_errors.insert(id, error.to_string());
                        }
                    }
                }
            }
            Err(error) => {
                page.recording = None;
                page.shortcut_errors.insert(id, error.to_string());
            }
        }
        self.notify_pane_views(cx);
        true
    }

    pub(super) fn shortcut_remove(
        &mut self,
        id: &'static str,
        slot: usize,
        cx: &mut Context<Self>,
    ) {
        if let (Some(page), Some(def)) = (self.settings_page_mut(), shortcuts::find(id)) {
            shortcuts::remove(&mut page.draft.shortcuts, def, slot);
            page.shortcut_errors.remove(id);
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn shortcut_reset(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let (Some(page), Some(def)) = (self.settings_page_mut(), shortcuts::find(id)) {
            shortcuts::reset(&mut page.draft.shortcuts, def);
            page.shortcut_errors.remove(id);
            if page
                .recording
                .as_ref()
                .is_some_and(|recording| recording.id == id)
            {
                page.recording = None;
            }
        }
        self.notify_pane_views(cx);
    }

    pub(super) fn shortcuts_reset_all(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.settings_page_mut() {
            page.draft.shortcuts.clear();
            page.shortcut_errors.clear();
            page.recording = None;
        }
        self.notify_pane_views(cx);
    }

    fn render_shortcuts_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut content: Vec<AnyElement> = Vec::new();
        content.push(
            div()
                .flex()
                .items_center()
                .justify_between()
                .pb(px(4.0))
                .child(
                    div()
                        .text_size(px(18.0))
                        .text_color(theme::text())
                        .child("Shortcuts"),
                )
                .child(
                    Button::new("shortcuts-reset-all")
                        .debug_selector(|| "shortcuts-reset-all".into())
                        .label("Reset all")
                        .compact()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.shortcuts_reset_all(cx))),
                )
                .into_any_element(),
        );
        content.push(
            div()
                .px(px(12.0))
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(
                    "Click a shortcut to change it or Record to add one; Escape cancels. \
                     Search above also finds shortcuts by name or keys.",
                )
                .into_any_element(),
        );
        let mut categories: Vec<&'static str> = Vec::new();
        for def in shortcuts::all() {
            if !categories.contains(&def.category) {
                categories.push(def.category);
            }
        }
        for category in categories {
            content.push(
                div()
                    .pt(px(12.0))
                    .text_size(px(11.0))
                    .text_color(theme::text_faint())
                    .child(category.to_uppercase())
                    .into_any_element(),
            );
            for def in shortcuts::all().filter(|def| def.category == category) {
                content.push(self.render_shortcut_row(def, cx));
            }
        }
        content
    }

    fn render_shortcut_row(
        &self,
        def: &'static shortcuts::ShortcutDef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = def.id;
        let page = self
            .settings_ui
            .page
            .as_ref()
            .expect("settings page exists while rendering");
        let macos = cfg!(target_os = "macos");
        let chords = def.effective(&page.draft.shortcuts);
        let modified = chords != def.effective(&self.settings.shortcuts);
        let not_default = def.is_modified(&page.draft.shortcuts);
        let error = page.shortcut_errors.get(id).cloned();
        let recording = page
            .recording
            .as_ref()
            .filter(|recording| recording.id == id);
        let controls = if !def.editable {
            div()
                .text_size(px(12.0))
                .text_color(theme::text_faint())
                .child("Fixed")
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .children(chords.iter().enumerate().map(|(slot, chord)| {
                    let capturing = recording.is_some_and(|recording| recording.slot == Some(slot));
                    let label = if capturing {
                        "Press keys\u{2026}".to_owned()
                    } else {
                        shortcuts::display(chord, macos)
                    };
                    div()
                        .flex()
                        .items_center()
                        .child(
                            Button::new(SharedString::from(format!("shortcut-chip-{id}-{slot}")))
                                .debug_selector(move || format!("shortcut-chip-{id}-{slot}"))
                                .label(label)
                                .compact()
                                .selected(capturing)
                                .tooltip("Click to change this shortcut")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.shortcut_start_record(id, Some(slot), window, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("shortcut-remove-{id}-{slot}")))
                                .debug_selector(move || format!("shortcut-remove-{id}-{slot}"))
                                .icon(Icon::empty().path("icons/close.svg").size(px(12.0)))
                                .compact()
                                .ghost()
                                .tooltip("Remove this shortcut")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.shortcut_remove(id, slot, cx);
                                })),
                        )
                }))
                .when(chords.is_empty() && recording.is_none(), |this| {
                    this.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_faint())
                            .child("Not set"),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!("shortcut-record-{id}")))
                        .debug_selector(move || format!("shortcut-record-{id}"))
                        .label(match recording {
                            Some(recording) if recording.slot.is_none() => "Press keys\u{2026}",
                            _ => "Record",
                        })
                        .compact()
                        .ghost()
                        .selected(recording.is_some_and(|recording| recording.slot.is_none()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.shortcut_start_record(id, None, window, cx);
                        })),
                )
                .into_any_element()
        };
        div()
            .id(SharedString::from(format!("shortcut-row-{id}")))
            .debug_selector(move || format!("shortcut-{id}"))
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.0))
            .px(px(12.0))
            .py(px(8.0))
            .rounded(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .debug_selector(move || format!("shortcut-modified-{id}"))
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .when(modified, |this| this.bg(theme::accent())),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme::text())
                                    .child(def.title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_faint())
                                    .child(def.category),
                            ),
                    )
                    .when_some(error, |this, message| {
                        this.child(
                            div()
                                .debug_selector(move || format!("shortcut-error-{id}"))
                                .text_size(px(12.0))
                                .text_color(theme::danger())
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_shrink_0()
                    .child(controls)
                    .child(div().w(px(RESET_SLOT)).flex_shrink_0().when(
                        not_default && def.editable,
                        |this| {
                            this.child(
                                Button::new(SharedString::from(format!("shortcut-reset-{id}")))
                                    .debug_selector(move || format!("shortcut-reset-{id}"))
                                    .icon(Icon::empty().path("icons/undo-2.svg").size(px(14.0)))
                                    .compact()
                                    .ghost()
                                    .tooltip("Reset to default")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.shortcut_reset(id, cx)
                                    })),
                            )
                        },
                    )),
            )
            .into_any_element()
    }
}
