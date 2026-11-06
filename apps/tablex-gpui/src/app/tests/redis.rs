// Redis on the driver route (P5c). The live tests load the REAL Redis driver
// dylib (built on demand, never stripped) behind a real host session and drive
// the real sidebar rows, dialogs and buttons by their measured bounds. They are
// `#[ignore]`d: they need `TABLEX_TEST_REDIS_URI`, the URI of the dedicated
// `tablex-test-redis` container (e.g. `redis://127.0.0.1:56379`). They use
// database 12 and keys under `tablexp5c:`; every key they create is deleted
// again, pass or fail, and nothing else is ever touched (never FLUSHALL).

use crate::redis::KeyType;

const REDIS_DB: u32 = 12;
const REDIS_PREFIX: &str = "tablexp5c:";
const REDIS_DRIVER: (&str, &str) = ("dbengine-redis", "abi-v2");

fn redis_uri() -> Option<String> {
    let base = std::env::var("TABLEX_TEST_REDIS_URI").ok();
    if base.is_none() {
        eprintln!("skipped: set TABLEX_TEST_REDIS_URI to the tablex-test-redis container URI");
    }
    base.map(|base| format!("{}/{REDIS_DB}", base.trim_end_matches('/')))
}

/// Runs one console command on the session, bypassing the UI.
fn redis_exec(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, command: &str) -> bool {
    let Ok(runner) = session.query_runner() else {
        return false;
    };
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "redis.command".into(),
        operation_id: runner.operation_id(),
        database: Some(REDIS_DB.to_string()),
        object: None,
        offset: 0,
        limit: 0,
        payload: serde_json::json!({ "command": command, "confirmed": true }),
    })
    .unwrap();
    match runner.open_query(&request) {
        Ok(page) => {
            runner.release_page(page.page_id);
            true
        }
        Err(_) => false,
    }
}

/// The text of one `GET`-like reply cell, read through the console route.
fn redis_get(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, command: &str) -> Option<String> {
    let runner = session.query_runner().ok()?;
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "redis.command".into(),
        operation_id: runner.operation_id(),
        database: Some(REDIS_DB.to_string()),
        object: None,
        offset: 0,
        limit: 0,
        payload: serde_json::json!({ "command": command }),
    })
    .unwrap();
    let page = runner.open_query(&request).ok()?;
    let mut guard = session.lock().ok()?;
    guard.adopt_page(&runner, page.page_id).ok()?;
    let lease = guard.acquire_arrow_page(page.page_id, 0, page.row_count).ok()?;
    // SAFETY: the lease holds the Arrow C Data pair the driver exported.
    let imported = unsafe { lease.into_record_batch() }.ok()?;
    let text = crate::grid_arrow::array_value_to_string(imported.batch.column(0).as_ref(), 0);
    drop(imported);
    drop(guard);
    runner.release_page(page.page_id);
    Some(text)
}

/// Deletes every `tablexp5c:` key of the test database when it drops.
struct RedisCleanup {
    session: std::sync::Arc<engine_driver_host::SharedDriverSession>,
}

impl Drop for RedisCleanup {
    fn drop(&mut self) {
        let mut cursor: Option<String> = None;
        loop {
            let request = serde_json::json!({
                "database": REDIS_DB.to_string(),
                "payload": { "pattern": format!("{REDIS_PREFIX}*") },
                "cursor": cursor,
                "limit": 1000,
            })
            .to_string();
            let Ok(page) = self.session.lock().map(|mut s| s.list_nav_nodes(&request)) else {
                return;
            };
            let Ok(page) = page else { return };
            for node in &page.nodes {
                if let Some(raw) = node.payload.get("key_raw").and_then(serde_json::Value::as_str) {
                    if let Some(key) = crate::redis::decode_key(raw).and_then(|k| String::from_utf8(k).ok()) {
                        redis_exec(&self.session, &format!("DEL \"{}\"", key.replace('"', "\\\"")));
                    }
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return,
            }
        }
    }
}

/// A rig on the live Redis driver whose sidebar shows only that connection.
struct RedisRig<'a> {
    rig: SqlRig<'a>,
    _cleanup: RedisCleanup,
    /// The live tests share one database and key prefix: one at a time.
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl<'a> RedisRig<'a> {
    fn new(cx: &'a mut TestAppContext, uri: &str) -> Self {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let rig = nav_rig(cx, ConnectionKind::Redis, REDIS_DRIVER, uri);
        // What a real connect does once the session is up.
        let key = rig.key.clone();
        rig.app.update(rig.visual, |this, _| this.navigator.enable_provider(&key));
        settle(rig.visual);
        let cleanup = RedisCleanup { session: rig.session.clone() };
        Self { rig, _cleanup: cleanup, _serial: serial }
    }

    fn exec(&self, command: &str) {
        assert!(redis_exec(&self.rig.session, command), "{command} ran");
    }

    fn get(&self, command: &str) -> Option<String> {
        redis_get(&self.rig.session, command)
    }

    /// The `database:N` node of the test database.
    fn db_node(&self) -> NavigatorNodeId {
        NavigatorNodeId::Provider { connection: self.rig.key.clone(), node: format!("database:{REDIS_DB}") }
    }

    fn key_node(&self, name: &str) -> NavigatorNodeId {
        NavigatorNodeId::Provider {
            connection: self.rig.key.clone(),
            node: format!("key:{REDIS_DB}:{}", crate::redis::encode_key(name.as_bytes())),
        }
    }

    fn folder_node(&self, prefix: &str) -> NavigatorNodeId {
        NavigatorNodeId::Provider { connection: self.rig.key.clone(), node: format!("ns:{REDIS_DB}:{prefix}") }
    }

    /// Expands connection and database; waits for the first key page.
    fn open_database(&mut self) {
        let key = self.rig.key.clone();
        self.rig.expand(&connection_node(&key));
        let db = self.db_node();
        self.rig.expand(&db);
    }

    fn visible_labels(&self) -> Vec<String> {
        self.rig.labels()
    }

    fn type_into(&mut self, selector: &'static str, text: &str) {
        let bounds = self
            .rig
            .visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is rendered"));
        self.rig.visual.simulate_click(bounds.center(), Default::default());
        settle(self.rig.visual);
        self.rig.visual.simulate_input(text);
        settle(self.rig.visual);
    }

    fn click(&mut self, selector: &'static str) {
        self.rig.click(selector);
    }

    fn active_tab(&self) -> Tab {
        self.rig
            .app
            .read_with(self.rig.visual, |this, _| this.workspace.active_tab().cloned().expect("a tab"))
    }

    /// Waits until the active tab finished its run.
    fn wait_tab_ready(&mut self) {
        let app = self.rig.app.clone();
        wait_until(self.rig.visual, "the key page", |visual| {
            app.read_with(visual, |this, _| {
                this.workspace.active_tab().and_then(|tab| this.workspace.runtime(tab.id)).is_some_and(|runtime| {
                    matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. })
                        && !runtime.redis.info_loading
                        && (runtime.redis.info.is_some() || runtime.redis.info_error.is_some())
                })
            })
        });
        settle(self.rig.visual);
    }

    fn open_key(&mut self, name: &str) {
        let node = self.key_node(name);
        let app = self.rig.app.clone();
        self.rig.app.update(self.rig.visual, |this, cx| {
            assert!(this.open_redis_key_node(&node, cx), "{name} is a loaded key");
        });
        let _ = app;
        self.wait_tab_ready();
    }

    fn grid_cells(&self) -> Vec<Vec<String>> {
        let tab = self.active_tab().id;
        self.rig.app.read_with(self.rig.visual, |this, _| {
            let grid = this.grid(tab);
            (0..grid.row_count())
                .map(|row| {
                    (0..grid.column_count())
                        .map(|column| grid.cell_text(CellPosition { row, column }).map(|t| t.to_string()).unwrap_or_default())
                        .collect()
                })
                .collect()
        })
    }

    fn select_row(&mut self, row: usize) {
        let tab = self.active_tab().id;
        self.rig.app.update(self.rig.visual, |this, cx| {
            this.select_grid_cell(tab, CellPosition { row, column: 0 });
            cx.notify();
        });
        settle(self.rig.visual);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_REDIS_URI (container tablex-test-redis)"]
fn live_redis_connects_lists_databases_and_loads_keys_lazily_with_filter(cx: &mut TestAppContext) {
    let Some(uri) = redis_uri() else { return };
    let mut rig = RedisRig::new(cx, &uri);
    // 22 keys in three namespaces plus one without.
    for index in 0..12 {
        rig.exec(&format!("SET {REDIS_PREFIX}user:{index} v{index}"));
    }
    for index in 0..8 {
        rig.exec(&format!("SET {REDIS_PREFIX}order:{index} o{index}"));
    }
    rig.exec(&format!("HSET {REDIS_PREFIX}cfg:app theme dark"));
    rig.exec(&format!("SET {REDIS_PREFIX}plain x"));
    rig.exec(&format!("LPUSH {REDIS_PREFIX}cfg:queue a"));
    // A small page size makes "Load More" observable.
    rig.rig.app.update(rig.rig.visual, |this, _| this.navigator.set_settings_defaults(false, 5));

    let key = rig.rig.key.clone();
    rig.rig.expand(&connection_node(&key));
    let labels = rig.visible_labels();
    assert!(
        labels.iter().any(|label| label.starts_with(&format!("db{REDIS_DB} ("))),
        "the databases are listed with their key counts: {labels:?}"
    );
    let db = rig.db_node();
    rig.rig.expand(&db);
    // One page of five keys: grouped under the shared namespace folder.
    let labels = rig.visible_labels();
    assert!(labels.iter().any(|label| label == "Load More"), "more keys wait behind a cursor: {labels:?}");
    let folder_label = |labels: &[String]| {
        labels
            .iter()
            .find(|label| label.starts_with("tablexp5c ("))
            .and_then(|label| label.trim_start_matches("tablexp5c (").trim_end_matches(')').parse::<usize>().ok())
            .expect("the namespace folder with its key count")
    };
    let before = folder_label(&labels);
    assert!((5..23).contains(&before), "one page of keys is loaded: {before}");
    rig.rig.click_node(
        &NavigatorNodeId::Provider { connection: key.clone(), node: format!("{}database:{REDIS_DB}", "__tablex_load_more__:") },
        "sidebar-row",
    );
    let (app, db_for_wait) = (rig.rig.app.clone(), db.clone());
    wait_until(rig.rig.visual, "the next key page", |visual| {
        app.read_with(visual, |this, _| !matches!(this.navigator.load_state(&db_for_wait), Some(crate::navigator::NodeLoadState::Loading)))
    });
    settle(rig.rig.visual);
    assert!(folder_label(&rig.visible_labels()) > before, "load more adds keys to the folder");

    // The folder opens without a driver call and lists the keys of the namespace.
    let folder = rig.folder_node(REDIS_PREFIX.trim_end_matches(':'));
    rig.rig.click_node(&folder, "nav-disclosure");
    let labels = rig.visible_labels();
    assert!(labels.iter().any(|label| label.starts_with("user (")), "{labels:?}");

    // The Filter Keys dialog narrows the list with a server-side pattern.
    rig.rig.app.update(rig.rig.visual, |this, cx| {
        let db = db.clone();
        let window = this.driver_runtime.window.expect("the window is known after a render");
        let root = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| this.open_redis_filter_dialog(db, window, cx));
            });
        });
    });
    settle(rig.rig.visual);
    rig.type_into("redis-field-0", &format!("{REDIS_PREFIX}order:*"));
    rig.click("redis-form-ok");
    let (app, db_for_wait) = (rig.rig.app.clone(), db.clone());
    wait_until(rig.rig.visual, "the filtered keys", |visual| {
        app.read_with(visual, |this, _| {
            matches!(this.navigator.load_state(&db_for_wait), Some(crate::navigator::NodeLoadState::Loaded))
        })
    });
    settle(rig.rig.visual);
    let labels = rig.visible_labels();
    assert!(
        labels.iter().all(|label| !label.starts_with("user (") && !label.starts_with("cfg (")),
        "only order keys remain: {labels:?}"
    );
    assert!(labels.iter().any(|label| label.starts_with("tablexp5c (")), "{labels:?}");
    let filter = rig.rig.app.read_with(rig.rig.visual, |this, _| this.navigator.redis_filter(&db));
    assert_eq!(filter.pattern, format!("{REDIS_PREFIX}order:*"));
    assert!(rig.rig.navigator_errors().is_empty(), "{:?}", rig.rig.navigator_errors());
}

