// R7: Users & Privileges, through the real connection menu, the tab, its dialogs and the shared review dialog.
//
// * The fixture tests run the REAL tab against a scripted catalog (a fake PostgreSQL / MySQL server that answers the catalog
//   queries from a state the tests change) and the injected executor of the structure editor (it logs the statements it is given
//   instead of running them, and tells the fake server what they did).
// * The live tests at the end (`#[ignore]`d) run the same code against the dedicated `tablex-test-*` containers.

use crate::app::accounts::AccountsView;
use crate::app::command_palette::StaticAction;
use crate::sql_account::catalog::{CatalogSession, ReadError, TextRows};

const AC_PASSWORD: &str = "hunter2-needle";

#[derive(Default)]
struct AcState {
    mysql: bool,
    /// name, can log in, superuser, create databases, connection limit
    roles: Vec<(String, bool, bool, bool, i32)>,
    /// MySQL: user, host, locked, role
    accounts: Vec<(String, String, bool, bool)>,
    /// role, member, admin option
    memberships: Vec<(String, String, bool)>,
    /// PostgreSQL held privileges: kind, schema, name, privilege, grantable
    held: Vec<[String; 7]>,
    /// Memberships another role granted (role, member, grantor): the connection's role cannot take them back.
    foreign_memberships: Vec<(String, String, String)>,
    databases: Vec<String>,
    schemas: Vec<String>,
    tables: Vec<(String, String)>,
    login: String,
    denied: Option<String>,
    /// How many times the list of roles was read.
    loads: usize,
    /// The database each read of privileges asked for.
    privilege_reads: Vec<Option<String>>,
    sql_mode: String,
    queries: Vec<String>,
    /// MySQL: `max_questions` of each user.
    limits: std::collections::BTreeMap<String, u32>,
    /// A read of privileges waits while this is set (a slow server).
    hold_privileges: bool,
}

type AcServer = std::sync::Arc<std::sync::Mutex<AcState>>;

fn ac_pg_server() -> AcServer {
    let role = |name: &str, login: bool, superuser: bool, createdb: bool, limit: i32| (name.to_owned(), login, superuser, createdb, limit);
    std::sync::Arc::new(std::sync::Mutex::new(AcState {
        roles: vec![role("app", true, false, false, -1), role("boss", true, true, true, -1), role("reader", false, false, false, -1), role("viewer", true, false, false, 3)],
        memberships: vec![("reader".into(), "app".into(), false)],
        held: [
            ["D", "", "warehouse", "CONNECT", "false", "boss", "true"],
            ["S", "", "public", "USAGE", "true", "boss", "true"],
            ["T", "public", "orders", "SELECT", "false", "boss", "true"],
            ["T", "public", "orders", "INSERT", "true", "boss", "true"],
        ]
        .into_iter()
        .map(|row| row.map(str::to_owned))
        .collect(),
        databases: vec!["other".into(), "warehouse".into()],
        schemas: vec!["audit".into(), "public".into()],
        tables: vec![("public".into(), "customers".into()), ("public".into(), "items".into()), ("public".into(), "orders".into())],
        login: "boss".into(),
        ..AcState::default()
    }))
}

fn ac_mysql_server() -> AcServer {
    let account = |user: &str, host: &str, locked: bool, role: bool| (user.to_owned(), host.to_owned(), locked, role);
    std::sync::Arc::new(std::sync::Mutex::new(AcState {
        mysql: true,
        accounts: vec![account("analyst", "%", true, true), account("app", "10.0.%", false, false), account("root", "localhost", false, false)],
        memberships: vec![("analyst".into(), "app".into(), false)],
        tables: vec![("shop".into(), "orders".into())],
        databases: vec!["shop".into(), "sys".into()],
        login: "root@localhost".into(),
        sql_mode: "STRICT_TRANS_TABLES".into(),
        ..AcState::default()
    }))
}

/// The fake server's session: answers the catalog queries from the state.
struct AcSession {
    state: AcServer,
    database: Option<String>,
}

fn ac_row(cells: &[&str]) -> Vec<Option<String>> {
    cells.iter().map(|cell| Some((*cell).to_owned())).collect()
}

impl CatalogSession for AcSession {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        if sql.contains("aclexplode") {
            while self.state.lock().unwrap().hold_privileges {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let mut state = self.state.lock().unwrap();
        state.queries.push(sql.to_owned());
        if let Some(message) = &state.denied {
            return Err(ReadError::from_server(message.clone()));
        }
        let flag = |on: bool| if on { "true" } else { "false" };
        Ok(if state.mysql {
            if sql.contains("CURRENT_USER") {
                vec![ac_row(&[&state.login, &state.sql_mode, "8.4.11"])]
            } else if sql.contains("FROM mysql.user") {
                state.loads += 1;
                state
                    .accounts
                    .iter()
                    .map(|(user, host, locked, role)| {
                        ac_row(&[user, host, if *locked { "Y" } else { "N" }, if *role { "Y" } else { "N" }, "caching_sha2_password", &state.limits.get(user).copied().unwrap_or(0).to_string(), "0", "0", "0", if *role { "1" } else { "0" }])
                    })
                    .collect()
            } else if sql.contains("role_edges") {
                state.memberships.iter().map(|(role, member, admin)| ac_row(&[role, "%", member, "10.0.%", if *admin { "Y" } else { "N" }])).collect()
            } else if sql.contains("information_schema.SCHEMA_PRIVILEGES") {
                state.privilege_reads.push(self.database.clone());
                vec![ac_row(&["shop", "SELECT", "NO"]), ac_row(&["shop", "INSERT", "NO"])]
            } else if sql.contains("information_schema.USER_PRIVILEGES") {
                vec![ac_row(&["USAGE", "NO"])]
            } else if sql.contains("information_schema.SCHEMATA") {
                state.databases.iter().map(|name| ac_row(&[name])).collect()
            } else if sql.contains("information_schema.TABLES") {
                state.tables.iter().map(|(schema, name)| ac_row(&[schema, name])).collect()
            } else {
                Vec::new()
            }
        } else if sql.contains("FROM pg_catalog.pg_roles ORDER BY rolname") {
            state.loads += 1;
            state.roles.iter().map(|(name, login, superuser, createdb, limit)| ac_row(&[name, flag(*login), flag(*superuser), flag(*createdb), "false", "true", "false", "false", &limit.to_string()])).map(|mut row| {
                row.push(None);
                row
            }).collect()
        } else if sql.contains("pg_auth_members") {
            state
                .memberships
                .iter()
                .map(|(role, member, admin)| ac_row(&[role, member, flag(*admin), "boss", "true", "true", "false", "true"]))
                .chain(state.foreign_memberships.iter().map(|(role, member, grantor)| ac_row(&[role, member, "false", grantor, "false", "false", "false", "false"])))
                .collect()
        } else if sql.starts_with("SELECT datname FROM") {
            state.databases.iter().map(|name| ac_row(&[name])).collect()
        } else if sql.contains("session_user") {
            vec![ac_row(&[&state.login, self.database.as_deref().unwrap_or("warehouse"), "PostgreSQL 17.10", "170010"])]
        } else if sql.contains("aclexplode") {
            state.privilege_reads.push(self.database.clone());
            state.held.iter().map(|row| ac_row(&row.each_ref().map(String::as_str))).collect()
        } else if sql.starts_with("SELECT 'D', '', datname") {
            state.databases.iter().map(|name| ac_row(&["D", "", name])).collect()
        } else if sql.starts_with("SELECT 'S', '', nspname") {
            state.schemas.iter().map(|name| ac_row(&["S", "", name])).collect()
        } else if sql.starts_with("SELECT 'T', n.nspname") {
            state.tables.iter().map(|(schema, name)| ac_row(&["T", schema, name])).collect()
        } else {
            Vec::new()
        })
    }
}

/// The server of the captures: the PostgreSQL one with a few dozen more roles and tables, so the list and the privileges read like a real
/// one; and the session the tab reads it through.
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
pub(super) fn ac_capture_server() -> impl Fn(Option<&str>) -> Box<dyn CatalogSession + Send> + Send + Sync + 'static {
    let server = ac_pg_server();
    {
        let mut state = server.lock().unwrap();
        for name in ["analytics_ro", "billing_app", "ci_deploy", "etl_loader", "metabase", "pg_monitor_agent", "support_ro", "web_app"] {
            state.roles.push((name.to_owned(), true, false, false, -1));
        }
        state.roles.push(("data_science".to_owned(), false, false, false, -1));
        state.memberships.push(("data_science".into(), "analytics_ro".into(), false));
        state.tables.extend([("public".to_owned(), "invoices".to_owned()), ("public".to_owned(), "order_items".to_owned()), ("audit".to_owned(), "log".to_owned())]);
        state.held.push(["T", "public", "customers", "SELECT", "false", "boss", "true"].map(str::to_owned));
        state.held.push(["T", "public", "invoices", "SELECT", "false", "boss", "true"].map(str::to_owned));
    }
    move |database| Box::new(AcSession { state: server.clone(), database: database.map(str::to_owned) })
}

/// Makes the tabs read from `server`, and the structure editor's executor log (and apply to the server) instead of running.
type AcEcho = std::sync::Arc<std::sync::Mutex<Option<usize>>>;

fn ac_wire(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, server: &AcServer) -> (PeLog, PeFail, AcEcho) {
    let reads = server.clone();
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let fail = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
    let echo: AcEcho = Default::default();
    let (log, failing, applied, echoing) = (ran.clone(), fail.clone(), server.clone(), echo.clone());
    app.update(visual, |this, _| {
        this.accounts_injected = Some(std::sync::Arc::new(move |database: Option<&str>| -> Box<dyn CatalogSession + Send> {
            Box::new(AcSession { state: reads.clone(), database: database.map(str::to_owned) })
        }));
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, _: &crate::app::structure_edit::apply::ApplyControl| {
            if failing.lock().unwrap().is_some_and(|needle| sql.contains(needle)) {
                // A server repeats the text around a mistake: all of the statement (PostgreSQL), or the first `n` bytes of it from the
                // keyword it stumbled on (MySQL: `near '...' at line 1`).
                return Err(match *echoing.lock().unwrap() {
                    // Not an echo: a message that names the account the statement is about.
                    Some(0) => "Operation ALTER USER failed for 'alice'@'%' (error 1396, SQLSTATE HY000)".to_owned(),
                    Some(n) => {
                        let from = sql.find("IDENTIFIED").unwrap_or(0);
                        let echoed: String = sql[from..].chars().scan(0, |bytes, c| { *bytes += c.len_utf8(); (*bytes <= n).then_some(c) }).collect();
                        format!("You have an error in your SQL syntax; check the manual that corresponds to your server version for the right syntax to use near '{echoed}' at line 1 (error 1064, SQLSTATE 42000)")
                    }
                    None => format!("syntax error at or near {sql}"),
                });
            }
            log.lock().unwrap().push(sql.to_owned());
            let mut state = applied.lock().unwrap();
            if let Some(name) = sql.strip_prefix("CREATE ROLE \"").and_then(|rest| rest.split('"').next()) {
                state.roles.push((name.to_owned(), true, false, false, -1));
            }
            if let (Some(name), Some(limit)) = (
                sql.strip_prefix("ALTER USER '").and_then(|rest| rest.split('\'').next()),
                sql.split("MAX_QUERIES_PER_HOUR ").nth(1).and_then(|rest| rest.split(' ').next()).and_then(|number| number.parse::<u32>().ok()),
            ) {
                state.limits.insert(name.to_owned(), limit);
            }
            if let Some(rest) = sql.strip_prefix("ALTER ROLE \"") {
                let (name, options) = rest.split_once("\" WITH ").unwrap_or((rest, ""));
                if let Some(role) = state.roles.iter_mut().find(|role| role.0 == name) {
                    for option in ["LOGIN", "NOLOGIN", "CREATEDB", "NOCREATEDB"] {
                        if options.split(' ').any(|word| word == option) {
                            match option {
                                "LOGIN" => role.1 = true,
                                "NOLOGIN" => role.1 = false,
                                "CREATEDB" => role.3 = true,
                                _ => role.3 = false,
                            }
                        }
                    }
                    if let Some(limit) = options.split("CONNECTION LIMIT ").nth(1).and_then(|rest| rest.split(' ').next()).and_then(|number| number.parse().ok()) {
                        role.4 = limit;
                    }
                }
            }
            // Memberships and privileges on a table (PostgreSQL's wording).
            if let Some(rest) = sql.strip_prefix("GRANT \"") {
                if let Some((role, member)) = rest.split_once("\" TO \"") {
                    state.memberships.push((role.to_owned(), member.trim_end_matches('"').to_owned(), false));
                }
            }
            if let Some(rest) = sql.strip_prefix("REVOKE \"") {
                if let Some((role, member)) = rest.split_once("\" FROM \"") {
                    state.memberships.retain(|row| !(row.0 == role && row.1 == member.trim_end_matches('"')));
                }
            }
            for (verb, joiner) in [("GRANT ", " TO "), ("REVOKE ", " FROM ")] {
                let Some((privileges, tail)) = sql.strip_prefix(verb).and_then(|rest| rest.split_once(" ON TABLE ")) else { continue };
                let Some((table, _)) = tail.split_once(joiner) else { continue };
                let Some((schema, name)) = table.split_once('.') else { continue };
                let (schema, name) = (schema.trim_matches('"').to_owned(), name.trim_matches('"').to_owned());
                let option_only = privileges.starts_with("GRANT OPTION FOR ");
                for privilege in privileges.trim_start_matches("GRANT OPTION FOR ").split(", ") {
                    let found = state.held.iter().position(|row| row[0] == "T" && row[1] == schema && row[2] == name && row[3] == privilege);
                    match (verb, option_only, found) {
                        ("GRANT ", _, Some(at)) => state.held[at][4] = sql.ends_with(" WITH GRANT OPTION").to_string(),
                        ("GRANT ", _, None) => state.held.push(["T".to_owned(), schema.clone(), name.clone(), privilege.to_owned(), sql.ends_with(" WITH GRANT OPTION").to_string(), "boss".to_owned(), "true".to_owned()]),
                        ("REVOKE ", true, Some(at)) => state.held[at][4] = "false".to_owned(),
                        ("REVOKE ", false, Some(at)) => {
                            state.held.remove(at);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(name) = sql.strip_prefix("DROP ROLE \"").and_then(|rest| rest.split('"').next()) {
                state.roles.retain(|role| role.0 != name);
            }
            Ok(())
        }));
    });
    (ran, fail, echo)
}

struct AcRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    ran: PeLog,
    fail: PeFail,
    echo: AcEcho,
    server: AcServer,
}

fn ac_pg(cx: &mut TestAppContext) -> AcRig<'_> {
    let (app, visual, _, _) = oa_pg(cx);
    let server = ac_pg_server();
    let (ran, fail, echo) = ac_wire(&app, visual, &server);
    AcRig { app, visual, ran, fail, echo, server }
}

