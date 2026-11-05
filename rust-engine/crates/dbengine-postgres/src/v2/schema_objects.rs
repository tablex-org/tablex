//! Schema objects that have source text instead of columns: functions,
//! procedures, sequences, triggers and types (enum, composite, domain), plus the
//! source of a materialized view.
//!
//! * Listing (`navigator`, `payload.kind`): one page of a kind in a schema, with
//!   the name filter and the page window bound as parameters. Functions,
//!   procedures and types that belong to an extension are left out (they are the
//!   extension's, and `CREATE EXTENSION` makes them), aggregates too (the server
//!   cannot print them with `pg_get_functiondef`).
//! * Source (`describe_object` with a [`SchemaObjectRef`]): the server's own
//!   printer where it has one (`pg_get_functiondef`, `pg_get_triggerdef`,
//!   `pg_get_viewdef`), otherwise a `CREATE` statement composed in SQL from the
//!   catalog with `format('%I')` / `quote_literal`, so no name is ever spliced
//!   into SQL text by this driver: every name is a bound parameter.
//!
//! Overloads are told apart by their input argument types, which together with
//! the name and the schema are the key of `pg_proc`'s unique index. The label
//! prints them (`oidvectortypes`, so it follows the session's `search_path`); the
//! lookup uses their OIDs (`proargtypes::text`, carried as the reference's
//! `identity`), which a changed `search_path` cannot alter.
//!
//! Server versions: PostgreSQL 11 added `prokind` and procedures; before it
//! aggregates are `proisagg` and there are no procedures. `pg_sequence` is 10+; an
//! older server lists no sequences. Both are chosen from `server_version_num`
//! (see [`Catalog`]).

use crate::db::postgres::{pg_like_contains_pattern, v2_error_detail, PostgresConn};
use engine_driver_kit::api::sql::{SchemaObjectKind, SchemaObjectRef};
use engine_driver_kit::api::NavigatorNode;
use serde_json::Value;