impl RedisRig<'_> {
    /// Replaces the text of a dialog field.
    fn fill(&mut self, selector: &'static str, text: &str) {
        let bounds = self
            .rig
            .visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is rendered"));
        self.rig.visual.simulate_click(bounds.center(), Default::default());
        settle(self.rig.visual);
        self.rig.visual.simulate_keystrokes("secondary-a");
        if text.is_empty() {
            self.rig.visual.simulate_keystrokes("backspace");
        } else {
            self.rig.visual.simulate_input(text);
        }
        settle(self.rig.visual);
    }

    /// Clicks OK in the open Redis dialog and waits until it is gone.
    fn submit(&mut self) {
        self.click("redis-form-ok");
        for _ in 0..200 {
            settle(self.rig.visual);
            if self.rig.visual.debug_bounds("redis-form-ok").is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the dialog stayed open");
    }

    /// Waits until the active tab re-read its key after an edit.
    fn wait_reloaded(&mut self) {
        let app = self.rig.app.clone();
        // Let the mutation reach the driver and the re-run start first.
        for _ in 0..40 {
            settle(self.rig.visual);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        wait_until(self.rig.visual, "the key to reload", |visual| {
            app.read_with(visual, |this, _| {
                this.workspace.active_tab().and_then(|tab| this.workspace.runtime(tab.id)).is_some_and(|runtime| {
                    matches!(runtime.query.status(), crate::query::QueryStatus::Ready { .. }) && !runtime.redis.info_loading
                })
            })
        });
        settle(self.rig.visual);
    }

    fn info(&self) -> crate::redis::KeyInfo {
        let tab = self.active_tab().id;
        self.rig.app.read_with(self.rig.visual, |this, _| {
            this.workspace.runtime(tab).and_then(|runtime| runtime.redis.info.clone()).expect("key info")
        })
    }

    /// The page size of the active tab (the footer's Limit).
    fn set_page_size(&mut self, size: i64) {
        let tab = self.active_tab().id;
        self.rig.app.update(self.rig.visual, |this, cx| this.set_output_limit(tab, size, cx));
        self.wait_tab_ready();
    }

    fn console(&mut self, command: &str) -> u64 {
        let key = self.rig.key.clone();
        let tab = self.rig.app.update(self.rig.visual, |this, _| this.open_redis_console_tab(&key, &REDIS_DB.to_string()));
        self.rig.tab = tab;
        settle(self.rig.visual);
        self.run_console(command);
        tab
    }

    fn run_console(&mut self, command: &str) {
        let tab = self.rig.tab;
        self.rig.app.update(self.rig.visual, |this, cx| {
            this.workspace.tab_mut(tab).unwrap().sql = command.into();
            this.run_tab_query(tab, cx);
        });
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_REDIS_URI (container tablex-test-redis)"]
fn live_redis_opens_every_value_type_with_a_typed_view_and_pages(cx: &mut TestAppContext) {
    let Some(uri) = redis_uri() else { return };
    let mut rig = RedisRig::new(cx, &uri);
    rig.exec(&format!(r#"SET {REDIS_PREFIX}json:doc "{{\"name\":\"Ada\",\"tags\":[1,2]}}""#));
    rig.exec(&format!("HSET {REDIS_PREFIX}h:cfg theme dark lang en size 12"));
    for item in 0..25 {
        rig.exec(&format!("RPUSH {REDIS_PREFIX}l:items item{item}"));
    }
    rig.exec(&format!("SADD {REDIS_PREFIX}s:tags red blue"));
    rig.exec(&format!("ZADD {REDIS_PREFIX}z:board 10 ann 20 bob"));
    rig.exec(&format!("XADD {REDIS_PREFIX}x:log 1-1 level info msg boot"));
    rig.exec(&format!("XADD {REDIS_PREFIX}x:log 2-1 level warn msg slow"));
    rig.exec(&format!("EXPIRE {REDIS_PREFIX}json:doc 3000"));
    rig.rig.app.update(rig.rig.visual, |this, _| this.navigator.set_settings_defaults(false, 500));
    rig.open_database();

    // String: the JSON view with a TTL in the header.
    rig.open_key(&format!("{REDIS_PREFIX}json:doc"));
    let tab = rig.active_tab();
    assert_eq!(tab.title, format!("{REDIS_PREFIX}json:doc@db{REDIS_DB}"));
    assert!(tab.query_mode.is_redis_key());
    let info = rig.info();
    assert_eq!(info.key_type(), Some(KeyType::String));
    assert!(info.ttl.is_some_and(|ttl| (2000..=3000).contains(&ttl)), "{info:?}");
    assert_eq!(info.length, Some(r#"{"name":"Ada","tags":[1,2]}"#.len() as i64));
    for selector in ["redis-key-toolbar", "redis-key-type", "redis-key-ttl", "redis-key-facts", "redis-string-view"] {
        assert!(rig.rig.visual.debug_bounds(selector).is_some(), "{selector} is rendered");
    }
    let (app, id) = (rig.rig.app.clone(), tab.id);
    let (view, text) = app.read_with(rig.rig.visual, |this, cx| {
        let runtime = this.workspace.runtime(id).unwrap();
        (runtime.redis.view, this.redis_ui.string_editors[&id].read(cx).value().to_string())
    });
    assert_eq!(view, crate::redis::StringView::Json, "a JSON value opens pretty-printed");
    assert!(text.contains("\n  \"name\": \"Ada\""), "{text}");
    rig.click("redis-view-hex");
    let hex = app.read_with(rig.rig.visual, |this, cx| this.redis_ui.string_editors[&id].read(cx).value().to_string());
    assert!(hex.starts_with("00000000  7b 22 6e 61 6d 65 22 3a"), "{hex}");
    rig.click("redis-view-text");
    let plain = app.read_with(rig.rig.visual, |this, cx| this.redis_ui.string_editors[&id].read(cx).value().to_string());
    assert_eq!(plain, r#"{"name":"Ada","tags":[1,2]}"#);

    // Hash: field / value grid.
    rig.open_key(&format!("{REDIS_PREFIX}h:cfg"));
    assert_eq!(rig.info().key_type(), Some(KeyType::Hash));
    assert_eq!(rig.info().length, Some(3));
    let mut cells = rig.grid_cells();
    cells.sort();
    assert_eq!(
        cells,
        [["lang", "en"], ["size", "12"], ["theme", "dark"]].map(|row| row.map(String::from).to_vec())
    );

    // List: index / value, paged by the footer.
    rig.open_key(&format!("{REDIS_PREFIX}l:items"));
    rig.set_page_size(10);
    assert_eq!(rig.grid_cells().len(), 10);
    assert_eq!(rig.grid_cells()[0], ["0", "item0"]);
    assert_eq!(rig.info().length, Some(25));
    rig.click("page-next");
    rig.wait_tab_ready();
    assert_eq!(rig.grid_cells()[0], ["10", "item10"]);
    rig.click("page-next");
    rig.wait_tab_ready();
    assert_eq!(rig.grid_cells().len(), 5);
    assert_eq!(rig.grid_cells()[4], ["24", "item24"]);

    // Set, sorted set and stream.
    rig.open_key(&format!("{REDIS_PREFIX}s:tags"));
    let mut members = rig.grid_cells();
    members.sort();
    assert_eq!(members, [vec!["blue".to_string()], vec!["red".to_string()]]);
    rig.open_key(&format!("{REDIS_PREFIX}z:board"));
    assert_eq!(rig.grid_cells(), [["ann", "10"], ["bob", "20"]].map(|row| row.map(String::from).to_vec()));
    rig.open_key(&format!("{REDIS_PREFIX}x:log"));
    let entries = rig.grid_cells();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0][0], "1-1");
    assert!(entries[0][1].contains("\"level\"") && entries[0][1].contains("boot"), "{:?}", entries[0]);
    assert!(rig.rig.navigator_errors().is_empty(), "{:?}", rig.rig.navigator_errors());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_REDIS_URI (container tablex-test-redis)"]
fn live_redis_edits_round_trip_through_the_key_mutations(cx: &mut TestAppContext) {
    let Some(uri) = redis_uri() else { return };
    let mut rig = RedisRig::new(cx, &uri);
    let (hash, list, set, zset, stream, text) = (
        format!("{REDIS_PREFIX}h"),
        format!("{REDIS_PREFIX}l"),
        format!("{REDIS_PREFIX}s"),
        format!("{REDIS_PREFIX}z"),
        format!("{REDIS_PREFIX}x"),
        format!("{REDIS_PREFIX}str"),
    );
    rig.exec(&format!("HSET {hash} theme dark"));
    rig.exec(&format!("RPUSH {list} a b"));
    rig.exec(&format!("SADD {set} red"));
    rig.exec(&format!("ZADD {zset} 1 ann"));
    rig.exec(&format!("XADD {stream} 1-1 k v"));
    rig.exec(&format!("SET {text} hello"));
    rig.rig.app.update(rig.rig.visual, |this, _| this.navigator.set_settings_defaults(false, 500));
    rig.open_database();

    // Hash: edit a value in the dialog, then add and remove a field.
    rig.open_key(&hash);
    rig.select_row(0);
    rig.click("redis-edit");
    rig.fill("redis-field-0", "light");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("HGET {hash} theme")).as_deref(), Some("light"));
    assert_eq!(rig.grid_cells(), [["theme", "light"]].map(|row| row.map(String::from).to_vec()));
    rig.click("redis-add");
    rig.fill("redis-field-0", "lang");
    rig.fill("redis-field-1", "en");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("HGET {hash} lang")).as_deref(), Some("en"));
    assert_eq!(rig.grid_cells().len(), 2);
    rig.select_row(0);
    let removed = rig.grid_cells()[0][0].clone();
    rig.click("redis-remove");
    assert!(rig.rig.visual.debug_bounds("redis-confirm").is_some(), "removing asks first");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.grid_cells().len(), 1);
    assert_eq!(rig.get(&format!("HEXISTS {hash} {removed}")).as_deref(), Some("0"));

    // List: push at the tail, edit an item, remove one.
    rig.open_key(&list);
    rig.click("redis-add");
    rig.fill("redis-field-1", "c");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.grid_cells().len(), 3);
    rig.select_row(1);
    rig.click("redis-edit");
    rig.fill("redis-field-0", "B!");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("LINDEX {list} 1")).as_deref(), Some("B!"));
    rig.select_row(0);
    rig.click("redis-remove");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("LLEN {list}")).as_deref(), Some("2"));

    // Set and sorted set.
    rig.open_key(&set);
    rig.click("redis-add");
    rig.fill("redis-field-0", "blue");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("SISMEMBER {set} blue")).as_deref(), Some("1"));
    // Remove a member (confirmed): SREM.
    rig.select_row(0);
    rig.click("redis-remove");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("SCARD {set}")).as_deref(), Some("1"));
    rig.open_key(&zset);
    rig.click("redis-add");
    rig.fill("redis-field-0", "bob");
    rig.fill("redis-field-1", "7.5");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("ZSCORE {zset} bob")).as_deref(), Some("7.5"));
    rig.select_row(0);
    rig.click("redis-edit");
    rig.fill("redis-field-0", "99");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("ZSCORE {zset} ann")).as_deref(), Some("99"));
    rig.select_row(0);
    rig.click("redis-remove");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("ZCARD {zset}")).as_deref(), Some("1"));

    // Stream.
    rig.open_key(&stream);
    rig.click("redis-add");
    rig.fill("redis-field-1", "level=warn\nmsg=slow");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("XLEN {stream}")).as_deref(), Some("2"));
    // Remove an entry (confirmed): XDEL.
    rig.select_row(0);
    rig.click("redis-remove");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("XLEN {stream}")).as_deref(), Some("1"));

    // String: the editor saves its text; TTL set and removed.
    rig.open_key(&text);
    let (app, id) = (rig.rig.app.clone(), rig.active_tab().id);
    rig.rig.visual.update(|window, cx| {
        let editor = app.read(cx).redis_ui.string_editors[&id].clone();
        editor.update(cx, |state, cx| state.set_value("changed value", window, cx));
    });
    rig.click("redis-string-save");
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("GET {text}")).as_deref(), Some("changed value"));
    rig.click("redis-key-ttl");
    rig.fill("redis-field-0", "5m");
    rig.submit();
    rig.wait_reloaded();
    let ttl: i64 = rig.get(&format!("TTL {text}")).unwrap().parse().unwrap();
    assert!((290..=300).contains(&ttl), "{ttl}");
    assert!(rig.info().ttl.is_some());
    rig.click("redis-key-ttl");
    rig.fill("redis-field-0", "");
    rig.submit();
    rig.wait_reloaded();
    assert_eq!(rig.get(&format!("TTL {text}")).as_deref(), Some("-1"));
    assert_eq!(rig.info().ttl, None);

    // Rename opens the key under its new name; the old one is gone.
    let renamed = format!("{REDIS_PREFIX}renamed");
    rig.click("redis-key-rename");
    rig.fill("redis-field-0", &renamed);
    rig.submit();
    wait_until(rig.rig.visual, "the renamed key tab", |visual| {
        let renamed = renamed.clone();
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| tab.title == format!("{renamed}@db{REDIS_DB}"))
        })
    });
    rig.wait_tab_ready();
    assert_eq!(rig.get(&format!("EXISTS {text}")).as_deref(), Some("0"));
    assert_eq!(rig.get(&format!("GET {renamed}")).as_deref(), Some("changed value"));

    // Delete asks first, then removes the key from the server and the sidebar.
    rig.click("redis-key-delete");
    assert!(rig.rig.visual.debug_bounds("redis-confirm").is_some(), "deleting asks first");
    rig.click("redis-form-cancel");
    assert_eq!(rig.get(&format!("EXISTS {renamed}")).as_deref(), Some("1"), "cancel keeps the key");
    rig.click("redis-key-delete");
    rig.submit();
    for _ in 0..100 {
        settle(rig.rig.visual);
        if rig.get(&format!("EXISTS {renamed}")).as_deref() == Some("0") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rig.get(&format!("EXISTS {renamed}")).as_deref(), Some("0"));
    assert!(rig.rig.navigator_errors().is_empty(), "{:?}", rig.rig.navigator_errors());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_REDIS_URI (container tablex-test-redis)"]
