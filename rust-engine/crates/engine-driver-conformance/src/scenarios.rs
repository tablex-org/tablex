//! The scenarios. Each one checks a group of rules of `engine_driver_api::sql`
//! ("Conformance rules") and cleans up after itself.

use crate::ensure;
use crate::harness::{expect_ok, expect_rejection, Check, Conn, Ctx, Failure, Problems};
use engine_driver_api::sql::{codes, ConstraintKind, SqlColumnNode, SqlObjectRef, TableProperties};
use engine_driver_api::{Capability, NavigatorNode, NavigatorPage};
use engine_driver_host::LoaderError;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

pub type ScenarioFn = fn(&Ctx) -> Check;

/// `(name, rule it checks, implementation)`.
pub const SCENARIOS: &[(&str, &str, ScenarioFn)] = &[
    ("capabilities", "capability tokens, sql_dialect, navigator kinds", capabilities),
    ("query_page_shape", "page columns, row_count, Arrow lease", query_page_shape),
    ("query_paging", "default/capped page size, exact truncated lookahead, total_count None unless exact", query_paging),
    ("query_not_pageable", "only a single pure read is pageable; offset>0 on anything else is refused and never re-runs the write", query_not_pageable),
    ("query_nonpageable_truncation", "a non-pageable row-returning statement reports truncated=false, drops rows beyond limit and is never re-run for offset>0", query_nonpageable_truncation),
    ("query_nested_with_write", "a nested data-modifying WITH needs confirmation and is never executed twice", query_nested_with_write),
    ("query_script_last_result_set", "a script returns the rows of its LAST result set", query_script_last_result_set),
    ("query_confirmation", "dangerous statements need payload.confirmed", query_confirmation),
    ("query_cancelled", "a requested cancel fails with query.cancelled", query_cancelled),
    ("query_interrupted", "an unrequested interrupt fails with retryable query.interrupted", query_interrupted),
    ("navigator_payloads", "table nodes carry SqlObjectRef, column nodes SqlColumnNode, describe action", navigator_payloads),
    ("navigator_limit", "limit 0 = default, small limits page with cursors, huge limits are capped", navigator_limit),
    ("navigator_filter", "payload.filter is a case-insensitive literal substring", navigator_filter),
    ("navigator_default_and_cap", "limit 0 = 500 nodes, limit above 5000 is capped at 5000", navigator_default_and_cap),
    ("metadata_not_blocked", "navigator/describe do not wait behind a running user query", metadata_not_blocked),
    ("mutation_crud", "insert/update/delete with composite keys, zero-row changes fail", mutation_crud),
    ("mutation_null_key", "a null key value matches SQL NULL", mutation_null_key),
    ("mutation_identity_not_unique", "a key matching several rows fails and rolls back", mutation_identity_not_unique),
    ("mutation_zero_row_insert", "an insert affecting zero rows is mutation.no_rows_affected", mutation_zero_row_insert),
    ("mutation_unsupported_kind", "unknown kind is mutation.unsupported", mutation_unsupported_kind),
    ("mutation_batch", "batch is all-or-nothing", mutation_batch),
    ("mutation_values_are_bound", "values are parameters; arrays/objects become JSON text", mutation_values_are_bound),
    ("mutation_in_user_transaction", "a mutation joins the user's transaction via savepoint and never commits/rolls it back", mutation_in_user_transaction),
    ("mutation_cancelled", "a requested cancel of a slow mutation fails with mutation.cancelled", mutation_cancelled),
    ("create_database_bad_payload", "create_database with a payload is mutation.bad_request", create_database_bad_payload),
    ("create_database", "create_database: affected_rows 0, refusal is mutation.create_database.rejected", create_database),
    ("describe_object", "describe returns TableProperties; unknown object is describe.not_found", describe_object),
];

const SLOW_SECONDS: u32 = 20;

fn rows_text(rows: &[Vec<String>]) -> String {
    format!("{rows:?}")
}

// --------------------------------------------------------------- capabilities