impl AcRig<'_> {
    fn view(&mut self) -> gpui::Entity<AccountsView> {
        self.app.read_with(self.visual, |this, _| this.accounts.values().next().cloned().expect("a Users & Privileges tab is open"))
    }

    fn snapshot(&mut self) -> crate::app::accounts::Snapshot {
        let view = self.view();
        view.read_with(self.visual, |view, cx| view.snapshot(cx))
    }

    /// Opens the tab from the connection's menu and waits for the roles.
    fn open(&mut self) {
        oa_menu_on(&self.app, self.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-accounts");
        self.wait_rows();
    }

    fn wait_rows(&mut self) {
        let view = self.view();
        wait_until(self.visual, "the roles to be listed", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).total > 0));
        settle(self.visual);
    }

    fn click(&mut self, selector: &'static str) {
        oa_click(self.visual, selector);
    }

    fn select_row(&mut self, name: &str) {
        let position = self.snapshot().rows.iter().position(|row| row == name).unwrap_or_else(|| panic!("{name} is listed"));
        let selector: &'static str = Box::leak(format!("accounts-row-{position}").into_boxed_str());
        self.click(selector);
    }

    fn panel(&mut self, selector: &'static str) {
        self.click(selector);
        let view = self.view();
        wait_until(self.visual, "the panel", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
        settle(self.visual);
    }

    fn type_into(&mut self, selector: &'static str, text: &str) {
        if text.is_empty() {
            // Typing nothing replaces nothing: select all and delete.
            let bounds = self.visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
            self.visual.simulate_click(bounds.center(), Default::default());
            self.visual.simulate_keystrokes("secondary-a backspace");
            settle(self.visual);
        } else {
            oa_type(self.visual, selector, text);
        }
    }

    fn script(&mut self) -> String {
        oa_script(self.visual)
    }

    fn apply_and_wait(&mut self) {
        self.click("changes-apply");
        oa_wait_host(&self.app, self.visual);
    }

    fn statements(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

// ---- offering it ---------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_connection_menu_and_the_palette_offer_users_and_privileges_on_the_servers_that_have_them(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    // The menu of the PostgreSQL connection has it; clicking it opens one tab.
    rig.open();
    let (kind, title, connection) = rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone(), tab.connection_id.clone())
    });
    assert_eq!((kind, title.as_str(), connection.as_str()), (TabKind::Accounts, "Users & Privileges", OA_PG));
    for selector in ["accounts-view", "accounts-title", "accounts-filter", "accounts-list", "accounts-new", "accounts-drop", "accounts-refresh", "accounts-review"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Asking again activates the same tab: one per connection.
    oa_menu_on(&rig.app, rig.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-accounts");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Accounts).count()), 1);

    // SQLite and MongoDB do not have the entry; the palette says why for them.
    for (connection, reason) in [(crate::ui_fixture::SQLITE_ID, "SQLite has no users"), (crate::ui_fixture::MONGO_ID, "covers PostgreSQL, MySQL and MariaDB")] {
        assert!(!rig.app.read_with(rig.visual, |this, _| this.accounts_supported(connection)));
        rig.app.update(rig.visual, |this, cx| {
            this.sidebar.notice = None;
            cx.notify();
        });
        rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| this.open_accounts(connection, window, cx)));
        settle(rig.visual);
        let notice = rig.app.read_with(rig.visual, |this, _| this.sidebar.notice.clone());
        assert!(notice.as_deref().is_some_and(|text| text.contains(reason)), "{connection}: {notice:?}");
    }
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Accounts).count()), 1, "no tab for them");
    assert!(rig.app.read_with(rig.visual, |this, _| this.accounts_supported(OA_PG)));
}

#[gpui::test]
fn the_palette_command_opens_the_tab_of_the_connection_selected_in_the_navigator(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.app.update(rig.visual, |this, _| this.navigator.select(NavigatorNodeId::Connection { connection: OA_PG.into() }));
    rig.visual.update(|window, cx| rig.app.update(cx, |this, cx| StaticAction::UsersAndPrivileges.run(this, window, cx)));
    settle(rig.visual);
    rig.wait_rows();
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().kind), TabKind::Accounts);
}

// ---- the list --------------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_roles_are_listed_filtered_and_marked_and_the_selected_one_shows_its_attributes(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.rows, ["app", "boss", "reader", "viewer"]);
    assert_eq!(snapshot.database, "warehouse");
    assert!(rig.visual.debug_bounds("accounts-nothing-selected").is_some());
    assert!(rig.visual.debug_bounds("accounts-subtitle").is_some());

    // The filter narrows the list as it is typed.
    rig.type_into("accounts-filter", "RE");
    assert_eq!(rig.snapshot().rows, ["reader"]);
    rig.type_into("accounts-filter", "zzz");
    assert!(rig.snapshot().rows.is_empty());
    rig.type_into("accounts-filter", "");
    assert_eq!(rig.snapshot().rows.len(), 4);

    // A row click selects it: the attribute controls show its attributes.
    rig.select_row("boss");
    assert_eq!(rig.snapshot().selected.as_deref(), Some("boss"));
    for selector in ["accounts-attr-login", "accounts-attr-superuser", "accounts-attr-createdb", "accounts-attr-createrole", "accounts-attr-inherit", "accounts-attr-replication", "accounts-attr-bypassrls", "accounts-attr-limit", "accounts-attr-valid-until", "accounts-password", "accounts-password-clear", "accounts-own-warning"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    rig.select_row("viewer");
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.inputs[0], "3", "the connection limit of the role selected");
    assert!(matches!(snapshot.working, Some(crate::sql_account::Attributes::Postgres(attributes)) if attributes.login && !attributes.superuser));
    assert!(rig.visual.debug_bounds("accounts-own-warning").is_none(), "only the login role carries the warning");
    assert_eq!(rig.snapshot().pending, 0);
}

#[gpui::test]
fn a_catalog_the_account_may_not_read_shows_the_servers_words_and_retry_reads_it_again(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.server.lock().unwrap().denied = Some("permission denied for table pg_roles".into());
    oa_menu_on(&rig.app, rig.visual, &NavigatorNodeId::Connection { connection: OA_PG.into() }, "connection-menu-accounts");
    let view = rig.view();
    wait_until(rig.visual, "the failure", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).error.is_some()));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("accounts-error").is_some(), "the error is shown, not an empty list");
    assert_eq!(rig.snapshot().error.as_deref(), Some("permission denied for table pg_roles"));
    assert!(rig.visual.debug_bounds("accounts-list").is_none());
    rig.server.lock().unwrap().denied = None;
    rig.click("accounts-error-retry");
    rig.wait_rows();
    assert_eq!(rig.snapshot().rows.len(), 4);
    assert!(rig.visual.debug_bounds("accounts-error").is_none());
}

#[gpui::test]
fn a_long_list_draws_only_the_rows_in_view(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    {
        let mut state = rig.server.lock().unwrap();
        for n in 0..6000 {
            state.roles.push((format!("role_{n:05}"), true, false, false, -1));
        }
    }
    rig.open();
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.total, 6004);
    let (start, end) = snapshot.drawn[0];
    assert!(end > start && end - start < 60, "{:?}: only the visible rows are built", snapshot.drawn);
    // The filter finds one of them.
    rig.type_into("accounts-filter", "role_04999");
    assert_eq!(rig.snapshot().rows, ["role_04999"]);
}

// ---- editing and the review ----------------------------------------------------------------------------------

#[gpui::test]
fn attribute_edits_become_one_reviewed_alter_that_runs_in_a_transaction_and_the_list_is_read_again(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.click("accounts-attr-createdb");
    rig.type_into("accounts-attr-limit", "5");
    assert_eq!(rig.snapshot().pending, 1, "the edits of one role are one ALTER");
    // Nothing is sent while editing.
    assert!(rig.statements().is_empty());
    let loads = rig.server.lock().unwrap().loads;

    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER ROLE \"app\" WITH CREATEDB CONNECTION LIMIT 5;");
    assert!(rig.visual.debug_bounds("changes-mode").is_some(), "PostgreSQL: all of it or none of it");
    assert!(rig.visual.debug_bounds("changes-understood").is_none(), "an alter loses nothing");
    assert!(rig.statements().is_empty(), "the review sends nothing");
    rig.apply_and_wait();
    let ran = rig.statements();
    assert_eq!(ran.first().map(String::as_str), Some("BEGIN"), "{ran:?}");
    assert_eq!(ran.last().map(String::as_str), Some("COMMIT"), "{ran:?}");
    assert!(ran.contains(&"ALTER ROLE \"app\" WITH CREATEDB CONNECTION LIMIT 5".to_owned()), "{ran:?}");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "the dialog closed after the apply");

    // The roles were read again, and the edits are gone.
    let view = rig.view();
    wait_until(rig.visual, "the list to be read again", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    settle(rig.visual);
    assert!(rig.server.lock().unwrap().loads > loads);
    assert_eq!(rig.snapshot().pending, 0);
}

#[gpui::test]
fn a_flag_put_back_and_a_discard_leave_nothing_to_review(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("viewer");
    rig.click("accounts-attr-superuser");
    assert_eq!(rig.snapshot().pending, 1);
    rig.click("accounts-attr-superuser");
    assert_eq!(rig.snapshot().pending, 0, "put back: no edit");
    rig.click("accounts-attr-replication");
    rig.click("accounts-discard");
    assert_eq!(rig.snapshot().pending, 0);
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "nothing to review: no dialog");
    // Edits stay with their role while another is looked at.
    rig.select_row("reader");
    rig.click("accounts-attr-login");
    rig.select_row("app");
    assert_eq!(rig.snapshot().pending, 1);
    rig.select_row("reader");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER ROLE \"reader\" WITH LOGIN;");
}