fn live_redis_console_runs_commands_confirms_unsafe_ones_and_refuses_writes_when_read_only(cx: &mut TestAppContext) {
    let Some(uri) = redis_uri() else { return };
    let mut rig = RedisRig::new(cx, &uri);
    let (key, hash) = (format!("{REDIS_PREFIX}c:k"), format!("{REDIS_PREFIX}c:h"));
    rig.exec(&format!("SET {key} value-1"));
    rig.exec(&format!("HSET {hash} a 1 b 2"));

    // A scalar reply is one row; a map reply is field / value rows.
    rig.console(&format!("GET {key}"));
    rig.rig.wait_status("GET", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 1 }));
    assert_eq!(rig.rig.rows(), 1);
    rig.run_console(&format!("HGETALL {hash}"));
    rig.rig.wait_status("HGETALL", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 2 }));
    let tab = rig.rig.tab;
    let columns = rig.rig.app.read_with(rig.rig.visual, |this, _| {
        let grid = this.grid(tab);
        (0..grid.column_count()).filter_map(|c| grid.column_name(c).map(str::to_owned)).collect::<Vec<_>>()
    });
    assert_eq!(columns, ["field", "value"]);

    // A write runs; the shared history records the runs.
    rig.run_console(&format!("SET {key} value-2"));
    rig.rig.wait_not_running_or_confirm();
    assert_eq!(rig.get(&format!("GET {key}")).as_deref(), Some("value-2"));
    let recorded = rig.rig.app.read_with(rig.rig.visual, |this, _| this.recorded_history.len());
    assert!(recorded >= 3, "console runs are recorded in the history: {recorded}");

    // FLUSHDB goes through the driver's confirmation: Cancel leaves the data.
    rig.run_console("FLUSHDB");
    rig.rig.wait_rendered("sql-confirm-run");
    let reason = rig.rig.visual.debug_bounds("sql-confirm-reason").is_some();
    assert!(reason, "the dialog says why");
    rig.rig.click("sql-confirm-cancel");
    assert!(rig.rig.visual.debug_bounds("sql-confirm-run").is_none());
    assert_eq!(rig.get(&format!("GET {key}")).as_deref(), Some("value-2"), "the data is still there");

    // A blocked command is an error, not a prompt.
    rig.run_console("KEYS *");
    rig.rig.wait_not_running("KEYS");
    assert!(matches!(rig.rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.rig.status());
    assert!(rig.rig.visual.debug_bounds("sql-confirm-run").is_none());

    // A read-only connection refuses every write and keeps reads.
    let app = rig.rig.app.clone();
    publish_types(&app, rig.rig.visual, true);
    rig.rig.give_profile(typed("replica"));
    for command in [format!("SET {key} nope"), format!("DEL {key}"), "FLUSHDB".to_string(), "MYSTERY x".to_string()] {
        rig.run_console(&command);
        settle(rig.rig.visual);
        match rig.rig.status() {
            crate::query::QueryStatus::Failed(message) => {
                assert!(message.contains("read-only"), "{command}: {message}");
            }
            other => panic!("{command} was not refused: {other:?}"),
        }
    }
    assert_eq!(rig.get(&format!("GET {key}")).as_deref(), Some("value-2"));
    rig.run_console(&format!("GET {key}"));
    rig.rig.wait_status("a read on a read-only connection", |status| matches!(status, crate::query::QueryStatus::Ready { rows: 1 }));
    // Edits from a key tab are refused with the same standard message.
    rig.rig.app.update(rig.rig.visual, |this, _| this.navigator.set_settings_defaults(false, 500));
    rig.open_database();
    rig.open_key(&key);
    rig.click("redis-key-delete");
    assert!(rig.rig.visual.debug_bounds("redis-confirm").is_none(), "no prompt for a refused delete");
    assert_eq!(rig.get(&format!("EXISTS {key}")).as_deref(), Some("1"));
}

