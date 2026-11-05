//! What a connection's type and overrides do to the running app: the
//! effective values (`Settings::for_connection`), the type colors drawn next
//! to a connection, the read-only refusals and the confirmations (SQL
//! execution, data changes).
//!
//! A connection without a type behaves exactly as before: global settings
//! only, no badge, and the existing "Apply N staged changes?" dialog.

use super::*;
use crate::settings::connection::{ConnectionProfile, ConnectionType, Effective};
use engine_storage::connection::SavedConnectionRecord;
use uuid::Uuid;

/// What a statement that waited for the user's confirmation is started as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GatedRun {
    /// A normal Run.
    Run,
    /// An Explain of the statement in this mode (Analyze executes it).
    Explain(engine_plan::ExplainMode),
    /// A MongoDB shell script of a script tab.
    MongoShell,
}

/// The type and overrides of every saved connection, loaded together with the
/// records (never read from disk while rendering).
#[derive(Default)]
pub(super) struct ConnectionPolicy {
    profiles: HashMap<Uuid, ConnectionProfile>,
    /// Changes whenever the profiles do, so what is derived from them can be kept until then.
    revision: u64,
}

fn next_policy_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl ConnectionPolicy {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn from_records(records: &[SavedConnectionRecord]) -> Self {
        Self {
            revision: next_policy_revision(),
            profiles: records
                .iter()
                .filter_map(|record| {
                    Some((
                        Uuid::parse_str(&record.id).ok()?,
                        ConnectionProfile::from_record(record),
                    ))
                })
                .collect(),
        }
    }

    pub(super) fn profile(&self, connection: &str) -> Option<&ConnectionProfile> {
        self.profiles.get(&Uuid::parse_str(connection).ok()?)
    }

    /// Every connection that has a profile, by id.
    pub(super) fn profiles(&self) -> impl Iterator<Item = (String, &ConnectionProfile)> {
        self.profiles
            .iter()
            .map(|(id, profile)| (id.to_string(), profile))
    }

    /// Fixture connections (capture scenarios, tests) have no stored record.
    pub(super) fn set_profile(&mut self, connection: &str, profile: ConnectionProfile) {
        if let Ok(id) = Uuid::parse_str(connection) {
            self.profiles.insert(id, profile);
            self.revision = next_policy_revision();
        }
    }
}

/// The colour a connection type is drawn in.
pub(super) fn type_color(ty: &ConnectionType) -> gpui::Rgba {
    super::navigator_ui::parse_hex_color(&ty.color).unwrap_or_else(theme::accent)
}

/// A small pill with the type's short name in its color (sidebar row, tab
/// group chip). Production is drawn stronger than the other types.
pub(super) fn type_badge(ty: &ConnectionType, selector: String) -> gpui::Stateful<Div> {
    let color = type_color(ty);
    let strong = ty.is_production();
    div()
        .id(gpui::SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_shrink_0()
        .h(px(14.0))
        .px(px(4.0))
        .ml(px(4.0))
        .flex()
        .items_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(super::navigator_ui::tinted(
            color,
            if strong { 0.9 } else { 0.5 },
        ))
        .bg(super::navigator_ui::tinted(
            color,
            if strong { 0.3 } else { 0.14 },
        ))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(ty.badge())
}

/// A small outlined pill in muted text with a tooltip: the route of a
/// connection (SSH / Proxy) and its read-only mark, on a sidebar row and on
/// the connection page.
pub(super) fn info_badge(
    text: impl Into<gpui::SharedString>,
    tooltip: impl Into<gpui::SharedString>,
    selector: String,
) -> gpui::Stateful<Div> {
    let tooltip = tooltip.into();
    div()
        .id(gpui::SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_shrink_0()
        .h(px(14.0))
        .px(px(4.0))
        .ml(px(4.0))
        .flex()
        .items_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(theme::border())
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
        })
        .child(text.into())
}

/// What a staged batch of SQL grid changes holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DataChangeSummary {
    pub(super) updates: usize,
    pub(super) inserts: usize,
    pub(super) deletes: usize,
    pub(super) table: String,
}