#[gpui::test]
fn a_bad_number_is_said_in_the_footer_and_blocks_the_review(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.type_into("accounts-attr-limit", "many");
    assert!(rig.snapshot().field_error.is_some_and(|text| text.contains("connection limit")), "the field says what is wrong");
    // Nothing valid was edited, so there is nothing to review; a valid edit elsewhere does not carry the bad number along.
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-subject").is_none());
    rig.click("accounts-attr-createdb");
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "the bad field blocks the review");
    assert!(rig.snapshot().notice.is_some_and(|(text, failed)| failed && text.contains("connection limit")));
    rig.type_into("accounts-attr-limit", "7");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER ROLE \"app\" WITH CREATEDB CONNECTION LIMIT 7;");
}

#[gpui::test]
fn a_password_is_hidden_in_the_review_and_goes_to_the_driver_only_as_a_scram_verifier(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.type_into("accounts-password", AC_PASSWORD);
    assert!(rig.snapshot().password_typed);
    rig.click("accounts-review");
    // The password left the form for the review; what the review shows and copies hides it.
    assert!(!rig.snapshot().password_typed, "the password field is emptied once the review has the password");
    assert_eq!(rig.script(), "ALTER ROLE \"app\" WITH PASSWORD '********';");
    let host = oa_host(&rig.app, rig.visual);
    let password = host.read_with(rig.visual, |host, _| {
        host.draft_changes().iter().find_map(|change| match change {
            crate::sql_ddl::Change::Account(account) => account.password().cloned(),
            _ => None,
        })
    });
    let password = password.expect("the change carries the password");
    assert!(password.is_present());
    assert!(format!("{host:?}", host = host.read_with(rig.visual, |host, _| host.draft_changes().to_vec())).find(AC_PASSWORD).is_none());
    rig.apply_and_wait();

    let ran = rig.statements();
    let alter = ran.iter().find(|sql| sql.starts_with("ALTER ROLE")).expect("the alter ran");
    assert!(alter.starts_with("ALTER ROLE \"app\" WITH PASSWORD 'SCRAM-SHA-256$4096:"), "{alter}");
    assert!(ran.iter().all(|sql| !sql.contains(AC_PASSWORD)), "the password itself never reaches the driver: {ran:?}");
    // The apply ended: the password is wiped, wherever it was copied to.
    assert!(!password.is_present());
    // No log of the app holds it or its verifier.
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    assert!(!log.contains(AC_PASSWORD) && !log.contains("SCRAM-SHA-256"), "{log}");
    let history = rig.app.read_with(rig.visual, |this, _| format!("{:?}", this.recorded_history));
    assert!(!history.contains(AC_PASSWORD) && !history.contains("SCRAM-SHA-256"));
}

#[gpui::test]
fn a_failed_apply_neither_shows_nor_logs_the_password_even_when_the_server_repeats_the_statement(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    *rig.fail.lock().unwrap() = Some("PASSWORD");
    rig.open();
    rig.select_row("app");
    rig.type_into("accounts-password", AC_PASSWORD);
    rig.click("accounts-review");
    let host = oa_host(&rig.app, rig.visual);
    let password = host.read_with(rig.visual, |host, _| {
        host.draft_changes().iter().find_map(|change| match change {
            crate::sql_ddl::Change::Account(account) => account.password().cloned(),
            _ => None,
        })
    });
    let password = password.expect("a password");
    rig.click("changes-apply");
    oa_wait_host(&rig.app, rig.visual);

    // The dialog says it failed, with the statement hidden.
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed);
    assert!(report.contains("syntax error at or near"), "{report}");
    assert!(report.contains("'********'") || report.contains("********"), "{report}");
    assert!(!report.contains(AC_PASSWORD) && !report.contains("SCRAM-SHA-256"), "{report}");
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    assert!(log.contains("syntax error"), "the failure is logged: {log}");
    assert!(!log.contains(AC_PASSWORD) && !log.contains("SCRAM-SHA-256"), "{log}");
    // The password is gone: a retry cannot run without typing it again.
    assert!(!password.is_present());
    assert!(rig.visual.debug_bounds("changes-error-0").is_some(), "the change says the password has to be typed again");
    *rig.fail.lock().unwrap() = None;
    let ran = rig.statements();
    rig.click("changes-apply");
    assert_eq!(rig.statements(), ran, "nothing more ran: Apply is not possible until the password is typed again");
}

#[gpui::test]
fn closing_the_review_wipes_the_password_and_the_form_asks_for_it_again(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.type_into("accounts-password", AC_PASSWORD);
    rig.click("accounts-review");
    let host = oa_host(&rig.app, rig.visual);
    let password = host.read_with(rig.visual, |host, _| {
        host.draft_changes().iter().find_map(|change| match change {
            crate::sql_ddl::Change::Account(account) => account.password().cloned(),
            _ => None,
        })
    });
    let password = password.expect("a password");
    assert!(password.is_present());
    rig.click("changes-close");
    assert!(rig.visual.debug_bounds("changes-subject").is_none());
    assert!(!password.is_present(), "closing the review wipes the password");
    assert!(!rig.snapshot().password_typed);
    assert!(rig.statements().is_empty());
}

#[gpui::test]
fn removing_a_password_is_a_postgres_only_alter_and_the_field_is_disabled_meanwhile(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("viewer");
    rig.click("accounts-password-clear");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER ROLE \"viewer\" WITH PASSWORD NULL;");
}

// ---- production, read-only, closing ----------------------------------------------------------------------------------