// ---- the same flows against the fixture server (no container needed) ----------

/// An app in the `redis` scenario: the fixture connection opened, db0 loaded
/// under the `*:*` key filter with `cache`, `queue` and `user` expanded, and the
/// hash tab of `user:1001` open.
fn redis_scenario_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::Redis,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(850.0)));
    settle(visual);
    (app, visual)
}

fn fixture_db_node() -> NavigatorNodeId {
    NavigatorNodeId::Provider { connection: crate::ui_fixture::REDIS_ID.into(), node: "database:0".into() }
}

fn fixture_key_node(name: &str) -> NavigatorNodeId {
    NavigatorNodeId::Provider {
        connection: crate::ui_fixture::REDIS_ID.into(),
        node: format!("key:0:{}", crate::redis::encode_key(name.as_bytes())),
    }
}

fn redis_sidebar_labels(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Vec<String> {
    app.read_with(visual, |this, _| this.navigator.visible_rows().into_iter().map(|row| row.label).collect())
}

fn fixture_cells(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> Vec<Vec<String>> {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        let grid = this.grid(tab);
        (0..grid.row_count())
            .map(|row| {
                (0..grid.column_count())
                    .map(|column| grid.cell_text(CellPosition { row, column }).map(|t| t.to_string()).unwrap_or_default())
                    .collect()
            })
            .collect()
    })
}