/// Rows are `(name, arguments, table, identity)`; the last three are `NULL` where the kind has none.
const FUNCTIONS_SQL: &str = "SELECT p.proname::text, oidvectortypes(p.proargtypes)::text, NULL::text, p.proargtypes::text
     FROM pg_proc p
     JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = $1
       AND p.prokind::text = ANY($5)
       AND NOT EXISTS (SELECT 1 FROM pg_depend d
                       WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e')
       AND ($2::text IS NULL OR p.proname ILIKE $2::text)
     ORDER BY p.proname, 2
     LIMIT $3 OFFSET $4";

/// PostgreSQL before 11: no `prokind`, aggregates are `proisagg`, window functions are listed.
const LEGACY_FUNCTIONS_SQL: &str = "SELECT p.proname::text, oidvectortypes(p.proargtypes)::text, NULL::text, p.proargtypes::text
     FROM pg_proc p
     JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = $1
       AND NOT p.proisagg
       AND NOT EXISTS (SELECT 1 FROM pg_depend d
                       WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e')
       AND ($2::text IS NULL OR p.proname ILIKE $2::text)
     ORDER BY p.proname, 2
     LIMIT $3 OFFSET $4";

const SEQUENCES_SQL: &str = "SELECT c.relname::text, NULL::text, NULL::text, NULL::text
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relkind = 'S'
       AND ($2::text IS NULL OR c.relname ILIKE $2::text)
     ORDER BY c.relname
     LIMIT $3 OFFSET $4";

const TRIGGERS_SQL: &str = "SELECT t.tgname::text, NULL::text, c.relname::text, NULL::text
     FROM pg_trigger t
     JOIN pg_class c ON c.oid = t.tgrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND NOT t.tgisinternal
       AND ($2::text IS NULL OR t.tgname ILIKE $2::text)
     ORDER BY c.relname, t.tgname
     LIMIT $3 OFFSET $4";

/// Enum, domain, range and stand-alone composite types (the row type of a table is not one).
const TYPES_SQL: &str = "SELECT t.typname::text, NULL::text, NULL::text, NULL::text
     FROM pg_type t
     JOIN pg_namespace n ON n.oid = t.typnamespace
     WHERE n.nspname = $1
       AND (t.typtype IN ('e', 'd', 'r')
            OR (t.typtype = 'c' AND EXISTS (SELECT 1 FROM pg_class c
                                            WHERE c.oid = t.typrelid AND c.relkind = 'c')))
       AND NOT EXISTS (SELECT 1 FROM pg_depend d
                       WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e')
       AND ($2::text IS NULL OR t.typname ILIKE $2::text)
     ORDER BY t.typname
     LIMIT $3 OFFSET $4";

/// `$1` schema, `$2` name, `$3` identity (the argument type OIDs; `NULL` for a reference
/// that has none), `$4` printed argument types (used only without an identity), `$5` the
/// `prokind` letters of the kind.
const FUNCTION_SOURCE_SQL: &str = "SELECT pg_get_functiondef(p.oid)
     FROM pg_proc p
     JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = $1 AND p.proname = $2
       AND (p.proargtypes::text = $3::text
            OR ($3::text IS NULL AND oidvectortypes(p.proargtypes) = $4::text))
       AND p.prokind::text = ANY($5)";

/// PostgreSQL before 11 (`$1`..`$4` as above, no kind).
const LEGACY_FUNCTION_SOURCE_SQL: &str = "SELECT pg_get_functiondef(p.oid)
     FROM pg_proc p
     JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = $1 AND p.proname = $2
       AND (p.proargtypes::text = $3::text
            OR ($3::text IS NULL AND oidvectortypes(p.proargtypes) = $4::text))
       AND NOT p.proisagg";

const SEQUENCE_SOURCE_SQL: &str = "SELECT format(
         'CREATE SEQUENCE %I.%I%s INCREMENT BY %s MINVALUE %s MAXVALUE %s START WITH %s CACHE %s %sCYCLE;',
         n.nspname, c.relname,
         CASE WHEN s.seqtypid <> 'bigint'::regtype THEN ' AS ' || format_type(s.seqtypid, NULL) ELSE '' END,
         s.seqincrement, s.seqmin, s.seqmax, s.seqstart, s.seqcache,
         CASE WHEN s.seqcycle THEN '' ELSE 'NO ' END)
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     JOIN pg_sequence s ON s.seqrelid = c.oid
     WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'S'";

/// `$1` schema, `$2` trigger name, `$3` table.
const TRIGGER_SOURCE_SQL: &str = "SELECT pg_get_triggerdef(t.oid, true)
     FROM pg_trigger t
     JOIN pg_class c ON c.oid = t.tgrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1 AND t.tgname = $2 AND c.relname = $3 AND NOT t.tgisinternal";

const TYPE_SOURCE_SQL: &str = "SELECT CASE t.typtype
         WHEN 'e' THEN format('CREATE TYPE %I.%I AS ENUM (%s);', n.nspname, t.typname,
             (SELECT coalesce(string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder), '')
              FROM pg_enum e WHERE e.enumtypid = t.oid))
         WHEN 'c' THEN format(E'CREATE TYPE %I.%I AS (\\n    %s\\n);', n.nspname, t.typname,
             (SELECT coalesce(string_agg(format('%I %s', a.attname, format_type(a.atttypid, a.atttypmod)),
                                         E',\\n    ' ORDER BY a.attnum), '')
              FROM pg_attribute a
              WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped))
         WHEN 'r' THEN (SELECT format(E'CREATE TYPE %I.%I AS RANGE (\\n    SUBTYPE = %s%s%s%s%s\\n);', n.nspname, t.typname,
                 format_type(r.rngsubtype, NULL),
                 CASE WHEN NOT oc.opcdefault
                      THEN E',\\n    SUBTYPE_OPCLASS = ' || quote_ident(ocn.nspname) || '.' || quote_ident(oc.opcname) ELSE '' END,
                 CASE WHEN r.rngcollation <> 0 AND r.rngcollation <> st.typcollation
                      THEN E',\\n    COLLATION = ' || quote_ident(cn.nspname) || '.' || quote_ident(co.collname) ELSE '' END,
                 CASE WHEN r.rngcanonical::oid <> 0 THEN E',\\n    CANONICAL = ' || r.rngcanonical::regproc::text ELSE '' END,
                 CASE WHEN r.rngsubdiff::oid <> 0 THEN E',\\n    SUBTYPE_DIFF = ' || r.rngsubdiff::regproc::text ELSE '' END)
             FROM pg_range r
             JOIN pg_type st ON st.oid = r.rngsubtype
             JOIN pg_opclass oc ON oc.oid = r.rngsubopc
             JOIN pg_namespace ocn ON ocn.oid = oc.opcnamespace
             LEFT JOIN pg_collation co ON co.oid = r.rngcollation
             LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace
             WHERE r.rngtypid = t.oid)
         ELSE format('CREATE DOMAIN %I.%I AS %s%s%s%s;', n.nspname, t.typname,
             format_type(t.typbasetype, t.typtypmod),
             CASE WHEN t.typdefault IS NOT NULL THEN ' DEFAULT ' || t.typdefault ELSE '' END,
             CASE WHEN t.typnotnull THEN ' NOT NULL' ELSE '' END,
             (SELECT coalesce(string_agg(format(E'\\n    CONSTRAINT %I %s', k.conname, pg_get_constraintdef(k.oid)),
                                         '' ORDER BY k.conname), '')
              FROM pg_constraint k WHERE k.contypid = t.oid AND k.contype = 'c'))
       END
     FROM pg_type t
     JOIN pg_namespace n ON n.oid = t.typnamespace
     WHERE n.nspname = $1 AND t.typname = $2 AND t.typtype IN ('e', 'c', 'd', 'r')";

const MATERIALIZED_VIEW_SOURCE_SQL: &str = "SELECT format(E'CREATE MATERIALIZED VIEW %I.%I AS\\n%s\\nWITH %sDATA;',
         n.nspname, c.relname,
         rtrim(pg_get_viewdef(c.oid, true), E';\\n '),
         CASE WHEN c.relispopulated THEN '' ELSE 'NO ' END)
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'm'";