fn capabilities(ctx: &Ctx) -> Check {
    let capabilities = ctx.handle.capabilities();
    let fixture = ctx.fixture;
    ensure!(
        capabilities.driver == fixture.driver_id(),
        "driver id is {:?}, expected {:?}",
        capabilities.driver,
        fixture.driver_id()
    );
    ensure!(
        capabilities.editors.iter().any(|editor| editor == "sql"),
        "editors must include \"sql\": {:?}",
        capabilities.editors
    );
    ensure!(!capabilities.uri_schemes.is_empty(), "no uri_schemes");
    let mut required = vec![
        Capability::Navigator,
        Capability::QuerySql,
        Capability::QueryArrowCData,
        Capability::OperationCancel,
        Capability::MutationInsert,
        Capability::MutationUpdate,
        Capability::MutationDelete,
        Capability::MutationBatch,
        Capability::MetadataTableProperties,
    ];
    if fixture.supports_create_database() {
        required.push(Capability::DatabaseCreate);
    } else {
        ensure!(
            !capabilities.has(Capability::DatabaseCreate),
            "{} has no CREATE DATABASE: database.create must not be advertised",
            fixture.label()
        );
    }
    let missing: Vec<_> = required
        .into_iter()
        .filter(|capability| !capabilities.has(*capability))
        .map(|capability| capability.as_str())
        .collect();
    ensure!(missing.is_empty(), "missing capability {missing:?}");
    ensure!(
        !capabilities.has(Capability::QueryPageLease),
        "kit drivers serve Arrow pages, not byte page leases"
    );
    let Some(dialect) = capabilities.sql_dialect.as_ref() else {
        return Err("capabilities carry no sql_dialect".into());
    };
    ensure!(
        dialect.identifier_quote == fixture.identifier_quote(),
        "identifier_quote {:?}, expected {:?}",
        dialect.identifier_quote,
        fixture.identifier_quote()
    );
    ensure!(
        dialect.string_literal_quote == "'",
        "string_literal_quote {:?}",
        dialect.string_literal_quote
    );
    ensure!(
        dialect.supports_schemas == fixture.supports_schemas(),
        "supports_schemas is {}, expected {}",
        dialect.supports_schemas,
        fixture.supports_schemas()
    );
    for kind in ["database", "table", "view", "column"] {
        ensure!(
            capabilities.navigator_kinds.iter().any(|k| k == kind),
            "navigator_kinds lacks {kind:?}: {:?}",
            capabilities.navigator_kinds
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------- query

fn query_page_shape(ctx: &Ctx) -> Check {
    let mut conn = ctx.connect()?;
    let page = expect_ok(
        conn.open_page("SELECT 1 AS a, 'x' AS b", 0, 10, false)
            .map_err(|e| e.to_string()),
        "select",
    )?;
    ensure!(page.row_count == 1, "row_count {}", page.row_count);
    let names: Vec<_> = page.columns.iter().map(|c| c.name.to_lowercase()).collect();
    ensure!(names == ["a", "b"], "column names {names:?}");
    ensure!(!page.flags.truncated, "one row of ten is not truncated");
    let lease = expect_ok(
        conn.session
            .acquire_arrow_page(page.page_id, 0, page.row_count),
        "acquire_arrow_page",
    )?;
    // SAFETY: the lease was just returned by the driver's Arrow export.
    let imported = unsafe { lease.into_record_batch() }
        .map_err(|error| Failure::Fail(format!("Arrow import failed: {error}")))?;
    ensure!(
        imported.batch.num_rows() == 1 && imported.batch.num_columns() == 2,
        "imported batch is {}x{}",
        imported.batch.num_rows(),
        imported.batch.num_columns()
    );
    Ok(())
}

fn query_paging(ctx: &Ctx) -> Check {
    let mut conn = ctx.connect()?;
    let mut problems = Problems::default();
    let numbers = ctx.fixture.numbers(30);
    let mut totals = BTreeSet::new();
    for (offset, limit, rows, truncated) in [
        (0, 10, 10, true),
        (0, 29, 29, true),
        (0, 30, 30, false), // exactly the whole result: the lookahead row proves it
        (0, 100, 30, false),
        (25, 10, 5, false),
        (25, 5, 5, false),
        (20, 5, 5, true),
        (30, 5, 0, false),
        (100, 5, 0, false),
    ] {
        match conn.open_page(&numbers, offset, limit, false) {
            Err(error) => problems.push(format!(
                "offset {offset} limit {limit}: {}",
                crate::harness::describe(&error)
            )),
            Ok(p) => {
                problems.check(p.row_count == rows && p.flags.truncated == truncated, || {
                    format!(
                        "offset {offset} limit {limit}: got row_count {} truncated {}, expected {rows} / {truncated}",
                        p.row_count, p.flags.truncated
                    )
                });
                if p.total_count.is_some() != p.total_is_exact {
                    totals.insert(format!(
                        "total_count {:?} with total_is_exact {}",
                        p.total_count, p.total_is_exact
                    ));
                }
                if let (Some(total), true) = (p.total_count, p.total_is_exact) {
                    problems.check(total == 30, || {
                        format!("exact total_count {total}, the result has 30 rows")
                    });
                }
            }
        }
    }
    for total in totals {
        problems.push(format!("{total} (total_count must be None unless exact)"));
    }
    match conn.open_page(&ctx.fixture.numbers(250), 0, 0, false) {
        Ok(page) => problems.check(page.row_count == 200 && page.flags.truncated, || {
            format!(
                "limit 0 must mean the default page size 200: got {} rows, truncated {}",
                page.row_count, page.flags.truncated
            )
        }),
        Err(error) => problems.push(format!("limit 0: {}", crate::harness::describe(&error))),
    }
    match conn.open_page(&ctx.fixture.numbers(100_005), 0, 1_000_000, false) {
        Ok(page) => problems.check(page.row_count == 100_000 && page.flags.truncated, || {
            format!(
                "pages are capped at 100000 rows: got {} rows, truncated {}",
                page.row_count, page.flags.truncated
            )
        }),
        Err(error) => problems.push(format!(
            "limit 1000000: {}",
            crate::harness::describe(&error)
        )),
    }
    // The window really starts at the offset.
    match conn.open_page(&numbers, 25, 3, false) {
        Ok(page) => {
            let batch = expect_ok(
                conn.session.render_cell_batch(page.page_id, 0, 3, 0, 1),
                "render",
            )?;
            let texts: Vec<_> = (0..batch.cell_count())
                .filter_map(|i| batch.cell(i))
                .map(|c| c.text)
                .collect();
            problems.check(texts == ["26", "27", "28"], || {
                format!("offset 25 limit 3 returned {texts:?}")
            });
        }
        Err(error) => problems.push(format!("offset 25: {}", crate::harness::describe(&error))),
    }
    problems.finish()
}

fn query_not_pageable(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let writes = ctx.fixture.write_statements(&scratch.scratch);
    ensure!(writes.len() >= 2, "fixture needs two write statements");
    let mut problems = Problems::default();
    let before = conn.count(&items)?;

    // A write runs once at offset 0 and reports truncated = false.
    match conn.open_page(&writes[0], 0, 1, false) {
        Ok(page) => problems.check(!page.flags.truncated, || {
            "a write page must report truncated = false".into()
        }),
        Err(error) => problems.push(format!(
            "write at offset 0: {}",
            crate::harness::describe(&error)
        )),
    }
    problems.check(conn.count(&items)? == before + 1, || {
        "the write at offset 0 must run exactly once".into()
    });

    // ... and is never re-run for offset > 0.
    let mut refused = |conn: &mut Conn, sql: &str, offset: u64, what: &str| {
        if let Err(Failure::Fail(message)) = expect_rejection(
            conn.open_page(sql, offset, 10, false),
            codes::QUERY_NOT_PAGEABLE,
            what,
        )
        .map(|_| ())
        {
            problems.push(message);
        }
    };
    refused(&mut conn, &writes[1], 10, "write at offset 10");
    refused(&mut conn, "SELECT 1; SELECT 2", 1, "script at offset 1");
    refused(
        &mut conn,
        &format!("DELETE FROM {items} WHERE tenant = 12345"),
        5,
        "delete at offset 5",
    );
    for (index, sql) in ctx
        .fixture
        .write_returning_statements(&scratch.scratch)
        .iter()
        .enumerate()
    {
        refused(
            &mut conn,
            sql,
            3,
            &format!("write-returning statement #{index} at offset 3"),
        );
    }
    let after = conn.count(&items)?;
    let unexpected = after - (before + 1);
    problems.check(unexpected == 0, || {
        format!("refused/violating paging requests executed {unexpected} extra write(s)")
    });

    // Pure reads page, including WITH ... SELECT.
    let with_select = format!("WITH x AS ({}) SELECT n FROM x", ctx.fixture.numbers(30));
    match conn.open_page(&with_select, 5, 10, false) {
        Ok(page) => problems.check(page.row_count == 10, || {
            format!("WITH ... SELECT page has {} rows", page.row_count)
        }),
        Err(error) => problems.push(format!(
            "WITH ... SELECT at offset 5 must be pageable: {}",
            crate::harness::describe(&error)
        )),
    }
    problems.finish()
}

fn query_confirmation(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let mut problems = Problems::default();
    conn.exec(&seed_items(&items))?;
    // A driver that lets CREATE DATABASE through must not leak the database.
    let confirm_database = format!("cfconfirm_{}", ctx.unique());
    let _drop_database = DropDatabase(ctx, ctx.fixture.drop_database(&confirm_database));

    // Each dangerous statement is refused, with a reason, and does not run. They
    // are run one by one: a statement a broken driver lets through must not hide
    // the verdict of the next one (the ones that would destroy a later check's
    // data are the last).
    let dangerous = [
        format!("ALTER TABLE {items} ADD COLUMN extra INT"),
        ctx.fixture.grant_statement(&scratch.scratch),
        ctx.fixture.create_database_statement(&confirm_database),
        format!("UPDATE {items} SET qty = (SELECT 1 WHERE 1 = 1)"),
        format!("SELECT 1; DROP TABLE {}", scratch.table("dups")),
        format!("/* c */ DROP TABLE {}", scratch.table("notes")),
        format!("DROP TABLE {}", scratch.table("refs")),
        format!("UPDATE {items} SET qty = 0"),
        format!("-- harmless looking\nDELETE FROM {items}"),
        format!("DELETE FROM {items}"),
        format!("TRUNCATE TABLE {items}"),
    ];
    for sql in &dangerous {
        match expect_rejection(
            conn.open_page(sql, 0, 10, false),
            codes::QUERY_CONFIRMATION_REQUIRED,
            &sql.replace('\n', " "),
        ) {
            Ok((_, message)) => problems.check(!message.trim().is_empty(), || {
                format!("`{}`: the refusal must explain why", sql.replace('\n', " "))
            }),
            Err(Failure::Fail(message)) => problems.push(message),
            Err(other) => return Err(other),
        }
    }
    // Whatever slipped through is reported above; reset for the safe checks.
    let _ = conn.exec(&ctx.fixture.delete_all(&items));
    if let Err(Failure::Fail(message)) = conn.exec(&seed_items(&items)) {
        problems.push(format!("could not re-seed after the refusals: {message}"));
        return problems.finish();
    }

    // Harmless statements and keywords inside strings/comments run unconfirmed.
    for sql in [
        format!("DELETE FROM {items} WHERE tenant = 1 AND id = 1"),
        format!("UPDATE {items} SET qty = 5 WHERE tenant = 1 AND id = 2"),
        "SELECT 'DROP TABLE x; DELETE FROM y'".to_owned(),
        "SELECT 1 /* DROP TABLE x */".to_owned(),
        format!("DELETE FROM {items} WHERE id IN (SELECT 99)"),
    ] {
        if let Err(error) = conn.run(&sql) {
            problems.push(format!(
                "`{sql}` must run without confirmation: {}",
                crate::harness::describe(&error)
            ));
        }
    }
    problems.check(conn.count(&items)? == 2, || {
        "the unconfirmed safe DELETE must have run".into()
    });

    // Confirmed: runs.
    match conn.open_page(&ctx.fixture.delete_all(&items), 0, 10, true) {
        Ok(_) => problems.check(conn.count(&items)? == 0, || {
            "the confirmed DELETE must have run".into()
        }),
        Err(error) => problems.push(format!(
            "confirmed DELETE: {}",
            crate::harness::describe(&error)
        )),
    }
    let dups = scratch.table("dups");
    // The table may already be gone if an unconfirmed DROP slipped through above.
    if conn
        .open_page(&format!("DROP TABLE {dups}"), 0, 10, true)
        .is_ok()
    {
        problems.check(conn.count(&dups).is_err(), || {
            "the confirmed DROP must have run".into()
        });
    }
    problems.finish()
}

/// Three rows in `items` (explicit columns: earlier statements of a scenario may
/// have altered the table if the driver under test lets them through).
fn seed_items(items: &str) -> String {
    format!(
        "INSERT INTO {items} (tenant, id, name, qty) VALUES (1, 1, 'a', 1), (1, 2, 'b', 2), (1, 3, 'c', 3)"
    )
}

/// Starts the slow query, lets it run, returns once it surely started.
fn start_slow(conn: &mut Conn) -> Result<crate::harness::Running, Failure> {
    let slow = conn.ctx.fixture.slow_query(SLOW_SECONDS);
    let running = conn.spawn_query(&slow)?;
    std::thread::sleep(Duration::from_millis(700));
    Ok(running)
}

fn query_cancelled(ctx: &Ctx) -> Check {
    let mut conn = ctx.connect()?;
    let running = start_slow(&mut conn)?;
    let started = Instant::now();
    conn.cancel(running.operation.id()).map_err(|e| {
        Failure::Fail(format!(
            "cancel_operation: {}",
            crate::harness::describe(&e)
        ))
    })?;
    let outcome = conn.join(running, Duration::from_secs(10));
    ensure!(
        started.elapsed() < Duration::from_secs(8),
        "the cancelled query kept running for {:?}",
        started.elapsed()
    );
    let (retryable, message) =
        expect_rejection(outcome, codes::QUERY_CANCELLED, "cancelled query")?;
    ensure!(!retryable, "a requested cancel is not retryable: {message}");
    // The connection survives.
    ensure!(
        conn.scalar("SELECT 1")? == "1",
        "the connection must stay usable after a cancel"
    );
    Ok(())
}

fn query_interrupted(ctx: &Ctx) -> Check {
    if !ctx.fixture.supports_external_interrupt() {
        return Err(Failure::Skip(format!(
            "{} cannot interrupt another session's query from SQL",
            ctx.fixture.label()
        )));
    }
    let mut victim = ctx.connect()?;
    let backend = victim.scalar(&ctx.fixture.backend_id_query())?;
    let running = start_slow(&mut victim)?;
    let mut killer = ctx.connect()?;
    killer
        .exec(&ctx.fixture.interrupt_backend_statement(&backend))
        .map_err(|e| match e {
            Failure::Fail(message) => {
                Failure::Fail(format!("could not interrupt backend {backend}: {message}"))
            }
            other => other,
        })?;
    let outcome = victim.join(running, Duration::from_secs(10));
    let (retryable, message) =
        expect_rejection(outcome, codes::QUERY_INTERRUPTED, "interrupted query")?;
    ensure!(
        retryable,
        "an unrequested interrupt must be retryable: {message}"
    );
    Ok(())
}

// ------------------------------------------------------------------ navigator

fn tables_page(conn: &mut Conn, request: &Value, what: &str) -> Result<NavigatorPage, Failure> {
    conn.nav(request)
        .map_err(|e| Failure::Fail(format!("{what}: {}", crate::harness::describe(&e))))
}

fn node<'a>(page: &'a NavigatorPage, label: &str) -> Result<&'a NavigatorNode, Failure> {
    page.nodes
        .iter()
        .find(|node| node.label == label)
        .ok_or_else(|| {
            Failure::Fail(format!(
                "no node {label:?}; got {:?}",
                page.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
            ))
        })
}

fn navigator_payloads(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let request = ctx.fixture.navigator_tables_request(&scratch.scratch);
    let page = tables_page(&mut conn, &request, "list tables")?;
    let items = node(&page, "items")?;
    ensure!(
        matches!(items.kind.as_str(), "table"),
        "items has kind {:?}",
        items.kind
    );
    let object: SqlObjectRef = serde_json::from_value(items.payload.clone()).map_err(|e| {
        Failure::Fail(format!(
            "table payload is not a SqlObjectRef: {e}: {}",
            items.payload
        ))
    })?;
    let expected = ctx.fixture.object_ref(&scratch.scratch, "items");
    ensure!(
        object == expected,
        "table payload {object:?}, expected {expected:?}"
    );
    if ctx
        .handle
        .capabilities()
        .has(Capability::MetadataTableProperties)
    {
        ensure!(
            items.actions.iter().any(|a| a == "describe"),
            "table nodes must advertise the describe action: {:?}",
            items.actions
        );
    }
    let kinds = &ctx.handle.capabilities().navigator_kinds;
    for node in &page.nodes {
        ensure!(
            kinds.contains(&node.kind),
            "node {:?} has kind {:?} which is not in navigator_kinds {kinds:?}",
            node.label,
            node.kind
        );
    }
    // The payload is echoed back unchanged to expand the node.
    let columns = tables_page(
        &mut conn,
        &json!({
            "parent": items.id,
            "database": items.payload.get("database").cloned().unwrap_or(Value::Null),
            "payload": items.payload,
            "cursor": null,
            "limit": 0
        }),
        "expand table",
    )?;
    let names: Vec<_> = columns.nodes.iter().map(|n| n.label.as_str()).collect();
    ensure!(
        names == ["tenant", "id", "name", "qty"],
        "columns of items: {names:?}"
    );
    for node in &columns.nodes {
        ensure!(node.kind == "column", "column node kind {:?}", node.kind);
        let column: SqlColumnNode = serde_json::from_value(node.payload.clone()).map_err(|e| {
            Failure::Fail(format!(
                "column payload is not a SqlColumnNode: {e}: {}",
                node.payload
            ))
        })?;
        ensure!(
            column.object == expected,
            "column payload object {:?}",
            column.object
        );
        ensure!(
            column.column == node.label,
            "column payload name {:?}",
            column.column
        );
        let key = matches!(column.column.as_str(), "tenant" | "id");
        ensure!(
            column.details.primary_key == key,
            "{}: primary_key {} (expected {key})",
            column.column,
            column.details.primary_key
        );
        ensure!(
            column.details.nullable == matches!(column.column.as_str(), "name" | "qty"),
            "{}: nullable {}",
            column.column,
            column.details.nullable
        );
    }
    Ok(())
}

fn labels(page: &NavigatorPage) -> BTreeSet<String> {
    page.nodes.iter().map(|n| n.label.clone()).collect()
}

fn navigator_limit(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let base = ctx.fixture.navigator_tables_request(&scratch.scratch);
    let all = tables_page(&mut conn, &base, "limit 0")?;
    ensure!(
        all.next_cursor.is_none() && all.nodes.len() >= 4,
        "limit 0 must return every object of a small scratch ({} nodes, cursor {:?})",
        all.nodes.len(),
        all.next_cursor
    );
    let expected = labels(&all);
    let mut seen = Vec::new();
    let mut cursor = None;
    for round in 0..10 {
        let mut request = base.clone();
        request["limit"] = json!(2);
        request["cursor"] = json!(cursor);
        let page = tables_page(&mut conn, &request, &format!("limit 2, page {round}"))?;
        ensure!(
            page.nodes.len() <= 2,
            "limit 2 returned {} nodes",
            page.nodes.len()
        );
        seen.extend(page.nodes.iter().map(|n| n.label.clone()));
        if page.next_cursor.is_none() {
            ensure!(
                round >= 1,
                "limit 2 over {} objects must return a cursor",
                expected.len()
            );
            break;
        }
        cursor = page.next_cursor;
    }
    let unique: BTreeSet<_> = seen.iter().cloned().collect();
    ensure!(
        seen.len() == unique.len(),
        "cursor paging repeated nodes: {seen:?}"
    );
    ensure!(
        unique == expected,
        "cursor paging saw {unique:?}, expected {expected:?}"
    );
    let mut huge = base.clone();
    huge["limit"] = json!(u64::MAX);
    let page = tables_page(&mut conn, &huge, "limit u64::MAX")?;
    ensure!(
        page.nodes.len() >= 4,
        "an oversized limit is capped, not refused"
    );
    Ok(())
}

fn navigator_filter(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let base = ctx.fixture.navigator_tables_request(&scratch.scratch);
    let with_filter = |filter: &str| {
        let mut request = base.clone();
        request["payload"]["filter"] = json!(filter);
        request
    };
    for (filter, expected) in [
        ("ITEM", vec!["items"]),
        ("item", vec!["items"]),
        ("Tem", vec!["items"]),
        ("nonexistent-zz", vec![]),
        // Literal substring: `_` and `%` are not LIKE wildcards.
        ("_", vec![]),
        ("%", vec![]),
        ("i_ems", vec![]),
    ] {
        let page = tables_page(
            &mut conn,
            &with_filter(filter),
            &format!("filter {filter:?}"),
        )?;
        let got: Vec<_> = labels(&page).into_iter().collect();
        ensure!(
            got == expected,
            "filter {filter:?} returned {got:?}, expected {expected:?}"
        );
    }
    Ok(())
}

fn metadata_not_blocked(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let request = ctx.fixture.navigator_tables_request(&scratch.scratch);
    let object = serde_json::to_value(ctx.fixture.object_ref(&scratch.scratch, "items"))
        .map_err(|e| e.to_string())?;
    let slow = ctx.fixture.slow_query(12);
    let running = conn.spawn_query(&slow)?;
    std::thread::sleep(Duration::from_millis(700));

    let started = Instant::now();
    let nav = conn.nav(&request);
    let nav_took = started.elapsed();
    let started = Instant::now();
    let describe = conn.describe(&object);
    let describe_took = started.elapsed();
    // Always stop the slow query before judging.
    let _ = conn.cancel(running.operation.id());
    let _ = conn.join(running, Duration::from_secs(15));
    expect_ok(
        nav.map_err(|e| crate::harness::describe(&e)),
        "navigator during a running query",
    )?;
    expect_ok(
        describe.map_err(|e| crate::harness::describe(&e)),
        "describe during a running query",
    )?;
    ensure!(
        nav_took < Duration::from_secs(4),
        "the navigator waited {nav_took:?} behind a running user query"
    );
    ensure!(
        describe_took < Duration::from_secs(4),
        "describe waited {describe_took:?} behind a running user query"
    );
    Ok(())
}

// ------------------------------------------------------------------ mutations

fn change_insert(values: Value) -> Value {
    json!({"op": "insert", "values": values})
}
fn change_update(key: Value, values: Value) -> Value {
    json!({"op": "update", "key": key, "values": values})
}
fn change_delete(key: Value) -> Value {
    json!({"op": "delete", "key": key})
}

fn mutation_crud(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let s = &scratch.scratch;
    let inserted = expect_ok(
        conn.mutate(
            s,
            "items",
            "insert",
            change_insert(json!({"tenant": 1, "id": 1, "name": "a", "qty": 5})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "insert",
    )?;
    ensure!(
        inserted.affected_rows == 1,
        "insert affected {}",
        inserted.affected_rows
    );
    let row = conn.rows(&format!(
        "SELECT name, qty FROM {items} WHERE tenant = 1 AND id = 1"
    ))?;
    ensure!(row == [["a", "5"]], "inserted row: {}", rows_text(&row));
    let updated = expect_ok(
        conn.mutate(
            s,
            "items",
            "update",
            change_update(json!({"tenant": 1, "id": 1}), json!({"qty": 6})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "update",
    )?;
    ensure!(
        updated.affected_rows == 1,
        "update affected {}",
        updated.affected_rows
    );
    ensure!(
        conn.scalar(&format!(
            "SELECT qty FROM {items} WHERE tenant = 1 AND id = 1"
        ))? == "6",
        "update did not apply"
    );
    // Composite key: only the full key identifies the row.
    expect_rejection(
        conn.mutate(
            s,
            "items",
            "update",
            change_update(json!({"tenant": 1, "id": 99}), json!({"qty": 1})),
        ),
        codes::MUTATION_NO_ROWS_AFFECTED,
        "update of a missing key",
    )?;
    expect_rejection(
        conn.mutate(
            s,
            "items",
            "delete",
            change_delete(json!({"tenant": 2, "id": 1})),
        ),
        codes::MUTATION_NO_ROWS_AFFECTED,
        "delete of a missing key",
    )?;
    let deleted = expect_ok(
        conn.mutate(
            s,
            "items",
            "delete",
            change_delete(json!({"tenant": 1, "id": 1})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "delete",
    )?;
    ensure!(
        deleted.affected_rows == 1,
        "delete affected {}",
        deleted.affected_rows
    );
    ensure!(conn.count(&items)? == 0, "delete did not apply");
    Ok(())
}

fn mutation_null_key(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let notes = scratch.table("notes");
    let s = &scratch.scratch;
    conn.exec(&format!(
        "INSERT INTO {notes} VALUES (NULL, 'a'), ('x', 'b')"
    ))?;
    expect_ok(
        conn.mutate(
            s,
            "notes",
            "update",
            change_update(json!({"owner": null}), json!({"body": "z"})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "update with a null key",
    )?;
    ensure!(
        conn.scalar(&format!("SELECT body FROM {notes} WHERE owner IS NULL"))? == "z",
        "a null key must match the row whose key IS NULL"
    );
    ensure!(
        conn.scalar(&format!("SELECT body FROM {notes} WHERE owner = 'x'"))? == "b",
        "the other row must be untouched"
    );
    expect_ok(
        conn.mutate(s, "notes", "delete", change_delete(json!({"owner": null})))
            .map_err(|e| crate::harness::describe(&e)),
        "delete with a null key",
    )?;
    ensure!(
        conn.count(&notes)? == 1,
        "delete with a null key removed the wrong rows"
    );
    Ok(())
}

fn mutation_identity_not_unique(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let dups = scratch.table("dups");
    let s = &scratch.scratch;
    conn.exec(&format!("INSERT INTO {dups} VALUES (1, 1), (1, 2)"))?;
    expect_rejection(
        conn.mutate(
            s,
            "dups",
            "update",
            change_update(json!({"k": 1}), json!({"v": 9})),
        ),
        codes::MUTATION_IDENTITY_NOT_UNIQUE,
        "update matching two rows",
    )?;
    expect_rejection(
        conn.mutate(s, "dups", "delete", change_delete(json!({"k": 1}))),
        codes::MUTATION_IDENTITY_NOT_UNIQUE,
        "delete matching two rows",
    )?;
    ensure!(conn.count(&dups)? == 2, "the failed delete must roll back");
    ensure!(
        conn.scalar(&format!("SELECT SUM(v) FROM {dups}"))?
            .starts_with('3'),
        "the failed update must roll back"
    );
    Ok(())
}

fn mutation_zero_row_insert(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let Some(setup) = ctx.fixture.zero_row_insert_setup(&scratch.scratch) else {
        return Err(Failure::Skip(
            "the dialect cannot express a zero-row insert".into(),
        ));
    };
    let mut conn = ctx.connect()?;
    for sql in setup {
        conn.exec(&sql)?;
    }
    expect_rejection(
        conn.mutate(
            &scratch.scratch,
            "swallow",
            "insert",
            change_insert(json!({"id": 1})),
        ),
        codes::MUTATION_NO_ROWS_AFFECTED,
        "insert that affects zero rows",
    )?;
    Ok(())
}

fn mutation_unsupported_kind(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    // The host answers for kinds the driver does not advertise
    // (`UnsupportedFeature`, before calling the driver); a driver that is asked
    // anyway must answer `mutation.unsupported`. Either way the kind is refused.
    match conn.mutate(&scratch.scratch, "items", "frobnicate", json!({})) {
        Err(LoaderError::UnsupportedFeature { feature }) => {
            ensure!(feature.contains("frobnicate"), "feature {feature:?}");
            Ok(())
        }
        other => expect_rejection(other, codes::MUTATION_UNSUPPORTED, "unknown mutation kind")
            .map(|_| ()),
    }
}

fn mutation_batch(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let s = &scratch.scratch;
    let applied = expect_ok(
        conn.mutate(
            s,
            "items",
            "batch",
            json!({"changes": [
                change_insert(json!({"tenant": 2, "id": 1, "name": "a", "qty": 1})),
                change_insert(json!({"tenant": 2, "id": 2, "name": "b", "qty": 2})),
                change_update(json!({"tenant": 2, "id": 1}), json!({"qty": 10})),
            ]}),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "batch",
    )?;
    ensure!(
        applied.affected_rows == 3,
        "batch affected {}",
        applied.affected_rows
    );
    ensure!(
        conn.scalar(&format!(
            "SELECT qty FROM {items} WHERE tenant = 2 AND id = 1"
        ))? == "10"
            && conn.count(&format!("{items} WHERE tenant = 2"))? == 2,
        "the batch did not apply in order"
    );
    // All-or-nothing: the second change fails, the first is rolled back.
    expect_rejection(
        conn.mutate(
            s,
            "items",
            "batch",
            json!({"changes": [
                change_insert(json!({"tenant": 3, "id": 1, "name": "x", "qty": 1})),
                change_update(json!({"tenant": 3, "id": 99}), json!({"qty": 2})),
            ]}),
        ),
        codes::MUTATION_NO_ROWS_AFFECTED,
        "batch with a failing change",
    )?;
    ensure!(
        conn.count(&format!("{items} WHERE tenant = 3"))? == 0,
        "a failed batch must roll back every change"
    );
    Ok(())
}

fn mutation_values_are_bound(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let s = &scratch.scratch;
    let nasty = format!("x'); DROP TABLE {items}; --");
    expect_ok(
        conn.mutate(
            s,
            "items",
            "insert",
            change_insert(json!({"tenant": 4, "id": 1, "name": nasty, "qty": 7})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "insert of a value that looks like SQL",
    )?;
    ensure!(
        conn.scalar(&format!(
            "SELECT name FROM {items} WHERE tenant = 4 AND id = 1"
        ))? == nasty,
        "the value must be stored verbatim (bound as a parameter)"
    );
    // Arrays and objects are sent as JSON text.
    for (id, value) in [
        (2, json!([1, 2, {"a": null}])),
        (3, json!({"k": ["v", 1.5]})),
    ] {
        expect_ok(
            conn.mutate(
                s,
                "items",
                "insert",
                change_insert(json!({"tenant": 4, "id": id, "name": value})),
            )
            .map_err(|e| crate::harness::describe(&e)),
            "insert of a JSON value",
        )?;
        let stored = conn.scalar(&format!(
            "SELECT name FROM {items} WHERE tenant = 4 AND id = {id}"
        ))?;
        let parsed: Value = serde_json::from_str(&stored)
            .map_err(|_| Failure::Fail(format!("stored {stored:?} is not JSON text")))?;
        ensure!(parsed == value, "stored JSON {parsed} differs from {value}");
    }
    Ok(())
}

fn mutation_in_user_transaction(ctx: &Ctx) -> Check {
    if !ctx.fixture.supports_transactions() {
        return Err(Failure::Skip(format!(
            "{} has no transactions to join: a mutation is not part of anything a user could roll back",
            ctx.fixture.label()
        )));
    }
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let s = &scratch.scratch;
    let begin = ctx.fixture.begin().to_owned();

    // Rolled back by the user: the mutation joined the transaction.
    conn.exec(&begin)?;
    conn.exec(&format!(
        "INSERT INTO {items} (tenant, id, name, qty) VALUES (7, 1, 'user', 1)"
    ))?;
    expect_ok(
        conn.mutate(
            s,
            "items",
            "insert",
            change_insert(json!({"tenant": 7, "id": 2, "name": "m", "qty": 2})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "mutation inside a user transaction",
    )?;
    // A failing mutation rolls back to its savepoint only.
    expect_rejection(
        conn.mutate(
            s,
            "items",
            "update",
            change_update(json!({"tenant": 7, "id": 99}), json!({"qty": 1})),
        ),
        codes::MUTATION_NO_ROWS_AFFECTED,
        "failing mutation inside a user transaction",
    )?;
    expect_rejection(
        conn.mutate(
            s,
            "items",
            "insert",
            change_insert(json!({"tenant": 7, "id": 2, "name": "dup", "qty": 2})),
        ),
        codes::MUTATION_FAILED,
        "duplicate-key insert inside a user transaction",
    )?;
    ensure!(
        conn.count(&format!("{items} WHERE tenant = 7"))? == 2,
        "the user's work and the successful mutation must still be pending (failed ones rolled back to their savepoint)"
    );
    conn.exec("ROLLBACK")?;
    ensure!(
        conn.count(&format!("{items} WHERE tenant = 7"))? == 0,
        "the user's ROLLBACK must undo the mutation too: it must not commit the user's transaction"
    );

    // Committed by the user: persists, visible to another connection.
    conn.exec(&begin)?;
    expect_ok(
        conn.mutate(
            s,
            "items",
            "insert",
            change_insert(json!({"tenant": 8, "id": 1, "name": "m", "qty": 2})),
        )
        .map_err(|e| crate::harness::describe(&e)),
        "mutation inside a second user transaction",
    )?;
    let mut other = ctx.connect()?;
    ensure!(
        other.count(&format!("{items} WHERE tenant = 8"))? == 0,
        "the mutation must not be visible to others before the user's COMMIT"
    );
    conn.exec("COMMIT")?;
    ensure!(
        other.count(&format!("{items} WHERE tenant = 8"))? == 1,
        "the user's COMMIT must persist the mutation"
    );
    Ok(())
}

/// Drops a database created by a scenario, even when the scenario fails.
struct DropDatabase<'a>(&'a Ctx<'a>, Vec<String>);

impl Drop for DropDatabase<'_> {
    fn drop(&mut self) {
        self.0.cleanup(&self.1);
    }
}

fn create_database(ctx: &Ctx) -> Check {
    if !ctx.fixture.supports_create_database()
        || !ctx.handle.capabilities().has(Capability::DatabaseCreate)
    {
        return Err(Failure::Skip("database.create is not advertised".into()));
    }
    let name = ctx.fixture.new_database_name(&ctx.unique());
    let _cleanup = DropDatabase(ctx, ctx.fixture.drop_database(&name));
    let mut conn = ctx.connect()?;
    let create = |conn: &mut Conn| {
        let operation = conn.session.begin_operation()?;
        conn.session.mutate(&engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: "create_database".into(),
            database: Some(name.clone()),
            object: None,
            identity: Value::Null,
            payload: Value::Null,
        })
    };
    let created = expect_ok(
        create(&mut conn).map_err(|e| crate::harness::describe(&e)),
        "create_database",
    )?;
    ensure!(
        created.affected_rows == 0,
        "create_database affected_rows {}",
        created.affected_rows
    );
    expect_rejection(
        create(&mut conn),
        codes::CREATE_DATABASE_REJECTED,
        "creating an existing database",
    )?;
    Ok(())
}

// ------------------------------------------------------------------- describe

fn describe_object(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let object = |table: &str| {
        serde_json::to_value(ctx.fixture.object_ref(&scratch.scratch, table))
            .map_err(|e| e.to_string())
    };
    let value = conn
        .describe(&object("items")?)
        .map_err(|e| Failure::Fail(format!("describe items: {}", crate::harness::describe(&e))))?;
    let properties: TableProperties = serde_json::from_value(value.clone())
        .map_err(|e| Failure::Fail(format!("response is not TableProperties: {e}: {value}")))?;
    let names: Vec<_> = properties.columns.iter().map(|c| c.name.as_str()).collect();
    ensure!(
        names == ["tenant", "id", "name", "qty"],
        "columns {names:?}"
    );
    for column in &properties.columns {
        ensure!(
            column.primary_key == matches!(column.name.as_str(), "tenant" | "id"),
            "{}: primary_key {}",
            column.name,
            column.primary_key
        );
        ensure!(
            column.nullable == matches!(column.name.as_str(), "name" | "qty"),
            "{}: nullable {}",
            column.name,
            column.nullable
        );
    }
    let keys = properties.keys.as_ref().ok_or("keys is absent")?;
    ensure!(
        keys.iter()
            .any(|key| key.kind == ConstraintKind::PrimaryKey && key.columns == ["tenant", "id"]),
        "no composite primary key in {keys:?}"
    );
    if ctx.fixture.supports_foreign_keys() {
        let refs: TableProperties =
            serde_json::from_value(conn.describe(&object("refs")?).map_err(|e| {
                Failure::Fail(format!("describe refs: {}", crate::harness::describe(&e)))
            })?)
            .map_err(|e| e.to_string())?;
        let foreign_keys = refs
            .foreign_keys
            .as_ref()
            .ok_or("foreign_keys is absent for refs")?;
        ensure!(
            foreign_keys
                .iter()
                .any(|fk| fk.referenced_table.ends_with("items")
                    && fk.columns == ["tenant", "item_id"]
                    && fk.referenced_columns == ["tenant", "id"]),
            "no composite foreign key to items in {foreign_keys:?}"
        );
        if let Some(references) = properties.references.as_ref() {
            ensure!(
                references.iter().any(|fk| fk.table.ends_with("refs")),
                "items.references should list refs: {references:?}"
            );
        }
    }
    expect_rejection(
        conn.describe(&object("no_such_table_zz")?),
        codes::DESCRIBE_NOT_FOUND,
        "describe of an unknown object",
    )?;
    Ok(())
}

// ------------------------------------------------- non-pageable truncation etc.

fn query_nonpageable_truncation(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let (Some(first), Some(second)) = (
        ctx.fixture.returning_write(&scratch.scratch, 6),
        ctx.fixture.returning_write(&scratch.scratch, 7),
    ) else {
        return Err(Failure::Skip(
            "the fixture has no row-returning write".into(),
        ));
    };
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let mut problems = Problems::default();

    // Three rows come back, the page limit is one: the extra rows are dropped,
    // the page is not truncated (there is no "next page" of a write), and the
    // write ran exactly once.
    match conn.open_page(&first, 0, 1, false) {
        Ok(page) => {
            problems.check(page.row_count <= 1, || {
                format!("limit 1 but the page has {} rows", page.row_count)
            });
            problems.check(!page.flags.truncated, || {
                "a non-pageable page must report truncated = false even when rows were dropped"
                    .into()
            });
        }
        Err(error) => problems.push(format!(
            "row-returning write at offset 0: {}",
            crate::harness::describe(&error)
        )),
    }
    problems.check(
        conn.count(&format!("{items} WHERE tenant = 6"))? == 3,
        || "the write must run exactly once at offset 0".into(),
    );

    // A page request for offset > 0 is refused and does NOT run it again.
    if let Err(Failure::Fail(message)) = expect_rejection(
        conn.open_page(&second, 2, 1, false),
        codes::QUERY_NOT_PAGEABLE,
        "row-returning write at offset 2",
    )
    .map(|_| ())
    {
        problems.push(message);
    }
    problems.check(
        conn.count(&format!("{items} WHERE tenant = 7"))? == 0,
        || "a refused page request must not execute the statement".into(),
    );
    // The same statement with a large limit still returns all rows, once.
    match conn.open_page(&second, 0, 100, false) {
        Ok(page) => problems.check(page.row_count == 3 && !page.flags.truncated, || {
            format!(
                "limit 100: got {} rows, truncated {}",
                page.row_count, page.flags.truncated
            )
        }),
        Err(error) => problems.push(format!("limit 100: {}", crate::harness::describe(&error))),
    }
    problems.check(
        conn.count(&format!("{items} WHERE tenant = 7"))? == 3,
        || "the statement must have run once".into(),
    );
    problems.finish()
}

fn query_nested_with_write(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let Some(statement) = ctx.fixture.nested_with_delete_statement(&scratch.scratch) else {
        return Err(Failure::Skip(
            "the dialect has no data-modifying CTEs".into(),
        ));
    };
    let mut conn = ctx.connect()?;
    let items = scratch.table("items");
    let mut problems = Problems::default();
    conn.exec(&seed_items(&items))?;

    // Unconfirmed: refused (DELETE without WHERE inside a nested WITH), untouched.
    match expect_rejection(
        conn.open_page(&statement, 0, 10, false),
        codes::QUERY_CONFIRMATION_REQUIRED,
        "nested WITH … DELETE without WHERE",
    ) {
        Ok(_) => {}
        Err(Failure::Fail(message)) => problems.push(message),
        Err(other) => return Err(other),
    }
    let remaining = conn.count(&items)?;
    problems.check(remaining == 3, || {
        format!("the unconfirmed nested DELETE ran ({remaining} of 3 rows left)")
    });
    if remaining != 3 {
        conn.exec(&ctx.fixture.delete_all(&items))?;
        conn.exec(&seed_items(&items))?;
    }

    // Offset > 0 is not pageable, even confirmed: it must not run.
    if let Err(Failure::Fail(message)) = expect_rejection(
        conn.open_page(&statement, 5, 10, true),
        codes::QUERY_NOT_PAGEABLE,
        "confirmed nested WITH at offset 5",
    )
    .map(|_| ())
    {
        problems.push(message);
    }
    let remaining = conn.count(&items)?;
    problems.check(remaining == 3, || {
        format!("a refused page request executed the nested DELETE ({remaining} of 3 rows left)")
    });
    if remaining != 3 {
        conn.exec(&seed_items(&items)).ok();
    }

    // Confirmed at offset 0: runs once; the page is not truncated.
    let before = conn.count(&items)?;
    match conn.open_page(&statement, 0, 1, true) {
        Ok(page) => problems.check(!page.flags.truncated, || {
            "the nested-WITH page must report truncated = false".into()
        }),
        Err(error) => problems.push(format!(
            "confirmed nested WITH: {}",
            crate::harness::describe(&error)
        )),
    }
    problems.check(conn.count(&items)? == 0 && before > 0, || {
        "the confirmed nested DELETE must have removed the rows".into()
    });

    // The lexer-hostile forms (SEARCH / CYCLE before the DML) behave the same.
    for (sql, deletes_all) in ctx.fixture.nested_with_variants(&scratch.scratch) {
        let label: String = sql.chars().take(90).collect();
        conn.exec(&ctx.fixture.delete_all(&items))?;
        conn.exec(&seed_items(&items))?;
        if deletes_all {
            match expect_rejection(
                conn.open_page(&sql, 0, 10, false),
                codes::QUERY_CONFIRMATION_REQUIRED,
                &label,
            ) {
                Ok(_) => {}
                Err(Failure::Fail(message)) => problems.push(message),
                Err(other) => return Err(other),
            }
            let left = conn.count(&items)?;
            problems.check(left == 3, || {
                format!("`{label}…` ran without confirmation ({left} of 3 rows left)")
            });
            conn.exec(&ctx.fixture.delete_all(&items))?;
            conn.exec(&seed_items(&items))?;
        }
        // Never executed for offset > 0 (even confirmed), and exactly once at 0.
        let before = conn.count(&items)?;
        if let Err(Failure::Fail(message)) = expect_rejection(
            conn.open_page(&sql, 5, 10, true),
            codes::QUERY_NOT_PAGEABLE,
            &format!("{label}… at offset 5"),
        )
        .map(|_| ())
        {
            problems.push(message);
        }
        let after_refused = conn.count(&items)?;
        problems.check(after_refused == before, || {
            format!("`{label}…`: a refused page request executed the statement ({before} -> {after_refused} rows)")
        });
        if let Err(error) = conn.open_page(&sql, 0, 10, true) {
            problems.push(format!(
                "`{label}…` confirmed: {}",
                crate::harness::describe(&error)
            ));
        }
        let after = conn.count(&items)?;
        let expected = if deletes_all { 0 } else { before + 1 };
        problems.check(after == expected, || {
            format!(
                "`{label}…` must run exactly once: {before} -> {after} rows, expected {expected}"
            )
        });
    }
    problems.finish()
}

fn query_script_last_result_set(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let mut conn = ctx.connect()?;
    let writes = ctx.fixture.write_statements(&scratch.scratch);
    ensure!(writes.len() >= 3, "fixture needs three write statements");
    let mut problems = Problems::default();

    let names = |conn: &mut Conn, script: &str| -> Result<(Vec<String>, u64), Failure> {
        let page = conn
            .open_page(script, 0, 10, false)
            .map_err(|e| Failure::Fail(format!("`{script}`: {}", crate::harness::describe(&e))))?;
        Ok((
            page.columns.iter().map(|c| c.name.to_lowercase()).collect(),
            page.row_count,
        ))
    };
    match names(&mut conn, "SELECT 1 AS a; SELECT 2 AS b") {
        Ok((columns, rows)) => problems.check(columns == ["b"] && rows == 1, || {
            format!("`SELECT 1 AS a; SELECT 2 AS b` returned columns {columns:?} / {rows} rows, expected [b] / 1")
        }),
        Err(Failure::Fail(message)) => problems.push(message),
        Err(other) => return Err(other),
    }
    // A trailing write has no result set: the last SELECT's rows are returned.
    let script = format!("SELECT 1 AS a; {}", writes[0]);
    match names(&mut conn, &script) {
        Ok((columns, rows)) => problems.check(columns == ["a"] && rows == 1, || {
            format!("select + insert returned columns {columns:?} / {rows} rows, expected [a] / 1")
        }),
        Err(Failure::Fail(message)) => problems.push(message),
        Err(other) => return Err(other),
    }
    // Only writes: a zero-column page.
    let script = format!("{}; {}", writes[1], writes[2]);
    match names(&mut conn, &script) {
        Ok((columns, _)) => problems.check(columns.is_empty(), || {
            format!("a script of writes must return a zero-column page, got {columns:?}")
        }),
        Err(Failure::Fail(message)) => problems.push(message),
        Err(other) => return Err(other),
    }
    // Every statement ran exactly once.
    let count = conn.count(&scratch.table("items"))?;
    problems.check(count == 3, || {
        format!("the three inserts of the scripts must each run once ({count} rows)")
    });
    problems.finish()
}

// ------------------------------------------------------------- navigator caps

fn navigator_default_and_cap(ctx: &Ctx) -> Check {
    // More than the 5 000 cap; one extra for good measure.
    const OBJECTS: u32 = 5_010;
    let scratch = ctx.scratch(false)?;
    let Some(statements) = ctx
        .fixture
        .bulk_object_statements(&scratch.scratch, OBJECTS)
    else {
        return Err(Failure::Skip(
            "the fixture cannot create thousands of tables".into(),
        ));
    };
    // Declared after the scratch guard, so it runs first: the tables are dropped
    // in small transactions before the schema goes.
    struct BulkCleanup<'a>(&'a Ctx<'a>, Vec<String>);
    impl Drop for BulkCleanup<'_> {
        fn drop(&mut self) {
            self.0.cleanup(&self.1);
        }
    }
    let _bulk_cleanup = BulkCleanup(
        ctx,
        ctx.fixture.bulk_object_cleanup(&scratch.scratch, OBJECTS),
    );
    let mut conn = ctx.connect()?;
    for sql in statements {
        conn.exec(&sql)?;
    }
    let base = ctx.fixture.navigator_tables_request(&scratch.scratch);
    let mut problems = Problems::default();
    let mut ask = |limit: u64| -> Result<NavigatorPage, Failure> {
        let mut request = base.clone();
        request["limit"] = json!(limit);
        tables_page(&mut conn, &request, &format!("limit {limit}"))
    };
    let default = ask(0)?;
    problems.check(
        default.nodes.len() == 500 && default.next_cursor.is_some(),
        || {
            format!(
                "limit 0 must return the default 500 nodes and a cursor: got {} nodes, cursor {:?}",
                default.nodes.len(),
                default.next_cursor
            )
        },
    );
    for limit in [6_000, u64::MAX] {
        let capped = ask(limit)?;
        problems.check(capped.nodes.len() == 5_000 && capped.next_cursor.is_some(), || {
            format!(
                "limit {limit} must be capped at 5000 nodes with a cursor: got {} nodes, cursor {:?}",
                capped.nodes.len(),
                capped.next_cursor
            )
        });
    }
    problems.finish()
}

// ------------------------------------------------------- cancelled mutations

fn mutation_cancelled(ctx: &Ctx) -> Check {
    let scratch = ctx.scratch(true)?;
    let Some(setup) = ctx.fixture.slow_mutation_setup(&scratch.scratch, &ctx.uri) else {
        return Err(Failure::Skip("the fixture has no slow mutation".into()));
    };
    let mut conn = ctx.connect()?;
    let mut admin = if ctx.fixture.slow_mutation_setup_as_admin() {
        ctx.admin_connect()?
    } else {
        None
    };
    for sql in setup {
        let setup_conn = admin.as_mut().unwrap_or(&mut conn);
        if let Err(Failure::Fail(message)) = setup_conn.exec(&sql) {
            return Err(Failure::Skip(format!(
                "the server refused the slow-mutation setup: {message}"
            )));
        }
    }
    let target = ctx.fixture.mutation_target(&scratch.scratch, "slow");
    let mut worker = ctx.connect()?;
    let operation = worker.session.begin_operation()?;
    let operation_id = operation.id();
    let connection_id = worker.session.connection_id();
    let request = engine_driver_api::MutationRequest {
        operation_id,
        kind: "insert".into(),
        database: target.database,
        object: Some(target.object),
        identity: target.identity,
        payload: change_insert(json!({"id": 1})),
    };
    let outcome = std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = &mut worker;
        scope.spawn(move || {
            let _ = sender.send(worker.session.mutate(&request));
        });
        std::thread::sleep(Duration::from_millis(700));
        let started = Instant::now();
        let cancelled = ctx.handle.cancel_operation(
            connection_id,
            &engine_driver_api::CancelRequest { operation_id },
        );
        (
            cancelled,
            receiver.recv_timeout(Duration::from_secs(10)),
            started,
        )
    });
    drop(operation);
    let (cancelled, result, started) = outcome;
    cancelled.map_err(|e| {
        Failure::Fail(format!(
            "cancel_operation: {}",
            crate::harness::describe(&e)
        ))
    })?;
    let result =
        result.map_err(|_| Failure::Fail("the cancelled mutation kept running for 10 s".into()))?;
    ensure!(
        started.elapsed() < Duration::from_secs(8),
        "the mutation took {:?} to stop after the cancel",
        started.elapsed()
    );
    let (retryable, message) =
        expect_rejection(result, codes::MUTATION_CANCELLED, "cancelled mutation")?;
    ensure!(!retryable, "a requested cancel is not retryable: {message}");
    Ok(())
}

fn create_database_bad_payload(ctx: &Ctx) -> Check {
    if !ctx.fixture.supports_create_database()
        || !ctx.handle.capabilities().has(Capability::DatabaseCreate)
    {
        return Err(Failure::Skip("database.create is not advertised".into()));
    }
    let name = ctx.fixture.new_database_name(&ctx.unique());
    let _cleanup = DropDatabase(ctx, ctx.fixture.drop_database(&name));
    let mut conn = ctx.connect()?;
    let operation = conn.session.begin_operation()?;
    let result = conn.session.mutate(&engine_driver_api::MutationRequest {
        operation_id: operation.id(),
        kind: "create_database".into(),
        database: Some(name),
        object: None,
        identity: Value::Null,
        payload: json!({"unexpected": true}),
    });
    expect_rejection(
        result,
        codes::MUTATION_BAD_REQUEST,
        "create_database with a payload",
    )?;
    Ok(())
}