fn fixture_redis<T>(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext, read: impl FnOnce(&crate::ui_fixture_redis::RedisFixture) -> T) -> T {
    app.read_with(visual, |this, _| read(this.scenario_ui.service.as_ref().unwrap().fixture().redis().unwrap()))
}

fn click_button(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    click_debug_button(visual, selector);
    settle(visual);
}

fn fill_field(visual: &mut gpui::VisualTestContext, selector: &'static str, text: &str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    if text.is_empty() {
        visual.simulate_keystrokes("backspace");
    } else {
        visual.simulate_input(text);
    }
    settle(visual);
}

#[gpui::test]
fn the_redis_connection_shows_databases_with_key_counts_and_a_namespace_tree_under_the_filter(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    let labels = redis_sidebar_labels(&app, visual);
    assert!(labels.iter().any(|l| l == "Redis fixture"), "{labels:?}");
    // The key count of the database, with the active filter named on its row.
    assert!(
        labels.iter().any(|l| l.starts_with("db0 (") && l.ends_with("keys) \u{00B7} *:*")),
        "{labels:?}"
    );
    // The namespaces are folders with their loaded counts; plain keys are not listed under `*:*`.
    for folder in ["cache (13)", "queue (2)", "user (24)", "session (10)"] {
        assert!(labels.contains(&folder.to_string()), "{folder} in {labels:?}");
    }
    assert!(!labels.contains(&"maintenance".to_string()), "the filter dropped the plain key");
    // An expanded folder shows its keys: the user hashes.
    assert!(labels.contains(&"1001".to_string()), "{labels:?}");
    // The key rows carry their type (no per-row entity: plain rows of the list).
    let (key_rows, with_type) = app.read_with(visual, |this, _| {
        let id = fixture_key_node("user:1001");
        (this.navigator.provider_node(&id).is_some(), this.navigator.provider_node(&id).and_then(|n| n.subtitle.clone()))
    });
    assert!(key_rows);
    assert_eq!(with_type.as_deref(), Some("hash"));
}

#[gpui::test]
fn load_more_pages_the_scan_and_the_filter_dialog_narrows_the_list(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    // Twenty keys per page: reload db0 without the scenario's filter.
    app.update(visual, |this, cx| {
        this.navigator.set_settings_defaults(false, 20);
        this.apply_redis_filter(&fixture_db_node(), crate::redis::KeyFilter { group: false, ..Default::default() }, cx);
    });
    settle(visual);
    let labels = redis_sidebar_labels(&app, visual);
    assert!(labels.iter().any(|l| l == "Load More"), "{labels:?}");
    let flat_keys = labels.iter().filter(|l| l.contains(':') || *l == "maintenance").count();
    assert_eq!(flat_keys, 20, "one page of 20 flat key names: {labels:?}");
    let load_more = NavigatorNodeId::Provider {
        connection: crate::ui_fixture::REDIS_ID.into(),
        node: "__tablex_load_more__:database:0".into(),
    };
    // The row is below the fold: scroll the sidebar to it, then click it.
    app.update(visual, |this, cx| {
        let index = this.navigator.visible_index(&load_more).expect("a Load More row");
        this.sidebar.scroll.scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        cx.notify();
    });
    settle(visual);
    click_node_part(&app, visual, &load_more, "sidebar-row");
    let after = redis_sidebar_labels(&app, visual);
    assert_eq!(after.iter().filter(|l| l.contains(':') || *l == "maintenance").count(), 40);

    // The Filter Keys dialog: a glob narrows the server's answer.
    app.update(visual, |this, cx| {
        let window = this.driver_runtime.window.expect("rendered");
        let root = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| this.open_redis_filter_dialog(fixture_db_node(), window, cx));
            });
        });
    });
    settle(visual);
    fill_field(visual, "redis-field-0", "leaderboard:*");
    click_button(visual, "redis-form-ok");
    settle(visual);
    let narrowed = redis_sidebar_labels(&app, visual);
    assert!(narrowed.contains(&"leaderboard:weekly".to_string()), "{narrowed:?}");
    assert!(!narrowed.iter().any(|l| l.starts_with("user:")), "{narrowed:?}");
    assert!(!narrowed.iter().any(|l| l == "Load More"));
    assert!(narrowed.iter().any(|l| l.ends_with("leaderboard:*")), "the row names the filter: {narrowed:?}");
    let filter = app.read_with(visual, |this, _| this.navigator.redis_filter(&fixture_db_node()));
    assert_eq!((filter.pattern.as_str(), filter.group), ("leaderboard:*", false));
}

#[gpui::test]
fn clicking_a_key_opens_a_typed_value_tab_for_every_type(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    // The scenario opened user:1001; its header and fields are on screen.
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().cloned().unwrap());
    assert_eq!(tab.title, "user:1001@db0");
    assert!(tab.query_mode.is_redis_key());
    for selector in ["redis-key-toolbar", "redis-key-type", "redis-key-ttl", "redis-key-facts", "redis-key-rename", "redis-key-delete", "redis-add", "redis-edit", "redis-remove"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is rendered");
    }
    assert_eq!(fixture_cells(&app, visual).len(), 5, "hash: one row per field");
    assert_eq!(fixture_cells(&app, visual)[0][0], "name");

    // A click on another key row opens its own tab (the real sidebar row).
    app.update(visual, |this, cx| {
        this.navigator.set_settings_defaults(false, 500);
        this.apply_redis_filter(&fixture_db_node(), crate::redis::KeyFilter::default(), cx);
    });
    settle(visual);
    for (name, columns, rows) in [
        ("queue:billing", vec!["index", "value"], 3),
        ("tags:featured", vec!["member"], 6),
        ("leaderboard:weekly", vec!["member", "score"], 15),
        ("events:orders", vec!["id", "fields"], 8),
    ] {
        // Expand the folders on the way, then click the leaf row.
        let mut path = String::new();
        let parts: Vec<&str> = name.split(':').collect();
        for part in &parts[..parts.len() - 1] {
            path = if path.is_empty() { (*part).to_owned() } else { format!("{path}:{part}") };
            let folder = NavigatorNodeId::Provider { connection: crate::ui_fixture::REDIS_ID.into(), node: format!("ns:0:{path}") };
            if app.read_with(visual, |this, _| this.navigator.visible_index(&folder).is_some_and(|i| !this.navigator.visible_row_at(i).unwrap().expanded)) {
                click_node_part(&app, visual, &folder, "nav-disclosure");
            }
        }
        click_node_part(&app, visual, &fixture_key_node(name), "sidebar-row");
        let tab = app.read_with(visual, |this, _| this.workspace.active_tab().cloned().unwrap());
        assert_eq!(tab.title, format!("{name}@db0"));
        let names = app.read_with(visual, |this, _| {
            let grid = this.grid(tab.id);
            (0..grid.column_count()).filter_map(|c| grid.column_name(c).map(str::to_owned)).collect::<Vec<_>>()
        });
        assert_eq!(names, columns, "{name}");
        assert_eq!(fixture_cells(&app, visual).len(), rows, "{name}");
    }
    // Opening the same key again reuses its tab.
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    click_node_part(&app, visual, &fixture_key_node("events:orders"), "sidebar-row");
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), before);
}