/// What the connected server's catalogs have, from `server_version_num`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Catalog(u32);

impl Catalog {
    const PG_10: u32 = 100_000;
    const PG_11: u32 = 110_000;

    pub(crate) fn new(server_version_num: u32) -> Self {
        Self(server_version_num)
    }

    /// The version in the numeric form the host keeps (`server_version` of the connect reply, MySQL's:
    /// 16.2 is `160200`, shown as `16.2.0`). PostgreSQL 10 and later number a release
    /// `major * 10000 + minor` (16.2 is `160002`); before 10 the number already is
    /// `major * 10000 + minor * 100 + patch` (9.6.5 is `90605`). `None` for 0 and for a minor beyond 99.
    pub(crate) fn host_version(self) -> Option<u32> {
        match self.0 {
            0 => None,
            version if version < Self::PG_10 => Some(version),
            version => {
                let (major, minor) = (version / 10_000, version % 10_000);
                (minor < 100).then_some(major * 10_000 + minor * 100)
            }
        }
    }

    /// `prokind` and procedures (11+).
    fn has_prokind(self) -> bool {
        self.0 >= Self::PG_11
    }

    /// `pg_sequence` (10+): the only way to print a sequence's options.
    fn has_sequence_catalog(self) -> bool {
        self.0 >= Self::PG_10
    }