impl DataChangeSummary {
    /// One line per kind of change that is staged.
    pub(super) fn lines(&self) -> Vec<String> {
        fn plural(count: usize, one: &str, many: &str) -> String {
            format!("{count} {}", if count == 1 { one } else { many })
        }
        [
            (self.updates > 0).then(|| plural(self.updates, "cell update", "cell updates")),
            (self.inserts > 0).then(|| plural(self.inserts, "inserted row", "inserted rows")),
            (self.deletes > 0).then(|| plural(self.deletes, "deleted row", "deleted rows")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

impl AppRoot {
    /// The effective values of a connection (override > type preset > global).
    pub(super) fn effective_for(&self, connection: &str) -> Effective {
        match self.connection_policy.profile(connection) {
            Some(profile) => self.settings.for_connection(profile),
            None => self.settings.for_connection(&ConnectionProfile::default()),
        }
    }

    pub(super) fn effective_for_tab(&self, tab_id: u64) -> Effective {
        let connection = self
            .workspace
            .tab(tab_id)
            .map_or("", |tab| tab.connection_id.as_str());
        self.effective_for(connection)
    }

    /// The type of a connection, when it has a known one. Borrowed, so the
    /// tab strip and sidebar can ask for every row of a frame.
    pub(super) fn connection_type_of(&self, connection: &str) -> Option<&ConnectionType> {
        let profile = self.connection_policy.profile(connection)?;
        self.settings.connection_type(&profile.type_id)
    }

    /// "Production · shop-db": the connection as the confirmations name it.
    pub(super) fn connection_subject(&self, connection: &str, effective: &Effective) -> String {
        let name = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map_or("this connection", |saved| saved.name.as_str());
        match &effective.connection_type {
            Some(ty) => format!("{} \u{00B7} {name}", ty.name),
            None => name.to_owned(),
        }
    }

    /// New tabs of a connection with its own page size open with it.
    pub(super) fn sync_connection_output_limits(&mut self) {
        let global = self.settings.results.page_size;
        let limits = self
            .saved_connections
            .iter()
            .filter_map(|saved| {
                let key = saved.id.to_string();
                let page_size = self.effective_for(&key).page_size;
                (page_size != global).then(|| (key, i64::from(page_size)))
            })
            .collect();
        self.workspace.set_connection_output_limits(limits);
    }

    // ---- read-only --------------------------------------------------------------

    fn read_only_notice(&self, connection: &str, effective: &Effective, what: &str) -> String {
        let subject = effective.connection_type.as_ref().map_or_else(
            || self.connection_subject(connection, effective),
            |ty| ty.name.clone(),
        );
        format!(
            "{what} blocked: this connection is read-only ({subject}). Change it in Settings \u{203A} Query & Connections \u{203A} Connection types."
        )
    }

    /// Why `what` ("Editing is", "$out is") is refused on the tab: its
    /// connection is read-only. `None` when it is not.
    pub(super) fn read_only_message(&self, tab_id: u64, what: &str) -> Option<String> {
        let connection = self.workspace.tab(tab_id)?.connection_id.as_str();
        self.read_only_connection_message(connection, what)
    }

    /// [`Self::read_only_message`] for a connection (sidebar actions).
    pub(super) fn read_only_connection_message(
        &self,
        connection: &str,
        what: &str,
    ) -> Option<String> {
        let effective = self.effective_for(connection);
        effective
            .read_only
            .then(|| self.read_only_notice(connection, &effective, what))
    }

    pub(super) fn read_only_edit_message(&self, tab_id: u64) -> Option<String> {
        self.read_only_message(tab_id, "Editing is")
    }

    pub(super) fn connection_is_read_only(&self, tab_id: u64) -> bool {
        self.effective_for_tab(tab_id).read_only
    }

    /// A grid edit on a read-only connection is refused with a message in the
    /// result's status row. `true` when it was refused.
    pub(super) fn refuse_read_only_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) -> bool {
        let Some(message) = self.read_only_edit_message(tab_id) else {
            return false;
        };
        self.view_mut(tab_id).detail_ui.error = Some(message);
        self.notify_tab(tab_id, cx);
        true
    }

    /// Fails the tab's run with `message` (the result area and the Error Log
    /// show it) without sending anything to the database.
    pub(super) fn refuse_statement(
        &mut self,
        tab_id: u64,
        sql: &str,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let tab = self.workspace.tab(tab_id);
        let connection = tab.map(|tab| tab.connection_id.clone());
        let context = tab.map(|tab| crate::query::RunContext {
            sql: sql.to_owned(),
            connection: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: tab.schema.clone(),
        });
        let failed = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| {
                let run = runtime.query.begin()?;
                if let Some(context) = context {
                    runtime.query.describe(run, context);
                }
                Some(runtime.query.fail(run, message.clone()))
            })
            .unwrap_or(false);
        if failed {
            self.report_warning(Area::Query, connection.as_deref(), &message);
        }
        cx.notify();
    }

    // ---- SQL execution ------------------------------------------------------------

    /// A statement typed in the SQL editor, before it is sent. `true` when it
    /// was taken care of here (refused, or waiting for the user's answer) and
    /// must not run now. Read-only is checked first: nothing is asked about a
    /// statement that would be refused anyway.
    pub(super) fn gate_editor_statement(
        &mut self,
        tab_id: u64,
        sql: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        self.gate_statement(tab_id, sql, GatedRun::Run, cx)
    }

    /// `gate_editor_statement` for a statement that is run as `then`: what the confirmation
    /// dialog starts when the user agrees.
    pub(super) fn gate_statement(
        &mut self,
        tab_id: u64,
        sql: &str,
        then: GatedRun,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .filter(|connection| !connection.is_empty())
        else {
            return false;
        };
        let effective = self.effective_for(&connection);
        if effective.read_only {
            let redis = self
                .workspace
                .tab(tab_id)
                .is_some_and(|tab| tab.query_mode.is_redis_command());
            let write = if redis {
                // Unknown commands count as writes.
                (!crate::redis::is_read_only(sql)).then(|| {
                    sql.split_whitespace()
                        .next()
                        .map_or_else(|| "An empty command".to_owned(), str::to_ascii_uppercase)
                })
            } else {
                crate::sql_write::first_write_keyword(sql, &self.sql_lex(tab_id))
            };
            if let Some(keyword) = write {
                let message =
                    self.read_only_notice(&connection, &effective, &format!("{keyword} is"));
                self.refuse_statement(tab_id, sql, message, cx);
                return true;
            }
        }
        self.ask_execute_confirmation(tab_id, &connection, &effective, sql, then, cx)
    }

    /// A MongoDB shell script about to run: asked about first on a connection that confirms
    /// executions (the script is shown). Read-only connections are not refused here: the script
    /// runs with writes off and the shell says which call it refused. `true` when the
    /// confirmation was opened and the script must not run now.
    pub(super) fn gate_shell_script(
        &mut self,
        tab_id: u64,
        script: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(connection) = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .filter(|connection| !connection.is_empty())
        else {
            return false;
        };
        let effective = self.effective_for(&connection);
        self.ask_execute_confirmation(tab_id, &connection, &effective, script, GatedRun::MongoShell, cx)
    }

    /// Opens the "Run this statement?" dialog when the connection's effective settings ask for
    /// one; `true` when it was opened (or refused for lack of a window).
    fn ask_execute_confirmation(
        &mut self,
        tab_id: u64,
        connection: &str,
        effective: &Effective,
        sql: &str,
        then: GatedRun,
        cx: &mut Context<Self>,
    ) -> bool {
        if !effective.confirm_execute {
            return false;
        }
        let Some(window) = self.driver_runtime.window else {
            self.refuse_statement(
                tab_id,
                sql,
                "Could not ask for confirmation: no window to ask in".into(),
                cx,
            );
            return true;
        };
        let subject = self.connection_subject(connection, effective);
        let sql = sql.to_owned();
        let root = cx.entity().downgrade();
        // The window cannot be updated while this entity is: open the dialog
        // once the current update has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| {
                    this.open_execute_confirmation(tab_id, sql, subject, then, window, cx);
                });
            });
        });
        true
    }

    fn open_execute_confirmation(
        &mut self,
        tab_id: u64,
        sql: String,
        subject: String,
        then: GatedRun,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let sql_for_run = sql.clone();
            dialog
                .width(px(520.0))
                .title(if then == GatedRun::MongoShell {
                    "Run this script?"
                } else {
                    "Run this statement?"
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "execute-confirm-subject".into())
                                .child(format!("It will run on {subject}.")),
                        )
                        .child(
                            div()
                                .debug_selector(|| "execute-confirm-statement".into())
                                .max_h(px(160.0))
                                .overflow_hidden()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(sql.clone()),
                        ),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Run"))
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            div()
                                .debug_selector(|| "execute-confirm-cancel".into())
                                .child(
                                    gpui_component::dialog::DialogClose::new()
                                        .trigger(|button| button.label("Cancel")),
                                ),
                        )
                        .child(
                            div().debug_selector(|| "execute-confirm-run".into()).child(
                                gpui_component::dialog::DialogAction::new().child(
                                    Button::new("execute-confirm-run").label("Run").primary(),
                                ),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let sql = sql_for_run.clone();
                    let _ = root.update(cx, |this, cx| match then {
                        GatedRun::Run => this.run_editor_sql(tab_id, sql, cx),
                        GatedRun::Explain(mode) => this.explain_confirmed(tab_id, mode, &sql, cx),
                        GatedRun::MongoShell => this.run_confirmed_shell_script(tab_id, sql, cx),
                    });
                    true
                })
        });
        cx.notify();
    }

    // ---- data changes -------------------------------------------------------------

    /// The staged SQL changes of a tab, by kind.
    pub(super) fn data_change_summary(&self, tab_id: u64) -> Option<DataChangeSummary> {
        let tab = self.workspace.tab(tab_id)?;
        let runtime = self.workspace.runtime(tab_id)?;
        let (mut inserts, mut deletes) = (0, 0);
        for row in &runtime.pending_sql_rows {
            match row {
                crate::workspace::PendingSqlRow::Insert(_) => inserts += 1,
                crate::workspace::PendingSqlRow::Delete { .. } => deletes += 1,
            }
        }
        let table = match tab.schema.as_deref().filter(|schema| !schema.is_empty()) {
            Some(schema) => format!("{schema}.{}", tab.collection),
            None => tab.collection.clone(),
        };
        Some(DataChangeSummary {
            updates: runtime.pending_edits.len(),
            inserts,
            deletes,
            table,
        })
    }

    /// The text of the data-change summary shown in the Apply / Commit dialog.
    pub(super) fn data_change_summary_lines(&self, tab_id: u64) -> Vec<String> {
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .unwrap_or_default();
        let effective = self.effective_for(&connection);
        let mut lines = vec![format!(
            "On {}",
            self.connection_subject(&connection, &effective)
        )];
        if let Some(summary) = self.data_change_summary(tab_id) {
            if !summary.table.is_empty() {
                lines.push(format!("Table {}", summary.table));
            }
            lines.extend(summary.lines());
        }
        lines
    }

    /// Commit of staged Mongo changes: refused on a read-only connection,
    /// confirmed first when the connection type asks for it.
    pub(super) fn request_commit_mongo(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.connection_id.clone())
            .unwrap_or_default();
        if self.refuse_while_editing_mongo_document(tab_id, cx) || self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        let effective = self.effective_for(&connection);
        if !effective.confirm_data_change {
            self.commit_mongo_mutations(tab_id, cx);
            return;
        }
        let staged = self
            .workspace
            .runtime(tab_id)
            .map_or(0, |runtime| runtime.mongo.mutations.active().len());
        let mut lines = vec![format!(
            "On {}",
            self.connection_subject(&connection, &effective)
        )];
        lines.push(format!(
            "{staged} staged {}",
            if staged == 1 { "change" } else { "changes" }
        ));
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title(format!("Commit {staged} staged changes?"))
                .child(
                    div()
                        .debug_selector(|| "data-change-summary".into())
                        .flex()
                        .flex_col()
                        .children(lines.clone()),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Commit"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| this.commit_mongo_mutations(tab_id, cx));
                    true
                })
        });
    }
}