#[gpui::test]
fn a_production_connection_asks_for_its_confirmation_and_a_read_only_one_refuses_the_edit(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let mut rig = ac_pg(cx);
    publish_types(&rig.app, rig.visual, false);
    give_profile(&rig.app, rig.visual, OA_PG, typed(PRODUCTION));
    rig.open();
    rig.select_row("viewer");
    rig.click("accounts-attr-createdb");
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-confirm").is_some(), "Production asks for a named confirmation");
    rig.click("changes-apply");
    assert!(rig.statements().is_empty(), "without the confirmation nothing runs");
    rig.click("changes-confirm");
    rig.apply_and_wait();
    assert!(rig.statements().iter().any(|sql| sql.starts_with("ALTER ROLE \"viewer\"")));

    // Read-only: the review says why and nothing can run.
    rig.ran.lock().unwrap().clear();
    publish_types(&rig.app, rig.visual, true);
    let view = rig.view();
    wait_until(rig.visual, "the list", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    rig.select_row("viewer");
    rig.click("accounts-attr-createrole");
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-disabled").is_some(), "the read-only reason is shown");
    rig.click("changes-confirm");
    rig.click("changes-apply");
    assert!(rig.statements().is_empty(), "a read-only connection refuses");
}

#[gpui::test]
fn a_tab_with_unapplied_edits_counts_as_having_pending_changes(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    let tab = rig.app.read_with(rig.visual, |this, _| this.workspace.active_tab().unwrap().id);
    assert!(!rig.app.read_with(rig.visual, |this, cx| this.accounts_dirty(tab, cx)));
    rig.select_row("app");
    rig.type_into("accounts-password", AC_PASSWORD);
    assert!(rig.app.read_with(rig.visual, |this, cx| this.accounts_dirty(tab, cx)), "a typed password is an edit");
    rig.click("accounts-discard");
    assert!(!rig.app.read_with(rig.visual, |this, cx| this.accounts_dirty(tab, cx)));
    rig.click("accounts-attr-createdb");
    assert!(rig.app.read_with(rig.visual, |this, cx| this.accounts_dirty(tab, cx)));
}

// ---- membership and privileges ---------------------------------------------------------------------------------------

#[gpui::test]
fn membership_is_toggled_by_clicking_roles_and_reviewed_as_grant_and_revoke(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.panel("accounts-tab-membership");
    assert_eq!(rig.snapshot().panel, crate::app::accounts::Panel::Membership);
    for selector in ["accounts-membership", "accounts-roles", "accounts-detail-filter", "accounts-members"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    // Every other role is listed: boss, reader, viewer. `app` is already in `reader`.
    for selector in ["accounts-role-0", "accounts-role-1", "accounts-role-2"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("accounts-role-3").is_none(), "a role is not offered itself");
    rig.click("accounts-role-0");
    rig.click("accounts-role-1");
    assert_eq!(rig.snapshot().pending, 2);
    rig.click("accounts-review");
    assert_eq!(rig.script(), "GRANT \"boss\" TO \"app\";\nREVOKE \"reader\" FROM \"app\";");
    rig.apply_and_wait();
    let ran = rig.statements();
    assert!(ran.contains(&"GRANT \"boss\" TO \"app\"".to_owned()) && ran.contains(&"REVOKE \"reader\" FROM \"app\"".to_owned()), "{ran:?}");
    // The server holds them now: nothing is left pending.
    let view = rig.view();
    wait_until(rig.visual, "the list to be read again", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    settle(rig.visual);
    assert_eq!(rig.snapshot().pending, 0, "the memberships that ran are no edit any more");

    // The filter narrows the roles, and the members of the selected role are told.
    let view = rig.view();
    wait_until(rig.visual, "the list", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    settle(rig.visual);
    rig.select_row("reader");
    rig.panel("accounts-tab-membership");
    assert!(rig.visual.debug_bounds("accounts-members").is_some());
}

#[gpui::test]
fn the_privileges_of_a_role_are_edited_with_real_checkboxes_on_the_selected_object_only(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    // No privilege control exists before an object is chosen.
    rig.click("accounts-tab-privileges");
    let view = rig.view();
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("accounts-database").is_some(), "PostgreSQL: the database is chosen here");
    assert!(rig.visual.debug_bounds("accounts-objects").is_some());
    assert!(rig.visual.debug_bounds("accounts-priv-select").is_none());
    let snapshot = rig.snapshot();
    // The two databases, the two schemas and the three tables; the lists are rows, not controls.
    assert_eq!(snapshot.objects, ["other", "warehouse", "audit", "public", "public.customers", "public.items", "public.orders"]);
    assert_eq!(snapshot.database, "warehouse");

    // A table: its seven privileges, each with a grant-option box. SELECT and INSERT are held (INSERT with the option).
    let orders = snapshot.objects.iter().position(|label| label == "public.orders").unwrap();
    let selector: &'static str = Box::leak(format!("accounts-object-{orders}").into_boxed_str());
    rig.click(selector);
    for name in ["select", "insert", "update", "delete", "truncate", "references", "trigger"] {
        for prefix in ["accounts-priv-", "accounts-priv-option-"] {
            let id: &'static str = Box::leak(format!("{prefix}{name}").into_boxed_str());
            assert!(rig.visual.debug_bounds(id).is_some(), "{id}");
        }
    }
    assert!(rig.visual.debug_bounds("accounts-priv-usage").is_none(), "a schema's privilege is not a table's");
    // Edit: INSERT off, DELETE on, SELECT with grant option.
    rig.click("accounts-priv-insert");
    rig.click("accounts-priv-delete");
    rig.click("accounts-priv-option-select");
    assert_eq!(rig.snapshot().pending, 3);
    rig.click("accounts-review");
    assert_eq!(
        rig.script(),
        "REVOKE INSERT ON TABLE \"public\".\"orders\" FROM \"app\";\nGRANT DELETE ON TABLE \"public\".\"orders\" TO \"app\";\nGRANT SELECT ON TABLE \"public\".\"orders\" TO \"app\" WITH GRANT OPTION;"
    );
    rig.apply_and_wait();
    let ran = rig.statements();
    assert!(ran.contains(&"REVOKE INSERT ON TABLE \"public\".\"orders\" FROM \"app\"".to_owned()), "{ran:?}");
    assert_eq!(ran.first().map(String::as_str), Some("BEGIN"));
    // The privileges are read again for this account and compared with the edits: what ran is no edit any more.
    let view = rig.view();
    wait_until(rig.visual, "the privileges to be read again", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready && !view.snapshot(cx).loading));
    settle(rig.visual);
    assert_eq!(rig.snapshot().pending, 0);
    assert!(!view.read_with(rig.visual, |view, cx| view.has_edits(cx)), "and the form no longer counts as edited");
}

#[gpui::test]
fn a_schema_and_a_database_have_their_own_privileges_and_all_none_buttons_set_every_box(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("viewer");
    rig.click("accounts-tab-privileges");
    let view = rig.view();
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    let objects = rig.snapshot().objects;
    let position = |label: &str| objects.iter().position(|object| object == label).unwrap();
    let schema: &'static str = Box::leak(format!("accounts-object-{}", position("audit")).into_boxed_str());
    rig.click(schema);
    assert!(rig.visual.debug_bounds("accounts-priv-usage").is_some() && rig.visual.debug_bounds("accounts-priv-create").is_some());
    assert!(rig.visual.debug_bounds("accounts-priv-select").is_none());
    rig.click("accounts-priv-usage");
    let database: &'static str = Box::leak(format!("accounts-object-{}", position("other")).into_boxed_str());
    rig.click(database);
    for id in ["accounts-priv-connect", "accounts-priv-create", "accounts-priv-temporary"] {
        assert!(rig.visual.debug_bounds(id).is_some(), "{id}");
    }
    rig.click("accounts-priv-all");
    rig.click("accounts-review");
    assert_eq!(
        rig.script(),
        "GRANT CONNECT, CREATE, TEMPORARY ON DATABASE \"other\" TO \"viewer\";\nGRANT USAGE ON SCHEMA \"audit\" TO \"viewer\";"
    );
}

#[gpui::test]
fn the_objects_filter_narrows_the_list_and_a_long_list_draws_only_the_rows_in_view(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    {
        let mut state = rig.server.lock().unwrap();
        for n in 0..5000 {
            state.tables.push(("big".into(), format!("t_{n:05}")));
        }
    }
    rig.open();
    rig.select_row("app");
    rig.click("accounts-tab-privileges");
    let view = rig.view();
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    let snapshot = rig.snapshot();
    // The list stops at the limit (the server has more tables than it lists).
    assert_eq!(snapshot.objects.len(), 2 + 2 + crate::sql_account::catalog::TABLE_LIMIT);
    let (start, end) = snapshot.drawn[2];
    assert!(end > start && end - start < 60, "{:?}", snapshot.drawn);
    rig.type_into("accounts-detail-filter", "t_04990");
    assert_eq!(rig.snapshot().objects, ["big.t_04990"]);
    rig.type_into("accounts-detail-filter", "orders");
    assert_eq!(rig.snapshot().objects, ["public.orders"]);
}

#[gpui::test]
fn another_database_reads_its_own_schemas_and_is_not_offered_while_privilege_edits_are_pending(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.click("accounts-tab-privileges");
    let view = rig.view();
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    assert_eq!(rig.server.lock().unwrap().privilege_reads.last().cloned().flatten().as_deref(), Some("warehouse"));
    // Choose `other` in the select (open it, one down, enter).
    rig.click("accounts-database");
    rig.visual.simulate_keystrokes("up enter");
    settle(rig.visual);
    wait_until(rig.visual, "the privileges of the other database", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    assert_eq!(rig.snapshot().database, "other");
    assert_eq!(rig.server.lock().unwrap().privilege_reads.last().cloned().flatten().as_deref(), Some("other"));
}

// ---- New and Drop --------------------------------------------------------------------------------------------------------

#[gpui::test]
fn a_new_role_is_checked_by_the_dialog_reviewed_with_its_password_hidden_and_listed_after_the_apply(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.click("accounts-new");
    for selector in ["accounts-new-name", "accounts-new-password", "accounts-new-confirm", "accounts-new-login", "accounts-new-superuser", "accounts-new-createdb"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let refused = |rig: &mut AcRig<'_>| {
        rig.click("accounts-new-submit");
        assert!(rig.visual.debug_bounds("accounts-new-error").is_some(), "the dialog says what is wrong");
        assert!(rig.visual.debug_bounds("accounts-new-name").is_some(), "and stays open");
        assert!(rig.app.read_with(rig.visual, |this, _| this.object_action.is_none()), "no review");
    };
    // No name; a name in use; a reserved name; two different passwords.
    refused(&mut rig);
    rig.type_into("accounts-new-name", "app");
    refused(&mut rig);
    rig.type_into("accounts-new-name", "pg_report");
    refused(&mut rig);
    rig.type_into("accounts-new-name", "report ro");
    rig.type_into("accounts-new-password", AC_PASSWORD);
    rig.type_into("accounts-new-confirm", "something else");
    refused(&mut rig);
    rig.type_into("accounts-new-confirm", AC_PASSWORD);
    rig.click("accounts-new-createdb");
    rig.click("accounts-new-submit");
    assert!(rig.visual.debug_bounds("accounts-new-name").is_none(), "the dialog is gone");
    assert_eq!(rig.script(), "CREATE ROLE \"report ro\" WITH LOGIN CREATEDB PASSWORD '********';");
    rig.apply_and_wait();
    let ran = rig.statements();
    let create = ran.iter().find(|sql| sql.starts_with("CREATE ROLE")).expect("ran");
    assert!(create.contains("PASSWORD 'SCRAM-SHA-256$") && !create.contains(AC_PASSWORD), "{create}");
    // The list is read again and has it.
    let view = rig.view();
    wait_until(rig.visual, "the new role", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).rows.iter().any(|row| row == "report ro")));
}

#[gpui::test]
fn dropping_a_role_offers_reassign_and_drop_owned_as_explicit_steps_of_the_change_set(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("reader");
    rig.click("accounts-drop");
    assert!(rig.visual.debug_bounds("accounts-drop-reassign").is_some() && rig.visual.debug_bounds("accounts-drop-owned").is_some());
    assert!(rig.visual.debug_bounds("accounts-drop-target").is_none(), "the new owner is chosen only when reassigning");
    assert!(rig.visual.debug_bounds("accounts-drop-own").is_none());
    // Without the steps: a plain DROP ROLE, with the warning that it fails while the role owns something.
    rig.click("accounts-drop-submit");
    assert_eq!(rig.script(), "DROP ROLE \"reader\";");
    assert!(rig.visual.debug_bounds("changes-understood").is_some());
    rig.click("changes-close");

    rig.click("accounts-drop");
    rig.click("accounts-drop-reassign");
    assert!(rig.visual.debug_bounds("accounts-drop-target").is_some());
    rig.click("accounts-drop-owned");
    rig.click("accounts-drop-submit");
    assert_eq!(rig.script(), "REASSIGN OWNED BY \"reader\" TO \"app\";\nDROP OWNED BY \"reader\";\nDROP ROLE \"reader\";");
    // Each step is a change of its own that can be discarded; DROP OWNED says what it deletes.
    assert!(rig.visual.debug_bounds("changes-item-2").is_some());
    rig.click("changes-understood");
    rig.apply_and_wait();
    let ran = rig.statements();
    let order: Vec<&str> = ran.iter().map(String::as_str).filter(|sql| sql.contains("OWNED") || sql.starts_with("DROP ROLE")).collect();
    assert_eq!(order, ["REASSIGN OWNED BY \"reader\" TO \"app\"", "DROP OWNED BY \"reader\"", "DROP ROLE \"reader\""]);
    let view = rig.view();
    wait_until(rig.visual, "the role to be gone", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).rows.iter().any(|row| row == "reader")));
}

#[gpui::test]
fn dropping_or_locking_out_the_login_role_carries_an_extra_explicit_warning(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("boss");
    rig.click("accounts-drop");
    assert!(rig.visual.debug_bounds("accounts-drop-own").is_some(), "the dialog names the login account");
    rig.click("accounts-drop-submit");
    assert_eq!(rig.script(), "DROP ROLE \"boss\";");
    assert!(rig.visual.debug_bounds("changes-warning-0").is_some());
    // It is destructive: Apply waits for the acknowledgement.
    rig.click("changes-apply");
    assert!(rig.statements().is_empty());
    rig.click("changes-close");

    // Removing the login of the same role warns as well.
    rig.click("accounts-attr-login");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER ROLE \"boss\" WITH NOLOGIN;");
    assert!(rig.visual.debug_bounds("changes-understood").is_some(), "locking yourself out needs the acknowledgement");
}

// ---- MySQL and MariaDB ---------------------------------------------------------------------------------------------------

/// The fixture app with a MySQL (or MariaDB) connection, whose tab reads from `server`; returns the rig and the connection's key.
fn ac_mysql<'a>(cx: &'a mut TestAppContext, kind: ConnectionKind, server: AcServer) -> (AcRig<'a>, String) {
    let (app, visual, _, _) = oa_pg(cx);
    let (ran, fail, echo) = ac_wire(&app, visual, &server);
    let id = uuid::Uuid::new_v4();
    let key = id.to_string();
    let scheme = if kind == ConnectionKind::Mariadb { "mariadb" } else { "mysql" };
    app.update(visual, |this, cx| {
        this.saved_connections.push(SavedConnection {
            id,
            name: "MySQL fixture".into(),
            uri: format!("{scheme}://root@127.0.0.1:3306/shop"),
            kind,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        cx.notify();
    });
    settle(visual);
    (AcRig { app, visual, ran, fail, echo, server }, key)
}

impl AcRig<'_> {
    fn open_connection(&mut self, key: &str) {
        let key = key.to_owned();
        self.visual.update(|window, cx| self.app.update(cx, |this, cx| this.open_accounts(&key, window, cx)));
        settle(self.visual);
        self.wait_rows();
    }
}

#[gpui::test]
fn mysql_accounts_are_listed_with_their_host_and_roles_are_told_apart(cx: &mut TestAppContext) {
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    rig.open_connection(&key);
    assert_eq!(rig.snapshot().rows, ["analyst@%", "app@10.0.%", "root@localhost"]);
    // A role has no password or limits.
    rig.select_row("analyst@%");
    assert!(rig.visual.debug_bounds("accounts-role-note").is_some());
    assert!(rig.visual.debug_bounds("accounts-password").is_none());
    // An account: lock, expiry, the four limits, the password, and the plugin shown but not edited.
    rig.select_row("app@10.0.%");
    for selector in ["accounts-attr-locked", "accounts-attr-expire", "accounts-attr-max-queries", "accounts-attr-max-updates", "accounts-attr-max-connections", "accounts-attr-max-user", "accounts-password", "accounts-plugin", "accounts-expired"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("accounts-attr-login").is_none(), "no PostgreSQL attribute here");
    assert!(rig.visual.debug_bounds("accounts-password-clear").is_none(), "MySQL accounts cannot lose the password here");
    // The server's mode was learned for the review's quoting.
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.diagram.sql_modes.get(&key).cloned()).as_deref(), Some("STRICT_TRANS_TABLES"));
}

#[gpui::test]
fn a_mysql_alter_is_reviewed_without_a_transaction_and_the_password_reaches_the_driver_only(cx: &mut TestAppContext) {
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.click("accounts-attr-locked");
    rig.type_into("accounts-attr-max-queries", "100");
    rig.type_into("accounts-password", "pa\\ss'word");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER USER 'app'@'10.0.%' IDENTIFIED BY '********' WITH MAX_QUERIES_PER_HOUR 100 ACCOUNT LOCK;");
    assert!(rig.visual.debug_bounds("changes-mode").is_some(), "MySQL cannot roll the statements back");
    assert!(rig.visual.debug_bounds("changes-warning-0").is_some(), "the TLS caution for a password in the statement");
    rig.apply_and_wait();
    let ran = rig.statements();
    assert!(!ran.iter().any(|sql| sql == "BEGIN" || sql == "COMMIT"), "no transaction on MySQL: {ran:?}");
    // The driver gets the password, quoted as MySQL reads it; the logs do not.
    let alter = ran.iter().find(|sql| sql.starts_with("ALTER USER")).unwrap_or_else(|| panic!("{ran:?}"));
    assert_eq!(alter, "ALTER USER 'app'@'10.0.%' IDENTIFIED BY 'pa\\\\ss''word' WITH MAX_QUERIES_PER_HOUR 100 ACCOUNT LOCK");
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    assert!(!log.contains("pa\\ss"), "{log}");
}