    /// The listing SQL of `kind`, `None` when the server has no such objects (or the driver
    /// cannot print them) and the page is empty.
    fn listing(self, kind: SchemaObjectKind) -> Option<&'static str> {
        match kind {
            SchemaObjectKind::Function if self.has_prokind() => Some(FUNCTIONS_SQL),
            SchemaObjectKind::Function => Some(LEGACY_FUNCTIONS_SQL),
            SchemaObjectKind::Procedure => self.has_prokind().then_some(FUNCTIONS_SQL),
            SchemaObjectKind::Sequence => self.has_sequence_catalog().then_some(SEQUENCES_SQL),
            SchemaObjectKind::Trigger => Some(TRIGGERS_SQL),
            SchemaObjectKind::Type => Some(TYPES_SQL),
            // Table-like (listed with the tables and views) or MySQL's.
            SchemaObjectKind::MaterializedView | SchemaObjectKind::Event => None,
        }
    }

    /// The source SQL of a function or procedure, `None` for a procedure before 11.
    fn function_source(self, kind: SchemaObjectKind) -> Option<&'static str> {
        match kind {
            SchemaObjectKind::Procedure if !self.has_prokind() => None,
            _ if self.has_prokind() => Some(FUNCTION_SOURCE_SQL),
            _ => Some(LEGACY_FUNCTION_SOURCE_SQL),
        }
    }
}

/// `server_version_num` of the server behind `client`.
async fn catalog(client: &tokio_postgres::Client) -> Result<Catalog, String> {
    let row = client
        .query_one("SHOW server_version_num", &[])
        .await
        .map_err(|error| v2_error_detail(&error))?;
    let text: String = row.try_get(0).map_err(|error| error.to_string())?;
    text.parse::<u32>()
        .map(Catalog::new)
        .map_err(|_| format!("unreadable server_version_num `{text}`"))
}

/// The version of the server behind the default database of `inner` (see [`Catalog::host_version`]);
/// `None` when it cannot be read: the connection works without it.
pub(crate) async fn host_server_version(inner: &PostgresConn) -> Option<u32> {
    let client = inner.v2_client("").await.ok()?;
    catalog(&client).await.ok()?.host_version()
}

/// The `prokind` letters a function-like kind lists: ordinary and window functions
/// are "functions"; aggregates are not listed at all.
fn prokinds(kind: SchemaObjectKind) -> &'static [&'static str] {
    match kind {
        SchemaObjectKind::Procedure => &["p"],
        _ => &["f", "w"],
    }
}

/// One page of the objects of `kind` in `schema`.
pub(crate) async fn list_page(
    inner: &PostgresConn,
    database: &str,
    schema: &str,
    kind: SchemaObjectKind,
    filter: Option<&str>,
    offset: i64,
    limit: i64,
) -> Result<Vec<NavigatorNode>, String> {
    let client = inner.v2_client(database).await?;
    let server = catalog(&client).await?;
    let Some(sql) = server.listing(kind) else {
        return Ok(Vec::new());
    };
    let pattern = filter
        .filter(|filter| !filter.is_empty())
        .map(pg_like_contains_pattern);
    let modern_functions = server.has_prokind()
        && matches!(kind, SchemaObjectKind::Function | SchemaObjectKind::Procedure);
    let rows = if modern_functions {
        client
            .query(sql, &[&schema, &pattern, &limit, &offset, &prokinds(kind)])
            .await
    } else {
        client.query(sql, &[&schema, &pattern, &limit, &offset]).await
    }
    .map_err(|error| v2_error_detail(&error))?;
    let mut nodes = Vec::with_capacity(rows.len());
    for row in &rows {
        let cell = |index: usize| {
            row.try_get::<_, Option<String>>(index)
                .map_err(|error| format!("unreadable {} row: {error}", kind.as_str()))
        };
        let (name, arguments, table, identity) =
            (cell(0)?.unwrap_or_default(), cell(1)?, cell(2)?, cell(3)?);
        nodes.push(node(database, schema, kind, name, arguments, table, identity));
    }
    Ok(nodes)
}

/// The navigator node of one object: its payload is the [`SchemaObjectRef`] that
/// `describe_object` takes back.
fn node(
    database: &str,
    schema: &str,
    kind: SchemaObjectKind,
    name: String,
    arguments: Option<String>,
    table: Option<String>,
    identity: Option<String>,
) -> NavigatorNode {
    let label = match (kind, &arguments, &table) {
        (_, Some(arguments), _) => format!("{name}({arguments})"),
        (SchemaObjectKind::Trigger, _, Some(table)) => format!("{name} on {table}"),
        _ => name.clone(),
    };
    let reference = SchemaObjectRef {
        database: Some(database.to_owned()),
        schema: Some(schema.to_owned()),
        kind,
        name,
        arguments,
        table,
        identity,
    };
    NavigatorNode {
        id: format!("{}:{database}:{schema}:{label}", kind.as_str()),
        kind: kind.as_str().into(),
        label,
        subtitle: None,
        icon: Some(kind.as_str().into()),
        has_children: false,
        payload: serde_json::to_value(&reference).unwrap_or(Value::Null),
        actions: vec!["view_source".into()],
    }
}