#[gpui::test]
fn a_json_string_key_opens_pretty_printed_with_text_and_hex_views(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| {
        this.navigator.set_settings_defaults(false, 500);
        this.apply_redis_filter(&fixture_db_node(), crate::redis::KeyFilter::default(), cx);
        let tab = this.open_redis_key_tab(crate::ui_fixture::REDIS_ID, "0", &crate::redis::encode_key(b"config:app"), "config:app", cx);
        let _ = tab;
    });
    settle(visual);
    assert!(visual.debug_bounds("redis-string-view").is_some());
    let (tab, view, text) = app.read_with(visual, |this, cx| {
        let id = this.workspace.active_tab().unwrap().id;
        (id, this.workspace.runtime(id).unwrap().redis.view, this.redis_ui.string_editors[&id].read(cx).value().to_string())
    });
    assert_eq!(view, crate::redis::StringView::Json);
    assert!(text.contains("\n  \"features\": {"), "{text}");
    click_button(visual, "redis-view-hex");
    let hex = app.read_with(visual, |this, cx| this.redis_ui.string_editors[&tab].read(cx).value().to_string());
    assert!(hex.starts_with("00000000  7b 22 6e 61 6d 65"), "{hex}");
    click_button(visual, "redis-view-text");
    let plain = app.read_with(visual, |this, cx| this.redis_ui.string_editors[&tab].read(cx).value().to_string());
    assert!(plain.starts_with("{\"name\":\"storefront\""), "{plain}");
    // Saving writes the editor's text through the string mutation.
    visual.update(|window, cx| {
        let editor = app.read(cx).redis_ui.string_editors[&tab].clone();
        editor.update(cx, |state, cx| state.set_value("{\"name\":\"saved\"}", window, cx));
    });
    click_button(visual, "redis-string-save");
    settle(visual);
    let stored = fixture_redis(&app, visual, |redis| redis.key(0, "config:app").map(|key| key.value.clone()));
    assert_eq!(stored, Some(crate::ui_fixture_redis::Value::Text("{\"name\":\"saved\"}".into())));
    assert!(app.read_with(visual, |this, cx| this.redis_ui.string_editors[&tab].read(cx).value().to_string()).contains("saved"));
}

#[gpui::test]
fn editing_through_the_dialogs_changes_the_server_and_the_view(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    // Hash: edit the first field's value from the footer.
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.select_grid_cell(tab, CellPosition { row: 0, column: 0 });
        cx.notify();
    });
    settle(visual);
    click_button(visual, "redis-edit");
    fill_field(visual, "redis-field-0", "Ada Lovelace");
    click_button(visual, "redis-form-ok");
    settle(visual);
    let value = fixture_redis(&app, visual, |redis| match &redis.key(0, "user:1001").unwrap().value {
        crate::ui_fixture_redis::Value::Hash(items) => items[0].clone(),
        other => panic!("{other:?}"),
    });
    assert_eq!(value, ("name".to_string(), "Ada Lovelace".to_string()));
    assert_eq!(fixture_cells(&app, visual)[0], ["name", "Ada Lovelace"]);

    // Add a field with the footer's Add.
    click_button(visual, "redis-add");
    fill_field(visual, "redis-field-0", "tier");
    fill_field(visual, "redis-field-1", "gold");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert_eq!(fixture_cells(&app, visual).len(), 6);
    // An empty field name is refused and keeps the dialog open.
    click_button(visual, "redis-add");
    click_button(visual, "redis-form-ok");
    assert!(visual.debug_bounds("redis-form-ok").is_some(), "the dialog stays open");
    click_button(visual, "redis-form-cancel");
    assert_eq!(fixture_cells(&app, visual).len(), 6);

    // TTL: set, then remove, from the header.
    click_button(visual, "redis-key-ttl");
    fill_field(visual, "redis-field-0", "2h");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert_eq!(fixture_redis(&app, visual, |redis| redis.key(0, "user:1001").unwrap().ttl), Some(7200));
    click_button(visual, "redis-key-ttl");
    fill_field(visual, "redis-field-0", "");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert_eq!(fixture_redis(&app, visual, |redis| redis.key(0, "user:1001").unwrap().ttl), None);
    // A TTL of zero would delete the key: refused in the dialog.
    click_button(visual, "redis-key-ttl");
    fill_field(visual, "redis-field-0", "0");
    click_button(visual, "redis-form-ok");
    assert!(visual.debug_bounds("redis-form-ok").is_some(), "0 is refused");
    click_button(visual, "redis-form-cancel");

    // Rename: opens the key under the new name.
    click_button(visual, "redis-key-rename");
    fill_field(visual, "redis-field-0", "user:ada");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert!(fixture_redis(&app, visual, |redis| redis.key(0, "user:1001").is_none() && redis.key(0, "user:ada").is_some()));
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().title.clone()), "user:ada@db0");
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().iter().filter(|t| t.title == "user:1001@db0").count());
    assert_eq!(tabs, 0, "the old tab is closed");

    // Delete asks first; Cancel keeps the key, OK removes it and its row.
    click_button(visual, "redis-key-delete");
    assert!(visual.debug_bounds("redis-confirm").is_some());
    click_button(visual, "redis-form-cancel");
    assert!(fixture_redis(&app, visual, |redis| redis.key(0, "user:ada").is_some()));
    click_button(visual, "redis-key-delete");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert!(fixture_redis(&app, visual, |redis| redis.key(0, "user:ada").is_none()));
    assert!(app.read_with(visual, |this, _| !this.workspace.tabs().iter().any(|t| t.title == "user:ada@db0")));
    assert!(!redis_sidebar_labels(&app, visual).contains(&"ada".to_string()));
    // The rename and the delete closed their tabs on their own: Reopen Closed Tab has nothing to bring back.
    ro_nothing_reopens(&app, visual);
}

