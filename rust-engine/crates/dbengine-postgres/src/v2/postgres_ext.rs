// Included inside `db::postgres` by `lib.rs` (ABI v2 only): `PostgresConn`
// keeps its per-database client pool and a few helpers private, and the shared
// source belongs to the native engine, so the v2 driver adds its accessors from
// within the module instead of changing that file.

/// One page of tables/views/materialized views, with an optional
/// case-insensitive literal name filter pushed into SQL (both `LIMIT` and
/// `OFFSET` are bound) and an optional `relkind` (`m`: only materialized views). Same predicate and order as [`POSTGRES_SQL_OBJECTS_SQL`].
const POSTGRES_V2_OBJECTS_PAGE_SQL: &str = "SELECT c.relname::text, c.relkind::text,
            obj_description(c.oid, 'pg_class')::text
     FROM pg_class c
     JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = $1
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND has_table_privilege(c.oid, 'SELECT')
       AND ($2::text IS NULL OR c.relname ILIKE $2::text)
       AND ($5::text IS NULL OR c.relkind::text = $5::text)
     ORDER BY c.relkind, c.relname
     LIMIT $3 OFFSET $4";

/// One page of the columns of every table-like object of a schema, ordered by table name (binary
/// collation) then `attnum`, continuing after the `($2, $3)` column when `$2` is not NULL. The
/// column expressions are those of [`POSTGRES_TABLE_COLUMNS_SQL`]; the objects are those
/// [`POSTGRES_V2_OBJECTS_PAGE_SQL`] lists.
const POSTGRES_V2_SCHEMA_COLUMNS_SQL: &str = "SELECT c.relname::text, a.attnum::int4,
            a.attname::text,
            pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
            NOT a.attnotnull AS nullable,
            pg_get_expr(ad.adbin, ad.adrelid) AS default_value,
            a.attgenerated <> '' AS is_generated,
            a.attidentity <> '' OR COALESCE(pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%', false) AS auto_increment,
            EXISTS (
                SELECT 1 FROM pg_index idx
                WHERE idx.indrelid = c.oid AND idx.indisprimary AND a.attnum = ANY(idx.indkey)
            ) AS is_primary_key
     FROM pg_attribute a
     JOIN pg_class c ON c.oid = a.attrelid
     JOIN pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
     WHERE n.nspname = $1
       AND c.relkind IN ('r', 'p', 'v', 'm')
       AND has_table_privilege(c.oid, 'SELECT')
       AND a.attnum > 0
       AND NOT a.attisdropped
       AND ($2::text IS NULL
            OR c.relname::text COLLATE \"C\" > $2::text
            OR (c.relname::text COLLATE \"C\" = $2::text AND a.attnum::int4 > $3::int4))
     ORDER BY c.relname::text COLLATE \"C\", a.attnum
     LIMIT $4";

/// The session settings every connection of the v2 driver gets, sent as `SET`s
/// right after the connection opens and before anything else runs on it (not as
/// startup `options`: poolers such as PgBouncer refuse that startup parameter; a
/// URI's own `options` is left untouched). The driver reads server text under
/// them: `extra_float_digits = 3` makes `real`/`double precision` text exact (it
/// is the default only from PostgreSQL 12), `DateStyle = ISO` and
/// `IntervalStyle = postgres` make dates, timestamps and intervals one parseable
/// form whatever the role or database default is (`DateStyle` keeps the server's
/// field order), `client_encoding = UTF8` is what tokio-postgres starts with.
pub(crate) const V2_SESSION_SETUP: &str = "SET extra_float_digits = 3; SET DateStyle = 'ISO'; SET IntervalStyle = 'postgres'; SET client_encoding = 'UTF8'";

/// The same statements one by one: they put the settings back after user SQL
/// changed them (see the query layer), and a script can carry them between its own
/// statements.
pub(crate) const V2_SESSION_RESTORE: [&str; 4] = [
    "SET extra_float_digits = 3",
    "SET DateStyle = 'ISO'",
    "SET IntervalStyle = 'postgres'",
    "SET client_encoding = 'UTF8'",
];

impl PostgresConn {
    /// Opens the user session with typed connect errors (`connect.tls_*` and
    /// friends); [`PostgresConn::connect`] is the same minus the error codes.
    pub(crate) async fn v2_connect(uri: &str) -> Result<Self, engine_driver_kit::BackendError> {
        let (config, mut transport) = Transport::prepare(uri)?;
        // The session pins the address of a host name (see `Transport::connect_session`).
        let client = transport
            .connect_session(&config)
            .await
            .map_err(|error| transport.connect_error(&error))?;
        Self::v2_session_setup_on(&client)
            .await
            .map_err(engine_driver_kit::BackendError::new)?;
        let default_database = current_database_for_client(&client)
            .await
            .map_err(engine_driver_kit::BackendError::new)?;
        // From here on `info()` is this session's handshake, whatever connects later.
        transport.seal_session_info();
        Ok(Self {
            client: Arc::new(client),
            config,
            transport,
            default_database,
            database_clients: Mutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            connect_setup: Some(V2_SESSION_SETUP),
        })
    }

    /// Sends [`V2_SESSION_SETUP`] on a client that just connected (see
    /// `run_connect_setup`: per-database clients get it there).
    async fn v2_session_setup_on(client: &Client) -> Result<(), String> {
        match client.batch_execute(V2_SESSION_SETUP).await {
            Err(error) if error.as_db_error().is_none() => Err(postgres_error_detail(&error)),
            _ => Ok(()),
        }
    }

    /// A second connection (the metadata connection) with this connection's
    /// configuration and **transport**: it shares the TLS pin and the session's
    /// TLS info instead of starting a fresh, unpinned `Transport`.
    pub(crate) async fn v2_reconnect(&self) -> Result<Self, String> {
        let client = self
            .transport
            .connect(&self.config)
            .await
            .map_err(|error| {
                format!("Postgres connect failed: {}", postgres_error_detail(&error))
            })?;
        Self::v2_session_setup_on(&client).await?;
        let default_database = current_database_for_client(&client).await?;
        Ok(Self {
            client: Arc::new(client),
            config: self.config.clone(),
            transport: self.transport.clone(),
            default_database,
            database_clients: Mutex::new(HashMap::new()),
            mutation_lock: Mutex::new(()),
            connect_setup: Some(V2_SESSION_SETUP),
        })
    }

    /// The transport every client of this connection uses (the cancel side
    /// channel reuses it, so a cancel is encrypted exactly like the session).
    pub(crate) fn v2_transport(&self) -> Transport {
        self.transport.clone()
    }

    /// Negotiated TLS version/cipher and how far the server was verified;
    /// `None` when the session is not encrypted.
    pub(crate) fn v2_tls_info(&self) -> Option<super::pg_transport::TlsInfo> {
        self.transport.info()
    }

    /// The client for `db` (`""` = the connection's default database),
    /// connecting to that database on first use.
    pub(crate) async fn v2_client(&self, db: &str) -> Result<Arc<Client>, String> {
        self.client_for_database(db).await
    }

    /// One page (at most `limit` rows) of [`POSTGRES_V2_SCHEMA_COLUMNS_SQL`]: `(table, attnum,
    /// column)`, after `after` (a table and an `attnum`) when given.
    pub(crate) async fn v2_list_schema_columns_page(
        &self,
        db: &str,
        schema: &str,
        after: Option<(&str, i32)>,
        limit: i64,
    ) -> Result<Vec<(String, i32, SqlColumn)>, String> {
        let (table, ordinal) = match after {
            Some((table, ordinal)) => (Some(table), ordinal),
            None => (None, 0),
        };
        let rows = self
            .client_for_database(db)
            .await?
            .query(POSTGRES_V2_SCHEMA_COLUMNS_SQL, &[&schema, &table, &ordinal, &limit])
            .await
            .map_err(|error| postgres_error_detail(&error))?;
        rows.iter()
            .map(|row| {
                Ok((
                    catalog_cell(row, 0)?,
                    catalog_cell(row, 1)?,
                    SqlColumn {
                        name: catalog_cell(row, 2)?,
                        data_type: catalog_cell(row, 3)?,
                        nullable: catalog_cell(row, 4)?,
                        default_expression: catalog_cell(row, 5)?,
                        is_generated: Some(catalog_cell(row, 6)?),
                        auto_increment: Some(catalog_cell(row, 7)?),
                        primary_key: catalog_cell(row, 8)?,
                        comment: None,
                    },
                ))
            })
            .collect()
    }

    pub(crate) async fn v2_list_objects_page(
        &self,
        db: &str,
        schema: &str,
        filter: Option<&str>,
        relkind: Option<&str>,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<(SqlObject, Option<String>)>, String> {
        let pattern = filter
            .filter(|filter| !filter.is_empty())
            .map(pg_like_contains_pattern);
        let rows = self
            .client_for_database(db)
            .await?
            .query(
                POSTGRES_V2_OBJECTS_PAGE_SQL,
                &[&schema, &pattern, &limit, &offset, &relkind],
            )
            .await
            .map_err(|error| postgres_error_detail(&error))?;
        // An object whose name cannot be read is an error naming the problem, never a
        // panic and never a table silently missing from the list.
        let mut objects = Vec::with_capacity(rows.len());
        for row in &rows {
            let name: String = catalog_cell(row, 0)?;
            let relkind: String = catalog_cell(row, 1)?;
            // The comment of `COMMENT ON TABLE`, listed with the table: no query per table.
            let comment: Option<String> = row.try_get(2).map_err(|error| postgres_error_detail(&error))?;
            objects.extend(
                SqlObjectKind::from_postgres_relkind(&relkind)
                    .map(|kind| (SqlObject { name, kind }, comment.filter(|text| !text.is_empty()))),
            );
        }
        Ok(objects)
    }
}

/// Whether a `table_properties` error means "no such table or view".
pub(crate) fn v2_is_not_found(message: &str) -> bool {
    message.starts_with(TABLE_NOT_FOUND_PREFIX)
}

/// `CREATE DATABASE` for a validated name (the statement the native engine runs).
pub(crate) fn v2_create_database_sql(name: &str) -> Result<String, String> {
    create_database_sql(name)
}

/// Server message with severity, detail and hint (`tokio_postgres::Error`'s own
/// `Display` prints just "db error" for server-side failures).
pub(crate) fn v2_error_detail(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => {
            let mut message = db.to_string();
            if let Some(detail) = db.detail() {
                message.push_str(&format!(" | detail: {detail}"));
            }
            if let Some(hint) = db.hint() {
                message.push_str(&format!(" | hint: {hint}"));
            }
            message
        }
        None => error.to_string(),
    }
}