/// The source of the object `reference` names, `None` when it does not exist.
pub(crate) async fn source(
    inner: &PostgresConn,
    database: &str,
    schema: &str,
    reference: &SchemaObjectRef,
) -> Result<Option<String>, String> {
    let client = inner.v2_client(database).await?;
    let server = catalog(&client).await?;
    let name = reference.name.as_str();
    let row = match reference.kind {
        SchemaObjectKind::Function | SchemaObjectKind::Procedure => {
            let Some(sql) = server.function_source(reference.kind) else {
                return Ok(None);
            };
            let arguments = reference.arguments.as_deref().unwrap_or_default();
            let identity = reference.identity.as_deref();
            if server.has_prokind() {
                client
                    .query_opt(sql, &[&schema, &name, &identity, &arguments, &prokinds(reference.kind)])
                    .await
            } else {
                client.query_opt(sql, &[&schema, &name, &identity, &arguments]).await
            }
        }
        SchemaObjectKind::Trigger => {
            let table = reference.table.as_deref().unwrap_or_default();
            client.query_opt(TRIGGER_SOURCE_SQL, &[&schema, &name, &table]).await
        }
        SchemaObjectKind::Sequence if !server.has_sequence_catalog() => return Ok(None),
        SchemaObjectKind::Sequence => client.query_opt(SEQUENCE_SOURCE_SQL, &[&schema, &name]).await,
        SchemaObjectKind::Type => client.query_opt(TYPE_SOURCE_SQL, &[&schema, &name]).await,
        SchemaObjectKind::MaterializedView => {
            client.query_opt(MATERIALIZED_VIEW_SOURCE_SQL, &[&schema, &name]).await
        }
        SchemaObjectKind::Event => return Ok(None),
    }
    .map_err(|error| v2_error_detail(&error))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let text: Option<String> = row.try_get(0).map_err(|error| error.to_string())?;
    Ok(text.map(terminated))
}