#[gpui::test]
fn mysql_privileges_are_global_database_and_table_with_grant_option_as_a_box_of_its_own(cx: &mut TestAppContext) {
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.click("accounts-tab-privileges");
    let view = rig.view();
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("accounts-database").is_none(), "MySQL grants across databases: no database to choose");
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.objects, ["All databases (*.*)", "shop", "sys", "shop.orders"]);
    // The database shop: SELECT and INSERT held. GRANT OPTION is a privilege of its own; there is no per-privilege option.
    let shop = snapshot.objects.iter().position(|label| label == "shop").unwrap();
    let selector: &'static str = Box::leak(format!("accounts-object-{shop}").into_boxed_str());
    rig.click(selector);
    assert!(rig.visual.debug_bounds("accounts-priv-grant-option").is_some());
    assert!(rig.visual.debug_bounds("accounts-priv-option-select").is_none());
    assert!(rig.visual.debug_bounds("accounts-priv-shutdown").is_none(), "a server-wide privilege is not offered on a database");
    rig.click("accounts-priv-select");
    rig.click("accounts-priv-delete");
    rig.click("accounts-priv-grant-option");
    rig.click("accounts-review");
    assert_eq!(
        rig.script(),
        "REVOKE SELECT ON `shop`.* FROM 'app'@'10.0.%';\nGRANT DELETE ON `shop`.* TO 'app'@'10.0.%' WITH GRANT OPTION;"
    );
    rig.apply_and_wait();
    assert!(rig.statements().contains(&"REVOKE SELECT ON `shop`.* FROM 'app'@'10.0.%'".to_owned()), "{:?}", rig.statements());
}

#[gpui::test]
fn mysql_roles_are_the_only_ones_an_account_joins(cx: &mut TestAppContext) {
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.panel("accounts-tab-membership");
    assert!(rig.visual.debug_bounds("accounts-role-0").is_some());
    assert!(rig.visual.debug_bounds("accounts-role-1").is_none(), "the other accounts are not roles");
    rig.click("accounts-role-0");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "REVOKE 'analyst'@'%' FROM 'app'@'10.0.%';");
}

#[gpui::test]
fn a_new_mysql_user_or_role_asks_for_a_host_and_a_password_and_the_dialect_names_the_statement(cx: &mut TestAppContext) {
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    rig.open_connection(&key);
    rig.click("accounts-new");
    for selector in ["accounts-new-name", "accounts-new-kind", "accounts-new-host", "accounts-new-password", "accounts-new-confirm", "accounts-new-locked"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(rig.visual.debug_bounds("accounts-new-login").is_none(), "no PostgreSQL option here");
    rig.type_into("accounts-new-name", "o'brien");
    rig.type_into("accounts-new-host", "10.1.%");
    rig.type_into("accounts-new-password", AC_PASSWORD);
    rig.type_into("accounts-new-confirm", AC_PASSWORD);
    rig.click("accounts-new-submit");
    assert_eq!(rig.script(), "CREATE USER 'o''brien'@'10.1.%' IDENTIFIED BY '********';");
    assert!(rig.visual.debug_bounds("changes-warning-0").is_some());
    rig.click("changes-close");

    // A role: no password.
    rig.click("accounts-new");
    rig.click("accounts-new-kind");
    rig.visual.simulate_keystrokes("down enter");
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("accounts-new-password").is_none(), "a role has no password");
    rig.type_into("accounts-new-name", "auditors");
    rig.click("accounts-new-submit");
    assert_eq!(rig.script(), "CREATE ROLE 'auditors'@'%';");
}

#[gpui::test]
fn dropping_a_mysql_user_says_its_sessions_stay_and_names_a_hostile_account_safely(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    server.lock().unwrap().accounts.push(("a'b\\c d%".into(), "h\\x".into(), false, false));
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, server);
    rig.open_connection(&key);
    rig.select_row("a'b\\c d%@h\\x");
    rig.click("accounts-drop");
    assert!(rig.visual.debug_bounds("accounts-drop-sessions").is_some(), "the dialog says open sessions stay connected");
    assert!(rig.visual.debug_bounds("accounts-drop-reassign").is_none(), "MySQL has no objects owned by an account");
    rig.click("accounts-drop-submit");
    assert_eq!(rig.script(), "DROP USER 'a''b\\\\c d%'@'h\\\\x';");
    assert!(rig.visual.debug_bounds("changes-warning-0").is_some());
    rig.click("changes-close");
    // The login account says so.
    rig.select_row("root@localhost");
    rig.click("accounts-drop");
    assert!(rig.visual.debug_bounds("accounts-drop-own").is_some());
}

#[gpui::test]
fn mariadb_roles_have_no_host_and_the_connection_menu_offers_the_tab(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    {
        let mut state = server.lock().unwrap();
        state.accounts = vec![("analyst".into(), "".into(), false, true), ("app".into(), "%".into(), false, false)];
        state.memberships.clear();
        state.login = "app@%".into();
    }
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mariadb, server);
    assert!(rig.app.read_with(rig.visual, |this, _| this.accounts_supported(&key)));
    rig.open_connection(&key);
    // The fake answers rows of the MySQL shape; a MariaDB role is `analyst` without a host.
    let rows = rig.snapshot().rows;
    assert!(rows.contains(&"app@%".to_owned()), "{rows:?}");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) --------------------------------------
//
// Run with the three URIs of the containers (see sql_ddl/live.rs) and `-- --ignored --test-threads=1`. A test whose URI is set and
// whose server is not as these need fails; one without its URI says so and ends.
//
// PostgreSQL's `tablex` is a superuser. MySQL's and MariaDB's `tablex` only owns its own database, so the containers (these local
// test containers only) are given what the editor needs, once, as their administrator:
//
//   docker exec tablex-test-mysql mysql -uroot -ptablex -e "GRANT CREATE USER, CREATE ROLE, DROP ROLE, ROLE_ADMIN ON *.* TO 'tablex'@'%'; \
//     GRANT SELECT ON mysql.* TO 'tablex'@'%'; GRANT ALL PRIVILEGES ON tablex.* TO 'tablex'@'%' WITH GRANT OPTION;"
//   docker exec tablex-test-mariadb mariadb -uroot -ptablex -e "GRANT CREATE USER ON *.* TO 'tablex'@'%'; \
//     GRANT SELECT ON mysql.* TO 'tablex'@'%'; GRANT ALL PRIVILEGES ON tablex.* TO 'tablex'@'%' WITH GRANT OPTION;"

/// Drops what a live test created, on success and on a panic alike; a statement that fails is ignored (it may name what is gone).
struct AcCleanup(std::sync::Arc<engine_driver_host::SharedDriverSession>, Vec<String>);

impl Drop for AcCleanup {
    fn drop(&mut self) {
        let Ok(runner) = self.0.query_runner() else { return };
        for sql in &self.1 {
            let request = serde_json::to_string(&engine_driver_api::QueryRequest {
                kind: "sql".into(),
                operation_id: runner.operation_id(),
                database: None,
                object: None,
                offset: 0,
                limit: 0,
                payload: serde_json::json!({ "sql": sql, "confirmed": true }),
            })
            .unwrap();
            if let Ok(page) = runner.open_query(&request) {
                runner.release_page(page.page_id);
            }
        }
    }
}

impl AcRig<'_> {
    /// Waits for the apply and fails the test when it did not succeed.
    fn apply_ok(&mut self) {
        self.click("changes-apply");
        oa_wait_host(&self.app, self.visual);
        let host = oa_host(&self.app, self.visual);
        if let Some((report, true)) = host.read_with(self.visual, |host, _| host.review_report()) {
            panic!("the apply failed: {report}");
        }
        // The list is read again.
        let view = self.view();
        wait_until(self.visual, "the list to be read again", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
        settle(self.visual);
    }

    fn wait_row(&mut self, name: &str, present: bool) {
        let (view, name) = (self.view(), name.to_owned());
        wait_until(self.visual, "the list to change", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).rows.contains(&name) == present));
        settle(self.visual);
    }

    fn wait_privileges(&mut self) {
        let view = self.view();
        wait_until(self.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
        settle(self.visual);
    }

    /// Clicks the privileges row of the object called `label`.
    fn select_object(&mut self, label: &str) {
        let position = self.snapshot().objects.iter().position(|object| object == label).unwrap_or_else(|| panic!("{label} is listed: {:?}", self.snapshot().objects.len()));
        let selector: &'static str = Box::leak(format!("accounts-object-{position}").into_boxed_str());
        self.click(selector);
    }

    fn refresh(&mut self) {
        self.click("accounts-refresh");
        let view = self.view();
        wait_until(self.visual, "the list", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading && view.snapshot(cx).total > 0));
        settle(self.visual);
    }
}

fn ac_scalar(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, sql: &str) -> String {
    let mut guard = session.lock().expect("the session");
    crate::app::data_import::writer::first_cell(&mut guard, None, sql).unwrap_or_default()
}

/// A session of `uri` with the user `name` and `password` instead of the test account's.
fn ac_connect_as(
    handle: &std::sync::Arc<engine_driver_host::DriverHandle>,
    uri: &str,
    name: &str,
    password: &str,
) -> Result<crate::app::accounts::DriverCatalog, String> {
    let mut url = url::Url::parse(uri).expect("a URI");
    url.set_username(name).expect("a user");
    url.set_password(Some(password)).expect("a password");
    // A new MySQL account may not use the test database yet: it connects to none.
    if url.scheme() != "postgres" && url.scheme() != "postgresql" {
        url.set_path("");
    }
    handle.connect(&serde_json::json!({ "uri": url.as_str() }).to_string()).map(crate::app::accounts::DriverCatalog::of_session).map_err(|error| error.to_string())
}

fn ac_rows(session: &mut crate::app::accounts::DriverCatalog, sql: &str) -> Result<TextRows, String> {
    session.rows(sql).map_err(|error| error.to_string())
}