#[gpui::test]
fn the_console_completes_runs_shows_replies_confirms_unsafe_commands_and_honours_read_only(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    // New Query on the database row opens the console on that database.
    app.update(visual, |this, cx| assert!(this.new_query_for_sidebar_node(&fixture_db_node(), cx)));
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().cloned().unwrap());
    assert_eq!((tab.title.as_str(), tab.database.as_str()), ("console@db0", "0"));
    assert!(tab.query_mode.is_redis_command());

    // Command names, then key names from the loaded keys.
    type_in_editor(visual, "hg");
    let editor = app.read_with(visual, |this, _| this.query_editors.by_tab[&tab.id].clone());
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open && labels == ["HGET", "HGETALL", "HGETDEL"][..labels.len()].to_vec() || labels.iter().any(|l| l == "HGETALL"), "{labels:?}");
    assert!(details.iter().flatten().any(|d| d == "key field"), "the syntax is the detail: {details:?}");
    type_in_editor(visual, "hgetal");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(editor_value(&editor, visual), "HGETALL");
    visual.simulate_input(" user:100");
    settle(visual);
    let (open, labels, details) = menu_state(&editor, visual);
    assert!(open && labels.contains(&"user:1001".to_string()), "{labels:?}");
    assert_eq!(details[0].as_deref(), Some("key"));
    visual.simulate_keystrokes("escape");

    // A map reply is a field / value table.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab.id).unwrap().sql = "HGETALL user:1002".into();
        this.run_tab_query(tab.id, cx);
    });
    settle(visual);
    assert_eq!(fixture_cells(&app, visual)[1], ["email", "user1002@example.com"]);
    // A scalar reply is one row.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab.id).unwrap().sql = "GET counter:visits".into();
        this.run_tab_query(tab.id, cx);
    });
    settle(visual);
    assert_eq!(fixture_cells(&app, visual), [["48213"]].map(|r| r.map(String::from).to_vec()));
    let history = app.read_with(visual, |this, _| this.recorded_history.iter().map(|h| h.sql.clone()).collect::<Vec<_>>());
    assert_eq!(history, ["HGETALL user:1002", "GET counter:visits"]);

    // FLUSHDB: the driver asks for confirmation; Cancel does not send it.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab.id).unwrap().sql = "FLUSHDB".into();
        this.run_tab_query(tab.id, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("sql-confirm-run").is_some(), "the confirm dialog opens");
    assert!(visual.debug_bounds("sql-confirm-reason").is_some());
    click_button(visual, "sql-confirm-cancel");
    assert!(fixture_redis(&app, visual, |redis| redis.confirmed_commands.is_empty()));
    app.update(visual, |this, cx| this.run_tab_query(tab.id, cx));
    settle(visual);
    click_button(visual, "sql-confirm-run");
    settle(visual);
    assert_eq!(fixture_redis(&app, visual, |redis| redis.confirmed_commands.clone()), ["FLUSHDB"]);

    // A blocked command is an error, never a prompt.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab.id).unwrap().sql = "KEYS *".into();
        this.run_tab_query(tab.id, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("sql-confirm-run").is_none());
    let status = app.read_with(visual, |this, _| this.workspace.runtime(tab.id).unwrap().query.status().clone());
    assert!(matches!(status, crate::query::QueryStatus::Failed(message) if message.contains("blocked")));

    // Read-only: writes (and unknown commands) are refused, reads still run.
    publish_types(&app, visual, true);
    give_profile(&app, visual, crate::ui_fixture::REDIS_ID, typed("replica"));
    for command in ["SET a b", "DEL a", "FLUSHDB", "MYSTERY x"] {
        app.update(visual, |this, cx| {
            this.workspace.tab_mut(tab.id).unwrap().sql = command.into();
            this.run_tab_query(tab.id, cx);
        });
        settle(visual);
        let status = app.read_with(visual, |this, _| this.workspace.runtime(tab.id).unwrap().query.status().clone());
        assert!(matches!(&status, crate::query::QueryStatus::Failed(message) if message.contains("read-only")), "{command}: {status:?}");
    }
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab.id).unwrap().sql = "GET counter:visits".into();
        this.run_tab_query(tab.id, cx);
    });
    settle(visual);
    assert_eq!(fixture_cells(&app, visual).len(), 1);
    // And so are element edits from a key tab (the standard message, no prompt).
    app.update(visual, |this, cx| {
        this.open_redis_key_tab(crate::ui_fixture::REDIS_ID, "0", &crate::redis::encode_key(b"maintenance"), "maintenance", cx);
    });
    settle(visual);
    click_button(visual, "redis-key-delete");
    assert!(visual.debug_bounds("redis-confirm").is_none());
    assert!(fixture_redis(&app, visual, |redis| redis.key(0, "maintenance").is_some()));
}

#[gpui::test]
fn the_ttl_counts_down_on_a_coarse_timer_and_only_repaints_when_the_label_changes(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| {
        let tab = this.open_redis_key_tab(crate::ui_fixture::REDIS_ID, "0", &crate::redis::encode_key(b"cache:page:home"), "cache:page:home", cx);
        // The info was read 100 s ago: 300 - 100 left.
        let runtime = this.workspace.runtime_mut(tab).unwrap();
        runtime.redis.info_read_at = Some(Instant::now() - Duration::from_secs(100));
        cx.notify();
    });
    settle(visual);
    let left = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.workspace.runtime(tab).unwrap().redis.ttl_left(Instant::now()).unwrap()
    });
    assert!((199..=200).contains(&left), "{left}");
    // One tick repaints (the label changed), a second tick in the same second does not.
    let (first, second) = app.update(visual, |this, cx| {
        let first = this.tick_redis_ttl(cx);
        let tab = this.workspace.active_tab().unwrap().id;
        let shown = this.redis_ui.ttl_labels.get(&tab).cloned();
        let again = this.tick_redis_ttl(cx);
        (first, (again, shown, this.redis_ui.ttl_labels.get(&tab).cloned()))
    });
    assert!(first, "a counting key keeps the timer alive");
    assert!(second.0 && second.1 == second.2, "{second:?}");
    assert!(second.1.as_deref().is_some_and(|label| label.ends_with('s') && label.contains('m')), "{second:?}");
    // A key without an expiry stops the timer.
    app.update(visual, |this, cx| {
        this.open_redis_key_tab(crate::ui_fixture::REDIS_ID, "0", &crate::redis::encode_key(b"counter:visits"), "counter:visits", cx);
    });
    settle(visual);
    assert!(!app.update(visual, |this, cx| this.tick_redis_ttl(cx)), "nothing counts down: the timer stops");
}

// ---- the key list at 100k keys ---------------------------------------------------

fn percentile(sorted: &[std::time::Duration], p: f64) -> std::time::Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Scrolls the sidebar through `rows` rows in `steps` jumps and returns the
/// time of each frame (refresh + paint) and its allocation count.
fn scroll_frames(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    rows: usize,
    steps: usize,
) -> (Vec<std::time::Duration>, Vec<u64>) {
    let (mut times, mut allocs) = (Vec::new(), Vec::new());
    for step in 0..steps {
        app.update(visual, |this, cx| {
            this.sidebar.scroll.scroll_to_item(step * rows / steps, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        let before = crate::grid_bench::allocation_count();
        let start = std::time::Instant::now();
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        times.push(start.elapsed());
        allocs.push(crate::grid_bench::allocation_count() - before);
    }
    (times, allocs)
}

/// Measures the Redis key list at 100k keys: what the navigator does with each
/// SCAN page (the server's answer is generated beforehand and not counted) and
/// the frames of scrolling the sidebar. Run on demand:
///
///   cargo test --release -p tablex-gpui redis_key_list_at_100k -- --ignored --nocapture --test-threads=1
#[gpui::test]
#[ignore = "on-demand measurement; run with --release --ignored --nocapture"]
fn redis_key_list_at_100k_keys_loads_pages_and_scrolls(cx: &mut TestAppContext) {
    const KEYS: usize = 100_000;
    const PAGE: u64 = 500;
    let (app, visual) = redis_scenario_app(cx);
    let server = crate::ui_fixture_redis::RedisFixture::generated(KEYS);
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = server.navigator_page(Some("database:0"), None, cursor.as_deref(), PAGE).unwrap();
        cursor = page.next_cursor.clone();
        pages.push(page);
        if cursor.is_none() {
            break;
        }
    }
    app.update(visual, |this, _| this.navigator.set_settings_defaults(false, PAGE));
    let db = fixture_db_node();
    for (label, group) in [("flat list", false), ("grouped by ':'", true)] {
        let bytes_before = crate::grid_bench::allocation_bytes();
        let (times, bytes) = app.update(visual, |this, _| {
            this.navigator.set_redis_filter(&db, crate::redis::KeyFilter { group, ..Default::default() });
            this.navigator.reset_node(&db);
            let mut first = this.navigator.toggle_expansion(&db).request;
            let mut times = Vec::new();
            let mut bytes = Vec::new();
            for page in pages.iter() {
                // What a click on Load More costs: the request, then the page.
                let page = page.clone();
                let before = crate::grid_bench::allocation_bytes();
                let start = std::time::Instant::now();
                let request = first.take().or_else(|| this.navigator.load_more(&db)).expect("a request");
                let generation = this.navigator.load_generation(&request).unwrap();
                this.navigator.apply_provider_page(&request, generation, Ok(page));
                times.push(start.elapsed());
                bytes.push(crate::grid_bench::allocation_bytes() - before);
            }
            (times, bytes)
        });
        let rows = app.read_with(visual, |this, _| this.navigator.visible_row_count());
        let loaded = app.read_with(visual, |this, _| {
            this.navigator.redis_loaded_keys(crate::ui_fixture::REDIS_ID, "0").count()
        });
        let retained = crate::grid_bench::allocation_bytes().saturating_sub(bytes_before);
        let slowest = times.iter().enumerate().max_by_key(|(_, time)| **time).map(|(index, _)| index);
        eprintln!("slowest page: {slowest:?}");
        let mut sorted = times.clone();
        sorted.sort();
        let mut sorted_bytes = bytes.clone();
        sorted_bytes.sort();
        eprintln!("--- {label}: {KEYS} keys, {PAGE} per SCAN page, {} pages (loaded {loaded}, visible rows {rows})", times.len());
        eprintln!(
            "Load More click (request + page): p50 {:?} p95 {:?} max {:?}; allocated per page p50 {} KiB max {} KiB; whole load {} MiB",
            percentile(&sorted, 0.5),
            percentile(&sorted, 0.95),
            sorted.last().unwrap(),
            sorted_bytes[sorted_bytes.len() / 2] / 1024,
            sorted_bytes.last().unwrap() / 1024,
            retained / (1 << 20),
        );
        settle(visual);
        let rows = app.read_with(visual, |this, _| this.navigator.visible_row_count());
        for _ in 0..3 {
            let _ = scroll_frames(&app, visual, rows, 8);
        }
        let (mut frames, allocs) = scroll_frames(&app, visual, rows, 120);
        frames.sort();
        let mean_allocs = allocs.iter().sum::<u64>() / allocs.len() as u64;
        eprintln!(
            "scroll over {rows} rows, 120 jumps: frame p50 {:?} p95 {:?} max {:?}; allocations per frame {} (max {})",
            percentile(&frames, 0.5),
            percentile(&frames, 0.95),
            frames.last().unwrap(),
            mean_allocs,
            allocs.iter().max().unwrap(),
        );
    }
}

#[gpui::test]
fn new_key_creates_the_key_opens_it_and_lists_it_in_the_sidebar(cx: &mut TestAppContext) {
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| {
        this.navigator.set_settings_defaults(false, 500);
        this.apply_redis_filter(&fixture_db_node(), crate::redis::KeyFilter { group: false, ..Default::default() }, cx);
        let window = this.driver_runtime.window.expect("rendered");
        let root = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = root.update(cx, |this, cx| {
                    this.open_redis_new_key_dialog(crate::ui_fixture::REDIS_ID, "0", window, cx)
                });
            });
        });
    });
    settle(visual);
    // An empty name is refused.
    click_button(visual, "redis-form-ok");
    assert!(visual.debug_bounds("redis-form-ok").is_some(), "the dialog stays open without a key name");
    fill_field(visual, "redis-field-0", "fresh:key");
    fill_field(visual, "redis-field-3", "hello world");
    fill_field(visual, "redis-field-4", "1h");
    click_button(visual, "redis-form-ok");
    settle(visual);
    let created = fixture_redis(&app, visual, |redis| redis.key(0, "fresh:key").cloned()).expect("the key was created");
    assert_eq!(created.value, crate::ui_fixture_redis::Value::Text("hello world".into()));
    assert_eq!(created.ttl, Some(3600), "the expiry is applied after the value");
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().title.clone()), "fresh:key@db0");
    assert!(redis_sidebar_labels(&app, visual).contains(&"fresh:key".to_string()), "the sidebar lists it");
}