/// `source` as a runnable statement: the server's printers leave the final `;` off.
fn terminated(mut source: String) -> String {
    let trimmed = source.trim_end().len();
    source.truncate(trimmed);
    if !source.ends_with(';') {
        source.push(';');
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_version_is_the_numeric_form_mysql_reports() {
        let host = |version_num| Catalog::new(version_num).host_version();
        // 10 and later: `major * 10000 + minor`, so 16.2 and 17.4.
        assert_eq!(host(160_002), Some(160_200));
        assert_eq!(host(170_004), Some(170_400));
        assert_eq!(host(140_010), Some(141_000));
        assert_eq!(host(100_000), Some(100_000), "10.0 has minor 0");
        // Before 10 the number already is major.minor.patch.
        assert_eq!(host(90_605), Some(90_605));
        // Nothing to report.
        assert_eq!(host(0), None);
        assert_eq!(host(160_100), None, "a minor of 100 or more does not fit");
    }

    #[test]
    fn overloads_get_their_argument_types_in_the_label_and_the_payload() {
        let add = node("app", "public", SchemaObjectKind::Function, "add".into(), Some("integer, integer".into()), None, Some("23 23".into()));
        let concat = node("app", "public", SchemaObjectKind::Function, "add".into(), Some("text, text".into()), None, Some("25 25".into()));
        assert_eq!(add.label, "add(integer, integer)");
        assert_ne!(add.id, concat.id, "two overloads are two nodes");
        let reference: SchemaObjectRef = serde_json::from_value(concat.payload).unwrap();
        assert_eq!(reference.name, "add");
        assert_eq!(reference.arguments.as_deref(), Some("text, text"));
        assert_eq!(reference.identity.as_deref(), Some("25 25"), "the lookup key is the type OIDs");
        // A function without arguments still says so.
        let none = node("app", "public", SchemaObjectKind::Function, "now2".into(), Some(String::new()), None, Some(String::new()));
        assert_eq!(none.label, "now2()");
    }

    #[test]
    fn a_trigger_names_its_table_and_a_sequence_only_itself() {
        let trigger = node("app", "public", SchemaObjectKind::Trigger, "audit".into(), None, Some("orders".into()), None);
        assert_eq!(trigger.label, "audit on orders");
        let reference: SchemaObjectRef = serde_json::from_value(trigger.payload).unwrap();
        assert_eq!((reference.name.as_str(), reference.table.as_deref()), ("audit", Some("orders")));
        let sequence = node("app", "public", SchemaObjectKind::Sequence, "orders_id_seq".into(), None, None, None);
        assert_eq!(sequence.label, "orders_id_seq");
        assert!(!sequence.has_children);
    }

    #[test]
    fn only_functions_and_window_functions_are_functions() {
        assert_eq!(prokinds(SchemaObjectKind::Function), ["f", "w"]);
        assert_eq!(prokinds(SchemaObjectKind::Procedure), ["p"]);
    }

    #[test]
    fn the_catalog_queries_follow_the_server_version() {
        use SchemaObjectKind::*;
        let (modern, ten, nine) = (Catalog::new(160_004), Catalog::new(100_022), Catalog::new(90_624));
        // 11+: `prokind`, procedures, `pg_sequence`.
        assert_eq!(modern.listing(Function), Some(FUNCTIONS_SQL));
        assert_eq!(modern.listing(Procedure), Some(FUNCTIONS_SQL));
        assert!(modern.listing(Sequence).is_some());
        assert_eq!(modern.function_source(Function), Some(FUNCTION_SOURCE_SQL));
        // 10: `proisagg`, no procedures, still `pg_sequence`.
        assert_eq!(ten.listing(Function), Some(LEGACY_FUNCTIONS_SQL));
        assert!(LEGACY_FUNCTIONS_SQL.contains("NOT p.proisagg") && !LEGACY_FUNCTIONS_SQL.contains("prokind"));
        assert_eq!(ten.listing(Procedure), None);
        assert_eq!(ten.function_source(Procedure), None);
        assert_eq!(ten.function_source(Function), Some(LEGACY_FUNCTION_SOURCE_SQL));
        assert!(!LEGACY_FUNCTION_SOURCE_SQL.contains("prokind"));
        assert!(ten.listing(Sequence).is_some());
        // 9.x: no sequence catalog either.
        assert_eq!(nine.listing(Sequence), None);
        assert!(nine.listing(Trigger).is_some() && nine.listing(Type).is_some());
        // The first server with each: the boundaries are inclusive.
        assert!(Catalog::new(110_000).listing(Procedure).is_some());
        assert_eq!(Catalog::new(109_999).listing(Procedure), None);
        assert!(Catalog::new(100_000).listing(Sequence).is_some());
        assert_eq!(Catalog::new(99_999).listing(Sequence), None);
        // Every listing returns the four columns the node is built from, whatever the version.
        for sql in [FUNCTIONS_SQL, LEGACY_FUNCTIONS_SQL, SEQUENCES_SQL, TRIGGERS_SQL, TYPES_SQL] {
            assert!(sql.starts_with("SELECT") && sql.matches("::text").count() >= 4, "{sql}");
        }
        // Range types are listed and printed.
        assert!(TYPES_SQL.contains("'r'") && TYPE_SOURCE_SQL.contains("AS RANGE"));
    }

    #[test]
    fn a_source_ends_with_one_semicolon() {
        assert_eq!(terminated("CREATE TRIGGER t ...".into()), "CREATE TRIGGER t ...;");
        assert_eq!(terminated("CREATE SEQUENCE s;\n".into()), "CREATE SEQUENCE s;");
        assert_eq!(terminated("$function$\n".into()), "$function$;");
    }
}