/// The whole life of a role / user through the tab on a live server: created with a hostile password (and logged in with), its
/// attributes changed, its password changed (the old one stops working), put in a role, given and refused a privilege on a table, and
/// dropped; the catalog read at each step is the real server's.
fn ac_live(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let postgres = kind == ConnectionKind::Postgres;
    let tag = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (user, group, table, owned) = (format!("cf_r7_{tag}"), format!("cf_r7_g{tag}"), format!("cf_r7_t_{tag}"), format!("cf_r7_o_{tag}"));
    let password = "p'w\"d\\ %_\u{fc}1".to_owned();
    let second = "n3w pass; -- \\'".to_owned();
    let rig = nav_rig(cx, kind, driver, &uri);
    let SqlRig { app, visual, key, session, handle, .. } = rig;
    let mut rig = AcRig { app, visual, ran: Default::default(), fail: Default::default(), echo: Default::default(), server: Default::default() };
    let qualified = if postgres { format!("public.{table}") } else { format!("tablex.{table}") };
    let login = if postgres { "tablex".to_owned() } else { "tablex@%".to_owned() };
    // What a user is called in the list, and how its statements name it.
    let mariadb = kind == ConnectionKind::Mariadb;
    let (user_row, group_row) = if postgres { (user.clone(), group.clone()) } else { (format!("{user}@%"), if mariadb { group.clone() } else { format!("{group}@%") }) };
    let _cleanup = AcCleanup(
        session.clone(),
        if postgres {
            vec![
                format!("DROP TABLE IF EXISTS public.{table}"),
                format!("DROP TABLE IF EXISTS public.{owned}"),
                format!("DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{user}') THEN EXECUTE 'DROP OWNED BY \"{user}\"'; EXECUTE 'DROP ROLE \"{user}\"'; END IF; END $$"),
                format!("DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{group}') THEN EXECUTE 'DROP OWNED BY \"{group}\"'; EXECUTE 'DROP ROLE \"{group}\"'; END IF; END $$"),
            ]
        } else {
            vec![
                format!("DROP TABLE IF EXISTS tablex.{table}"),
                format!("DROP USER IF EXISTS '{user}'@'%'"),
                format!("DROP ROLE IF EXISTS '{group}'@'%'"),
                format!("DROP ROLE IF EXISTS '{group}'"),
            ]
        },
    );
    rig.open_connection(&key);
    assert!(rig.snapshot().rows.contains(&login), "the login account is listed: {:?}", rig.snapshot().rows);

    // 1. New role / user with a hostile password, from the dialog; the review hides the password.
    rig.click("accounts-new");
    rig.type_into("accounts-new-name", &user);
    rig.type_into("accounts-new-password", &password);
    rig.type_into("accounts-new-confirm", &password);
    rig.click("accounts-new-submit");
    let script = rig.script();
    assert!(script.starts_with(&format!("CREATE {} ", if postgres { "ROLE" } else { "USER" })) && script.contains("'********'"), "{script}");
    assert!(!script.contains("p'w") && !script.contains("SCRAM"), "{script}");
    rig.apply_ok();
    rig.wait_row(&user_row, true);
    let flag_sql = if postgres {
        format!("SELECT rolcanlogin::text FROM pg_roles WHERE rolname = '{user}'")
    } else {
        format!("SELECT COUNT(*) FROM mysql.user WHERE User = '{user}' AND Host = '%'")
    };
    assert_eq!(ac_scalar(&session, &flag_sql), if postgres { "true" } else { "1" });

    // 2. The password works exactly as typed (SCRAM verifier / quoted text).
    let mut as_user = ac_connect_as(&handle, &uri, &user, &password).unwrap_or_else(|error| panic!("the new password does not log in: {error}"));
    let who = ac_rows(&mut as_user, if postgres { "SELECT current_user::text" } else { "SELECT CURRENT_USER()" }).unwrap();
    assert_eq!(who[0][0].as_deref(), Some(if postgres { user.as_str() } else { &user_row }));
    drop(as_user);
    assert!(ac_connect_as(&handle, &uri, &user, "not the password").is_err(), "a wrong password does not");

    // 3. Attributes.
    rig.select_row(&user_row);
    if postgres {
        rig.click("accounts-attr-createdb");
        rig.type_into("accounts-attr-limit", "3");
    } else {
        rig.type_into("accounts-attr-max-user", "2");
        rig.click("accounts-attr-locked");
    }
    rig.click("accounts-review");
    let script = rig.script();
    assert_eq!(script, if postgres { format!("ALTER ROLE \"{user}\" WITH CREATEDB CONNECTION LIMIT 3;") } else { format!("ALTER USER '{user}'@'%' WITH MAX_USER_CONNECTIONS 2 ACCOUNT LOCK;") });
    rig.apply_ok();
    if postgres {
        assert_eq!(ac_scalar(&session, &format!("SELECT rolcreatedb::text || ':' || rolconnlimit FROM pg_roles WHERE rolname = '{user}'")), "true:3");
    } else {
        assert_eq!(ac_scalar(&session, &format!("SELECT max_user_connections FROM mysql.user WHERE User = '{user}'")), "2");
        assert!(ac_connect_as(&handle, &uri, &user, &password).is_err(), "a locked account does not log in");
        // The list says so after it is read again, and Unlock reverses it.
        rig.select_row(&user_row);
        let view = rig.view();
        let locked = view.read_with(rig.visual, |view, cx| matches!(view.snapshot(cx).working, Some(crate::sql_account::Attributes::Mysql(attributes)) if attributes.locked));
        assert!(locked, "the read-back attributes say locked");
        rig.click("accounts-attr-locked");
        rig.click("accounts-review");
        assert_eq!(rig.script(), format!("ALTER USER '{user}'@'%' ACCOUNT UNLOCK;"));
        rig.apply_ok();
        assert!(ac_connect_as(&handle, &uri, &user, &password).is_ok(), "an unlocked account logs in");
    }

    // 4. A new password: the old one stops working.
    rig.select_row(&user_row);
    rig.type_into("accounts-password", &second);
    rig.click("accounts-review");
    assert!(rig.script().ends_with("'********';") && !rig.script().contains("n3w"));
    rig.apply_ok();
    assert!(ac_connect_as(&handle, &uri, &user, &password).is_err(), "the old password no longer works");
    assert!(ac_connect_as(&handle, &uri, &user, &second).is_ok(), "the new one does");

    // 5. Membership: a second role / user type "role", then the first joins it.
    rig.click("accounts-new");
    if !postgres {
        rig.click("accounts-new-kind");
        rig.visual.simulate_keystrokes("down enter");
        settle(rig.visual);
    }
    rig.type_into("accounts-new-name", &group);
    if postgres {
        rig.click("accounts-new-login");
    }
    rig.click("accounts-new-submit");
    rig.apply_ok();
    let group_listed = if postgres { group.clone() } else { group_row.clone() };
    rig.wait_row(&group_listed, true);
    rig.select_row(&user_row);
    rig.panel("accounts-tab-membership");
    let position = rig.snapshot().roles.iter().position(|role| *role == group_listed).unwrap_or_else(|| panic!("{group_listed} is offered: {:?}", rig.snapshot().roles));
    let selector: &'static str = Box::leak(format!("accounts-role-{position}").into_boxed_str());
    rig.click(selector);
    rig.click("accounts-review");
    assert_eq!(rig.script(), if postgres { format!("GRANT \"{group}\" TO \"{user}\";") } else if mariadb { format!("GRANT '{group}' TO '{user}'@'%';") } else { format!("GRANT '{group}'@'%' TO '{user}'@'%';") });
    rig.apply_ok();
    let member_sql = if postgres {
        format!("SELECT pg_has_role('{user}', '{group}', 'member')::text")
    } else {
        format!("SELECT COUNT(*) FROM mysql.role_edges WHERE FROM_USER = '{group}' AND TO_USER = '{user}'")
    };
    if postgres || kind == ConnectionKind::Mysql {
        assert_eq!(ac_scalar(&session, &member_sql), if postgres { "true" } else { "1" });
    } else {
        assert_eq!(ac_scalar(&session, &format!("SELECT COUNT(*) FROM mysql.roles_mapping WHERE Role = '{group}' AND User = '{user}'")), "1");
    }

    // 6. A privilege on a table: given from the Privileges tab, it works; taken back, it does not.
    exec_confirmed(&session, &format!("CREATE TABLE {qualified} (id int)"));
    rig.refresh();
    rig.select_row(&user_row);
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.select_object(&qualified);
    rig.click("accounts-priv-select");
    rig.click("accounts-review");
    let (table_sql, who_sql) = (format!("SELECT count(*) FROM {qualified}"), if postgres { format!("\"{user}\"") } else { format!("'{user}'@'%'") });
    assert!(rig.script().starts_with("GRANT SELECT ON ") && rig.script().ends_with(&format!(" TO {who_sql};")), "{}", rig.script());
    rig.apply_ok();
    let mut as_user = ac_connect_as(&handle, &uri, &user, &second).unwrap();
    assert!(ac_rows(&mut as_user, &table_sql).is_ok(), "SELECT is allowed once granted");
    assert!(ac_rows(&mut as_user, &format!("INSERT INTO {qualified} VALUES (1)")).is_err(), "INSERT was never granted");
    // The grant is read back as held.
    rig.select_row(&user_row);
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.select_object(&qualified);
    rig.click("accounts-priv-select");
    rig.click("accounts-review");
    assert!(rig.script().starts_with("REVOKE SELECT ON "), "{}", rig.script());
    rig.apply_ok();
    assert!(ac_rows(&mut as_user, &table_sql).is_err(), "SELECT is refused once revoked");
    drop(as_user);

    // 7. Drop. PostgreSQL: the role owns a table: DROP is refused without the explicit steps, and with them the table goes to tablex.
    if postgres {
        exec_confirmed(&session, &format!("CREATE TABLE public.{owned} (id int)"));
        exec_confirmed(&session, &format!("ALTER TABLE public.{owned} OWNER TO \"{user}\""));
        rig.select_row(&user_row);
        rig.click("accounts-drop");
        rig.click("accounts-drop-submit");
        assert_eq!(rig.script(), format!("DROP ROLE \"{user}\";"));
        rig.click("changes-understood");
        rig.click("changes-apply");
        oa_wait_host(&rig.app, rig.visual);
        let host = oa_host(&rig.app, rig.visual);
        let report = host.read_with(rig.visual, |host, _| host.review_report());
        assert!(report.as_ref().is_some_and(|(text, failed)| *failed && text.contains("cannot be dropped")), "the server refuses: {report:?}");
        assert_eq!(ac_scalar(&session, &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{user}'")), "1", "nothing was dropped");
        rig.click("changes-close");
        rig.refresh();
        rig.select_row(&user_row);
        rig.click("accounts-drop");
        rig.click("accounts-drop-reassign");
        let others: Vec<String> = rig.snapshot().rows.into_iter().filter(|row| *row != user).collect();
        let to = others.iter().position(|row| row == "tablex").expect("tablex is a candidate");
        rig.click("accounts-drop-target");
        for _ in 0..to {
            rig.visual.simulate_keystrokes("down");
        }
        rig.visual.simulate_keystrokes("enter");
        settle(rig.visual);
        rig.click("accounts-drop-owned");
        rig.click("accounts-drop-submit");
        assert_eq!(rig.script(), format!("REASSIGN OWNED BY \"{user}\" TO \"tablex\";\nDROP OWNED BY \"{user}\";\nDROP ROLE \"{user}\";"));
        rig.click("changes-understood");
        rig.apply_ok();
        assert_eq!(ac_scalar(&session, &format!("SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = '{owned}'")), "tablex", "its table went to the new owner");
        exec_confirmed(&session, &format!("DROP TABLE public.{owned}"));
    } else {
        rig.select_row(&user_row);
        rig.click("accounts-drop");
        rig.click("accounts-drop-submit");
        assert_eq!(rig.script(), format!("DROP USER '{user}'@'%';"));
        rig.click("changes-understood");
        rig.apply_ok();
    }
    rig.wait_row(&user_row, false);
    assert!(ac_connect_as(&handle, &uri, &user, &second).is_err(), "a dropped account does not log in");
    rig.select_row(&group_listed);
    rig.click("accounts-drop");
    rig.click("accounts-drop-submit");
    rig.click("changes-understood");
    rig.apply_ok();
    rig.wait_row(&group_listed, false);

    // Nothing is left of what the test made, and no log of the app holds a password.
    let leftover = ac_scalar(
        &session,
        &if postgres { format!("SELECT count(*) FROM pg_roles WHERE rolname LIKE 'cf\\_r7\\_%{tag}%'") } else { format!("SELECT count(*) FROM mysql.user WHERE User LIKE 'cf\\_r7\\_%{tag}%'") },
    );
    assert_eq!(leftover, "0");
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    for secret in [&password, &second] {
        assert!(!log.contains(secret.as_str()), "{log}");
    }
    assert!(!log.contains("SCRAM-SHA-256"), "{log}");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgres_roles_from_creation_to_drop(cx: &mut TestAppContext) {
    ac_live(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_users_from_creation_to_drop(cx: &mut TestAppContext) {
    ac_live(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_users_from_creation_to_drop(cx: &mut TestAppContext) {
    ac_live(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// Names made of everything that could end a quoted name or a statement go through the dialogs, the review and a real server:
/// the row read back is the name typed, the statements find it, and (PostgreSQL) a password that SASLprep changes is still the
/// password the server checks.
fn ac_live_hostile(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let postgres = kind == ConnectionKind::Postgres;
    let tag = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let name = format!("cf_r7_h{tag}'\"\\ %\u{fc};--");
    // Typed decomposed, with a no-break space; the server's SASLprep makes it `\u{e9}x y`.
    let typed = "e\u{301}x\u{a0}y-p'w\\".to_owned();
    let normalized = "\u{e9}x y-p'w\\".to_owned();
    let rig = nav_rig(cx, kind, driver, &uri);
    let SqlRig { app, visual, key, session, handle, .. } = rig;
    let mut rig = AcRig { app, visual, ran: Default::default(), fail: Default::default(), echo: Default::default(), server: Default::default() };
    let literal = |text: &str| if postgres { format!("'{}'", text.replace('\'', "''")) } else { format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''")) };
    let _cleanup = AcCleanup(
        session.clone(),
        if postgres {
            vec![format!("DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {}) THEN EXECUTE 'DROP ROLE ' || quote_ident({}); END IF; END $$", literal(&name), literal(&name))]
        } else {
            vec![format!("DROP USER IF EXISTS {}@'%'", literal(&name))]
        },
    );
    rig.open_connection(&key);
    rig.click("accounts-new");
    rig.type_into("accounts-new-name", &name);
    rig.type_into("accounts-new-password", &typed);
    rig.type_into("accounts-new-confirm", &typed);
    rig.click("accounts-new-submit");
    let quoted = if postgres { format!("\"{}\"", name.replace('"', "\"\"")) } else { format!("{}@'%'", literal(&name)) };
    assert!(rig.script().starts_with(&format!("CREATE {} {quoted}", if postgres { "ROLE" } else { "USER" })), "{}", rig.script());
    rig.apply_ok();
    let row = if postgres { name.clone() } else { format!("{name}@%") };
    rig.wait_row(&row, true);
    let count_sql = if postgres { format!("SELECT count(*) FROM pg_roles WHERE rolname = {}", literal(&name)) } else { format!("SELECT count(*) FROM mysql.user WHERE User = {}", literal(&name)) };
    assert_eq!(ac_scalar(&session, &count_sql), "1", "the name reached the server as typed");
    if postgres {
        assert!(ac_connect_as(&handle, &uri, &name, &normalized).is_ok(), "SASLprep: the server checks the normalized form of what was typed");
        assert!(ac_connect_as(&handle, &uri, &name, &typed).is_ok(), "and the typed form maps to the same");
    }

    // An alteration finds the same account again.
    rig.select_row(&row);
    if postgres {
        rig.type_into("accounts-attr-limit", "4");
    } else {
        rig.type_into("accounts-attr-max-user", "4");
    }
    rig.click("accounts-review");
    let alter = rig.script();
    assert!(alter.contains(&quoted) && alter.contains(if postgres { "CONNECTION LIMIT 4" } else { "MAX_USER_CONNECTIONS 4" }), "{alter}");
    rig.apply_ok();
    let limit_sql = if postgres { format!("SELECT rolconnlimit FROM pg_roles WHERE rolname = {}", literal(&name)) } else { format!("SELECT max_user_connections FROM mysql.user WHERE User = {}", literal(&name)) };
    assert_eq!(ac_scalar(&session, &limit_sql), "4");

    // The privileges of an account with this name are read (a backslash in the name is part of the literal that asks for them).
    rig.select_row(&row);
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.click("accounts-tab-attributes");
    if postgres {
        // A password PostgreSQL's preparation does not normalize (an emoji Unicode 3.2 had no code point for): it hashes the text as typed,
        // trade mark sign and all, and so does the verifier made here.
        let fallback = "Secure\u{2122}\u{1f512}";
        rig.type_into("accounts-password", fallback);
        rig.click("accounts-review");
        rig.apply_ok();
        assert!(ac_connect_as(&handle, &uri, &name, fallback).is_ok(), "the password PostgreSQL hashes as typed logs in");
        assert!(ac_connect_as(&handle, &uri, &name, "SecureTM\u{1f512}").is_err(), "not the normalized one");
    }

    rig.select_row(&row);
    rig.click("accounts-drop");
    rig.click("accounts-drop-submit");
    rig.click("changes-understood");
    rig.apply_ok();
    rig.wait_row(&row, false);
    assert_eq!(ac_scalar(&session, &count_sql), "0");
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    assert!(!log.contains("p'w"), "{log}");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgres_hostile_names_and_a_password_sasl_prep_changes(cx: &mut TestAppContext) {
    ac_live_hostile(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_hostile_names(cx: &mut TestAppContext) {
    ac_live_hostile(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_hostile_names(cx: &mut TestAppContext) {
    ac_live_hostile(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

/// An account that may not read the catalog gets the server's refusal as a typed `Denied`, not a failed or an empty list.
fn ac_live_denied(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    use crate::app::accounts::{CatalogSource, LoadJob};
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let tag = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let limited = format!("cf_r7_np{tag}");
    let rig = nav_rig(cx, kind, driver, &uri);
    let SqlRig { app, visual, key, session, .. } = rig;
    let _cleanup = AcCleanup(session.clone(), vec![format!("DROP USER IF EXISTS '{limited}'@'%'")]);
    exec_confirmed(&session, &format!("CREATE USER '{limited}'@'%' IDENTIFIED BY 'np-pw'"));
    let mut url = url::Url::parse(&uri).unwrap();
    url.set_username(&limited).unwrap();
    url.set_password(Some("np-pw")).unwrap();
    url.set_path("");
    let spec = app.read_with(visual, |this, _| {
        let saved = this.saved_connections.iter().find(|saved| saved.matches_id(&key)).cloned().expect("the connection");
        this.secondary_session_spec(&saved, &key)
    });
    let mut spec = spec.expect("a second session can be opened");
    spec.uri = Some(engine_connect::records::RuntimeUri::new(url.as_str()));
    let engine = crate::sql_account::AccountEngine::of(kind).unwrap();
    let error = match CatalogSource::Driver(Box::new(spec)).run(LoadJob::Accounts { engine }) {
        Ok(_) => panic!("an account without SELECT on mysql.user listed the users"),
        Err(error) => error,
    };
    assert!(matches!(&error, crate::sql_account::catalog::ReadError::Denied(message) if message.to_lowercase().contains("denied")), "{error:?}");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_an_account_that_cannot_read_the_users_is_told_so(cx: &mut TestAppContext) {
    ac_live_denied(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_an_account_that_cannot_read_the_users_is_told_so(cx: &mut TestAppContext) {
    ac_live_denied(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

#[gpui::test]
fn a_message_that_names_the_account_stays_readable_when_the_password_starts_like_its_name(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    server.lock().unwrap().accounts.push(("alice".into(), "%".into(), false, false));
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, server);
    *rig.fail.lock().unwrap() = Some("IDENTIFIED");
    *rig.echo.lock().unwrap() = Some(0);
    rig.open_connection(&key);
    rig.select_row("alice@%");
    rig.type_into("accounts-password", "alice2024");
    rig.click("accounts-review");
    let host = oa_host(&rig.app, rig.visual);
    rig.click("changes-apply");
    oa_wait_host(&rig.app, rig.visual);
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed);
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    for (name, text) in [("report", &report), ("error log", &log)] {
        assert!(text.contains("failed for 'alice'@'%'"), "the account name stays in the {name}: {text}");
        assert!(!text.contains("alice2024"), "{name}: {text}");
    }
}

// ---- the server's echo of a failed statement -------------------------------------------------------------------------

#[gpui::test]
fn a_mysql_error_that_quotes_the_start_of_the_statement_shows_and_logs_none_of_the_password(cx: &mut TestAppContext) {
    let secret = "Zq9!kLm2xPw";
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, ac_mysql_server());
    // The server stumbles at IDENTIFIED and quotes 40 bytes from there: `IDENTIFIED BY 'Zq9!kLm2` is the start of the password.
    *rig.fail.lock().unwrap() = Some("IDENTIFIED");
    *rig.echo.lock().unwrap() = Some(28);
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.type_into("accounts-password", secret);
    rig.click("accounts-review");
    let host = oa_host(&rig.app, rig.visual);
    rig.click("changes-apply");
    oa_wait_host(&rig.app, rig.visual);
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed);
    assert!(report.contains("You have an error in your SQL syntax") && report.contains("SQLSTATE 42000"), "the code stays: {report}");
    let log = rig.app.read_with(rig.visual, |this, _| this.error_log.log.export_text());
    assert!(log.contains("SQLSTATE 42000"), "{log}");
    let chars: Vec<char> = secret.chars().collect();
    for run in chars.windows(4) {
        let run: String = run.iter().collect();
        assert!(!report.contains(&run) && !log.contains(&run), "{run} is in the report or the log:\n{report}\n{log}");
    }
}

#[gpui::test]
fn an_apply_of_several_account_changes_reads_the_catalog_once(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.click("accounts-attr-createdb");
    rig.panel("accounts-tab-membership");
    rig.click("accounts-role-0");
    rig.click("accounts-role-1");
    assert_eq!(rig.snapshot().pending, 3);
    rig.click("accounts-review");
    let before = rig.server.lock().unwrap().loads;
    rig.apply_and_wait();
    let view = rig.view();
    wait_until(rig.visual, "the list to be read again", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    settle(rig.visual);
    // Let any second read that was asked for finish before counting.
    for _ in 0..5 {
        settle(rig.visual);
    }
    assert_eq!(rig.server.lock().unwrap().loads, before + 1, "one session and one read for the whole change-set");
}

// ---- the apply session's sql_mode --------------------------------------------------------------------------------------

#[gpui::test]
fn a_password_runs_quoted_for_the_mode_the_apply_session_reports(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    server.lock().unwrap().sql_mode = "STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES".into();
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, server);
    rig.app.update(rig.visual, |this, _| this.diagram.injected_sql_mode = Some("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES".into()));
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.type_into("accounts-password", "pa\\ss'word");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER USER 'app'@'10.0.%' IDENTIFIED BY '********';");
    rig.apply_and_wait();
    // A backslash is an ordinary character in this mode: it is not doubled.
    assert_eq!(rig.statements().iter().find(|sql| sql.starts_with("ALTER USER")).map(String::as_str), Some("ALTER USER 'app'@'10.0.%' IDENTIFIED BY 'pa\\ss''word'"));
}

#[gpui::test]
fn text_made_for_another_sql_mode_than_the_servers_is_refused_and_reviewed_again(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    server.lock().unwrap().accounts.push(("a\\b".into(), "h".into(), false, false));
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, server);
    // The tab learned the default mode; the session the apply opens says the server reads backslashes literally.
    rig.app.update(rig.visual, |this, _| this.diagram.injected_sql_mode = Some("NO_BACKSLASH_ESCAPES".into()));
    rig.open_connection(&key);
    rig.select_row("a\\b@h");
    rig.click("accounts-attr-locked");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER USER 'a\\\\b'@'h' ACCOUNT LOCK;", "reviewed for the mode the tab knew");
    rig.click("changes-apply");
    oa_wait_host(&rig.app, rig.visual);
    let host = oa_host(&rig.app, rig.visual);
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed && report.contains("differs from the SQL that was reviewed"), "{report}");
    assert!(!rig.statements().iter().any(|sql| sql.starts_with("ALTER USER")), "nothing ran: {:?}", rig.statements());
    // The mode was learned: the dialog now shows the SQL as the server reads it, and that applies.
    settle(rig.visual);
    assert_eq!(rig.script(), "ALTER USER 'a\\b'@'h' ACCOUNT LOCK;");
    rig.apply_and_wait();
    assert!(rig.statements().contains(&"ALTER USER 'a\\b'@'h' ACCOUNT LOCK".to_owned()), "{:?}", rig.statements());
}

#[test]
fn the_statement_is_wiped_from_the_request_that_carried_it() {
    let mut request = engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: engine_driver_api::OperationId(1),
        database: None,
        object: None,
        offset: 0,
        limit: 10,
        payload: serde_json::json!({"sql": format!("ALTER USER 'u' IDENTIFIED BY '{AC_PASSWORD}'"), "confirmed": true}),
    };
    let json = crate::app::structure_edit::apply::request_json(&mut request).expect("serializes");
    assert!(json.expose().contains(AC_PASSWORD), "the driver is given the whole request");
    assert_eq!(request.payload["sql"], "", "the copy of the statement the request holds is wiped");
    assert!(!format!("{request:?}").contains(AC_PASSWORD));
}

// ---- the password stays until the review has opened ------------------------------------------------------------------

#[gpui::test]
fn a_password_typed_stays_in_the_form_when_the_review_cannot_open(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.type_into("accounts-password", AC_PASSWORD);
    // The connection is removed while the tab is open: the review has no context to open in.
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.retain(|saved| !saved.matches_id(OA_PG));
        cx.notify();
    });
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "no review");
    assert!(rig.app.read_with(rig.visual, |this, _| this.object_action.is_none()));
    let snapshot = rig.snapshot();
    assert!(snapshot.password_typed, "the typed password is still there");
    assert_eq!(snapshot.pending, 1, "the password is still a pending change");
    let view = rig.view();
    assert!(view.read_with(rig.visual, |view, cx| view.has_edits(cx)), "and the form still counts as edited");
}

// ---- what did not run stays pending ---------------------------------------------------------------------------------

#[gpui::test]
fn after_a_partial_apply_what_ran_is_clean_and_what_did_not_is_still_pending(cx: &mut TestAppContext) {
    let server = ac_mysql_server();
    // Not the account the connection logs in as: that one asks for an acknowledgement first.
    server.lock().unwrap().login = "ghost@elsewhere".into();
    let (mut rig, key) = ac_mysql(cx, ConnectionKind::Mysql, server);
    rig.open_connection(&key);
    rig.select_row("app@10.0.%");
    rig.type_into("accounts-attr-max-queries", "100");
    rig.select_row("root@localhost");
    rig.type_into("accounts-attr-max-queries", "50");
    assert_eq!(rig.snapshot().pending, 2);
    rig.click("accounts-review");
    // MySQL cannot roll back: the first statement runs, the second is refused by the server.
    *rig.fail.lock().unwrap() = Some("'root'");
    let loads = rig.server.lock().unwrap().loads;
    rig.click("changes-apply");
    oa_wait_host(&rig.app, rig.visual);
    let host = oa_host(&rig.app, rig.visual);
    let (report, failed) = host.read_with(rig.visual, |host, _| host.review_report()).expect("a report");
    assert!(failed && report.contains("ran and stay applied"), "{report}");
    let view = rig.view();
    wait_until(rig.visual, "the list to be read again", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    settle(rig.visual);
    assert!(rig.server.lock().unwrap().loads > loads, "the roles were read again");
    // `app` has its new limit: no edit left. `root` does not: its edit is pending, in its field too.
    let snapshot = rig.snapshot();
    assert_eq!(snapshot.pending, 1, "only the statement that did not run is pending");
    assert_eq!(snapshot.selected.as_deref(), Some("root@localhost"));
    assert_eq!(snapshot.inputs[2], "50");
    rig.click("changes-close");
    rig.select_row("app@10.0.%");
    assert_eq!(rig.snapshot().inputs[2], "100", "the server holds what ran");
    assert_eq!(rig.snapshot().pending, 1);
    // Applying again (the server accepts now) runs what is left, and nothing else.
    *rig.fail.lock().unwrap() = None;
    rig.ran.lock().unwrap().clear();
    rig.click("accounts-review");
    assert_eq!(rig.script(), "ALTER USER 'root'@'localhost' WITH MAX_QUERIES_PER_HOUR 50;");
}

// ---- what other roles granted --------------------------------------------------------------------------------------

#[gpui::test]
fn a_membership_another_role_granted_is_shown_with_its_grantor_and_cannot_be_unticked(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.server.lock().unwrap().foreign_memberships.push(("viewer".into(), "app".into(), "stranger".into()));
    rig.open();
    rig.select_row("app");
    rig.panel("accounts-tab-membership");
    let roles = rig.snapshot().roles;
    let position = roles.iter().position(|role| role == "viewer").expect("viewer is listed");
    let selector: &'static str = Box::leak(format!("accounts-role-{position}").into_boxed_str());
    rig.click(selector);
    // It stays, it is no edit, and the footer says whose it is.
    assert_eq!(rig.snapshot().pending, 0);
    assert!(rig.snapshot().notice.is_some_and(|(text, failed)| failed && text.contains("granted by stranger")));
    // A membership of the connection's own role (reader) is still toggled as before.
    let reader: &'static str = Box::leak(format!("accounts-role-{}", roles.iter().position(|role| role == "reader").unwrap()).into_boxed_str());
    rig.click(reader);
    assert_eq!(rig.snapshot().pending, 1);
    assert!(rig.snapshot().notice.is_none());
}

#[gpui::test]
fn a_privilege_another_role_granted_is_shown_beside_the_editable_ones_and_not_ticked(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.server.lock().unwrap().held.push(["T", "public", "orders", "UPDATE", "false", "stranger", "false"].map(str::to_owned));
    rig.open();
    rig.select_row("app");
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.select_object("public.orders");
    assert!(rig.visual.debug_bounds("accounts-priv-foreign").is_some(), "the grant of the stranger is told");
    // UPDATE is not ticked: the connection's role does not hold it to take back.
    rig.click("accounts-priv-update");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "GRANT UPDATE ON TABLE \"public\".\"orders\" TO \"app\";");
}

/// What another role granted is told, not offered for revoking, on a real PostgreSQL: the server's own `REVOKE` leaves those grants, and
/// the tab knows which grants the connection's role can take back by asking the server.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgres_grants_by_other_roles_are_shown_and_left_alone(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to a dedicated tablex-test-* container URI");
        return;
    };
    let tag = uuid::Uuid::new_v4().to_simple().to_string()[..8].to_owned();
    let (target, grantor, group, table) = (format!("cf_r7_a{tag}"), format!("cf_r7_g{tag}"), format!("cf_r7_m{tag}"), format!("cf_r7_t{tag}"));
    let rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let SqlRig { app, visual, key, session, handle, .. } = rig;
    let mut rig = AcRig { app, visual, ran: Default::default(), fail: Default::default(), echo: Default::default(), server: Default::default() };
    let _cleanup = AcCleanup(
        session.clone(),
        vec![
            format!("DROP TABLE IF EXISTS public.{table}"),
            format!("DROP ROLE IF EXISTS {target}"),
            format!("DROP ROLE IF EXISTS {grantor}"),
            format!("DROP ROLE IF EXISTS {group}"),
        ],
    );
    exec_confirmed(&session, &format!("CREATE ROLE {target}"));
    exec_confirmed(&session, &format!("CREATE ROLE {group}"));
    exec_confirmed(&session, &format!("CREATE ROLE {grantor} LOGIN PASSWORD 'gr-pw'"));
    exec_confirmed(&session, &format!("CREATE TABLE public.{table} (id int)"));
    exec_confirmed(&session, &format!("GRANT SELECT ON public.{table} TO {grantor} WITH GRANT OPTION"));
    exec_confirmed(&session, &format!("GRANT {group} TO {grantor} WITH ADMIN OPTION"));
    // The other role grants SELECT and a membership to the target; the connection's role grants INSERT.
    let mut as_grantor = ac_connect_as(&handle, &uri, &grantor, "gr-pw").expect("the grantor logs in");
    ac_rows(&mut as_grantor, &format!("GRANT SELECT ON public.{table} TO {target}")).expect("granted");
    ac_rows(&mut as_grantor, &format!("GRANT {group} TO {target}")).expect("granted");
    drop(as_grantor);
    exec_confirmed(&session, &format!("GRANT INSERT ON public.{table} TO {target}"));

    rig.open_connection(&key);
    rig.select_row(&target);
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.select_object(&format!("public.{table}"));
    assert!(rig.visual.debug_bounds("accounts-priv-foreign").is_some(), "the grant of {grantor} is told");
    // INSERT (ours) is held and can be taken back; SELECT (theirs) is not offered as held.
    rig.click("accounts-priv-insert");
    rig.click("accounts-review");
    assert_eq!(rig.script(), format!("REVOKE INSERT ON TABLE \"public\".\"{table}\" FROM \"{target}\";"));
    rig.apply_ok();
    assert_eq!(ac_scalar(&session, &format!("SELECT has_table_privilege('{target}', 'public.{table}', 'INSERT')::text")), "false");
    assert_eq!(ac_scalar(&session, &format!("SELECT has_table_privilege('{target}', 'public.{table}', 'SELECT')::text")), "true", "the other role's grant is still there");

    // The membership the other role granted is shown with its grantor and stays.
    rig.select_row(&target);
    rig.panel("accounts-tab-membership");
    let position = rig.snapshot().roles.iter().position(|role| *role == group).expect("the group is listed");
    let selector: &'static str = Box::leak(format!("accounts-role-{position}").into_boxed_str());
    rig.click(selector);
    assert_eq!(rig.snapshot().pending, 0);
    assert!(rig.snapshot().notice.is_some_and(|(text, _)| text.contains(&format!("granted by {grantor}"))));
    assert_eq!(ac_scalar(&session, &format!("SELECT pg_has_role('{target}', '{group}', 'member')::text")), "true");
}

#[gpui::test]
fn privilege_edits_add_no_grants_while_the_privileges_are_read_again_and_come_back_after(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("app");
    rig.click("accounts-tab-privileges");
    rig.wait_privileges();
    rig.select_object("public.orders");
    // INSERT is held (with the grant option): take it away, and give DELETE.
    rig.click("accounts-priv-insert");
    rig.click("accounts-priv-delete");
    assert_eq!(rig.snapshot().pending, 2);
    // A Refresh reads the roles and then the privileges again; the server is slow with the privileges.
    rig.server.lock().unwrap().hold_privileges = true;
    rig.click("accounts-refresh");
    let view = rig.view();
    wait_until(rig.visual, "the roles to be read", |visual| view.read_with(visual, |view, cx| !view.snapshot(cx).loading));
    for _ in 0..5 {
        settle(rig.visual);
    }
    let waiting = rig.snapshot();
    assert!(!waiting.privileges_ready, "the privileges are still being read");
    assert_eq!(waiting.pending, 0, "no grant of everything, and no revoke lost: the privilege edits are not changes yet");
    rig.click("accounts-review");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "nothing to review while the privileges are unknown");
    rig.server.lock().unwrap().hold_privileges = false;
    wait_until(rig.visual, "the privileges", |visual| view.read_with(visual, |view, cx| view.snapshot(cx).privileges_ready));
    settle(rig.visual);
    assert_eq!(rig.snapshot().pending, 2, "both edits are back");
    rig.click("accounts-review");
    assert_eq!(rig.script(), "REVOKE INSERT ON TABLE \"public\".\"orders\" FROM \"app\";\nGRANT DELETE ON TABLE \"public\".\"orders\" TO \"app\";");
}

// ---- the forms stay when the review cannot open ----------------------------------------------------------------------

#[gpui::test]
fn the_new_form_comes_back_with_what_was_typed_when_the_review_cannot_open(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.click("accounts-new");
    rig.type_into("accounts-new-name", "report_ro");
    rig.type_into("accounts-new-password", AC_PASSWORD);
    rig.type_into("accounts-new-confirm", AC_PASSWORD);
    // The connection goes away while the form is open: there is no editing context to review in.
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.retain(|saved| !saved.matches_id(OA_PG));
        cx.notify();
    });
    rig.click("accounts-new-submit");
    assert!(rig.visual.debug_bounds("changes-subject").is_none(), "no review");
    assert!(rig.visual.debug_bounds("accounts-new-name").is_some(), "the form is back");
    assert!(rig.visual.debug_bounds("accounts-new-error").is_some(), "and says why");
    // What was typed is still in the fields: submitting again after the connection is back reviews it with the password.
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.push(SavedConnection {
            id: uuid::Uuid::parse_str(OA_PG).unwrap(),
            name: "Postgres fixture".into(),
            uri: "postgres://localhost/warehouse".into(),
            kind: ConnectionKind::Postgres,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        cx.notify();
    });
    rig.click("accounts-new-submit");
    assert_eq!(rig.script(), "CREATE ROLE \"report_ro\" WITH LOGIN PASSWORD '********';");
}

#[gpui::test]
fn the_drop_form_comes_back_when_the_review_cannot_open(cx: &mut TestAppContext) {
    let mut rig = ac_pg(cx);
    rig.open();
    rig.select_row("reader");
    rig.click("accounts-drop");
    rig.click("accounts-drop-owned");
    rig.app.update(rig.visual, |this, cx| {
        this.saved_connections.retain(|saved| !saved.matches_id(OA_PG));
        cx.notify();
    });
    rig.click("accounts-drop-submit");
    assert!(rig.visual.debug_bounds("changes-subject").is_none());
    assert!(rig.visual.debug_bounds("accounts-drop-owned").is_some() && rig.visual.debug_bounds("accounts-drop-error").is_some());
}