#[gpui::test]
fn the_type_filter_and_a_namespace_menu_narrow_the_key_list(cx: &mut TestAppContext) {
    use crate::redis::{KeyFilter, KeyType};
    let (app, visual) = redis_scenario_app(cx);
    app.update(visual, |this, cx| {
        this.navigator.set_settings_defaults(false, 500);
        this.apply_redis_filter(
            &fixture_db_node(),
            KeyFilter { key_type: Some(KeyType::Set), group: false, ..Default::default() },
            cx,
        );
    });
    settle(visual);
    let labels = redis_sidebar_labels(&app, visual);
    assert!(labels.contains(&"tags:featured".to_string()), "{labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with("user:")), "{labels:?}");
    assert!(labels.iter().any(|l| l.ends_with("set")), "the row names the type filter: {labels:?}");
    // "Show Only This Namespace" filters by the escaped subtree pattern.
    app.update(visual, |this, cx| {
        this.apply_redis_filter(&fixture_db_node(), KeyFilter::default(), cx);
        let node = fixture_db_node();
        let mut filter = this.navigator.redis_filter(&node);
        filter.pattern = crate::redis::subtree_pattern("cache:product");
        this.apply_redis_filter(&node, filter, cx);
    });
    settle(visual);
    let labels = redis_sidebar_labels(&app, visual);
    assert!(labels.iter().any(|l| l == "cache (12)"), "{labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with("user")), "{labels:?}");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_REDIS_URI (container tablex-test-redis)"]
fn live_redis_creates_a_key_of_every_type_with_an_expiry(cx: &mut TestAppContext) {
    let Some(uri) = redis_uri() else { return };
    let rig = RedisRig::new(cx, &uri);
    let key = rig.rig.key.clone();
    for (index, (kind, name, value)) in [
        ("string", "", "hello"),
        ("hash", "field", "value"),
        ("list", "", "first"),
        ("set", "member", ""),
        ("zset", "player", "42.5"),
        ("stream", "", "level=info\nmsg=boot"),
    ]
    .into_iter()
    .enumerate()
    {
        let name_of_key = format!("{REDIS_PREFIX}new:{kind}");
        let values = [name_of_key.as_str(), kind, name, value, "10m"].map(String::from).to_vec();
        let (shown, edit, ttl) = super::redis::new_key_edit(&values).expect("valid dialog values");
        assert_eq!(shown, name_of_key);
        let (key_raw, db) = (crate::redis::encode_key(shown.as_bytes()), REDIS_DB.to_string());
        rig.rig.app.update(rig.rig.visual, |this, cx| {
            this.run_redis_edit(&key, &db, &key_raw, edit, Box::new(|_, affected, _| assert_eq!(affected, 1)), cx);
        });
        let expected = shown.clone();
        for _ in 0..200 {
            settle(rig.rig.visual);
            if rig.get(&format!("TYPE {expected}")).as_deref() == Some(kind) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(rig.get(&format!("TYPE {expected}")).as_deref(), Some(kind), "{kind} #{index}");
        if let Some(crate::redis::Ttl::Seconds(seconds)) = ttl {
            rig.exec(&format!("EXPIRE {expected} {seconds}"));
            let left: i64 = rig.get(&format!("TTL {expected}")).unwrap().parse().unwrap();
            assert!((590..=600).contains(&left), "{left}");
        }
    }
}

#[gpui::test]
fn a_production_redis_connection_asks_before_a_change_and_before_running_a_command(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual) = redis_scenario_app(cx);
    give_profile(&app, visual, crate::ui_fixture::REDIS_ID, typed(PRODUCTION));
    // A change that removes nothing asks once, naming the connection type.
    click_button(visual, "redis-key-ttl");
    fill_field(visual, "redis-field-0", "1h");
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert!(visual.debug_bounds("redis-data-change").is_some(), "Production asks before a data change");
    click_button(visual, "redis-form-cancel");
    assert_eq!(fixture_redis(&app, visual, |redis| redis.key(0, "user:1001").unwrap().ttl), None, "Cancel changes nothing");
    click_button(visual, "redis-key-ttl");
    fill_field(visual, "redis-field-0", "1h");
    click_button(visual, "redis-form-ok");
    settle(visual);
    click_button(visual, "redis-form-ok");
    settle(visual);
    assert_eq!(fixture_redis(&app, visual, |redis| redis.key(0, "user:1001").unwrap().ttl), Some(3600));

    // The console asks before running anything, and runs it once confirmed.
    app.update(visual, |this, cx| assert!(this.new_query_for_sidebar_node(&fixture_db_node(), cx)));
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "GET maintenance".into();
        this.run_tab_query(tab, cx);
    });
    settle(visual);
    assert!(visual.debug_bounds("execute-confirm-run").is_some(), "Production asks before a command");
    assert!(visual.debug_bounds("execute-confirm-statement").is_some());
    click_button(visual, "execute-confirm-run");
    settle(visual);
    assert_eq!(fixture_cells(&app, visual), [["off"]].map(|row| row.map(String::from).to_vec()));
}
