//! Chinook-like SQLite catalog of the `table-diagram` UI scenario: real
//! foreign keys (including the Employee self-reference and PlaylistTrack's
//! composite key) answered as `TableProperties`, the fixture stand-in for
//! the driver's `describe_object` on the diagram route.

use crate::catalog::{
    CatalogColumn, CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex, TableProperties,
};

/// The database and table the scenario opens.
pub const DATABASE: &str = "main";
pub const FOCUS_TABLE: &str = "Track";

/// `(name, type, primary key, nullable)`.
type ColumnSpec = (&'static str, &'static str, bool, bool);
/// `(columns, referenced table, referenced columns)`.
type ForeignKeySpec = (&'static [&'static str], &'static str, &'static [&'static str]);

struct TableSpec {
    name: &'static str,
    columns: &'static [ColumnSpec],
    foreign_keys: &'static [ForeignKeySpec],
}

const fn id(name: &'static str) -> ColumnSpec {
    (name, "INTEGER", true, false)
}

const fn int(name: &'static str, nullable: bool) -> ColumnSpec {
    (name, "INTEGER", false, nullable)
}

const fn text(name: &'static str, nullable: bool) -> ColumnSpec {
    (name, "NVARCHAR(120)", false, nullable)
}

const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "Artist",
        columns: &[id("ArtistId"), text("Name", true)],
        foreign_keys: &[],
    },
    TableSpec {
        name: "Album",
        columns: &[id("AlbumId"), text("Title", false), int("ArtistId", false)],
        foreign_keys: &[(&["ArtistId"], "Artist", &["ArtistId"])],
    },
    TableSpec {
        name: "Genre",
        columns: &[id("GenreId"), text("Name", true)],
        foreign_keys: &[],
    },
    TableSpec {
        name: "MediaType",
        columns: &[id("MediaTypeId"), text("Name", true)],
        foreign_keys: &[],
    },
    TableSpec {
        name: "Track",
        columns: &[
            id("TrackId"),
            text("Name", false),
            int("AlbumId", true),
            int("MediaTypeId", false),
            int("GenreId", true),
            text("Composer", true),
            int("Milliseconds", false),
            int("Bytes", true),
            ("UnitPrice", "NUMERIC(10,2)", false, false),
        ],
        foreign_keys: &[
            (&["AlbumId"], "Album", &["AlbumId"]),
            (&["GenreId"], "Genre", &["GenreId"]),
            (&["MediaTypeId"], "MediaType", &["MediaTypeId"]),
        ],
    },
    TableSpec {
        name: "Employee",
        columns: &[
            id("EmployeeId"),
            text("LastName", false),
            text("FirstName", false),
            text("Title", true),
            int("ReportsTo", true),
            ("BirthDate", "DATETIME", false, true),
        ],
        foreign_keys: &[(&["ReportsTo"], "Employee", &["EmployeeId"])],
    },
    TableSpec {
        name: "Customer",
        columns: &[
            id("CustomerId"),
            text("FirstName", false),
            text("LastName", false),
            text("Email", false),
            int("SupportRepId", true),
        ],
        foreign_keys: &[(&["SupportRepId"], "Employee", &["EmployeeId"])],
    },
    TableSpec {
        name: "Invoice",
        columns: &[
            id("InvoiceId"),
            int("CustomerId", false),
            ("InvoiceDate", "DATETIME", false, false),
            ("Total", "NUMERIC(10,2)", false, false),
        ],
        foreign_keys: &[(&["CustomerId"], "Customer", &["CustomerId"])],
    },
    TableSpec {
        name: "InvoiceLine",
        columns: &[
            id("InvoiceLineId"),
            int("InvoiceId", false),
            int("TrackId", false),
            ("UnitPrice", "NUMERIC(10,2)", false, false),
            int("Quantity", false),
        ],
        foreign_keys: &[
            (&["InvoiceId"], "Invoice", &["InvoiceId"]),
            (&["TrackId"], "Track", &["TrackId"]),
        ],
    },
    TableSpec {
        name: "Playlist",
        columns: &[id("PlaylistId"), text("Name", true)],
        foreign_keys: &[],
    },
    TableSpec {
        name: "PlaylistTrack",
        columns: &[
            ("PlaylistId", "INTEGER", true, false),
            ("TrackId", "INTEGER", true, false),
        ],
        foreign_keys: &[
            (&["PlaylistId"], "Playlist", &["PlaylistId"]),
            (&["TrackId"], "Track", &["TrackId"]),
        ],
    },
];

/// `(table, column, comment)`: the fixture's column comments (what PostgreSQL,
/// MySQL and ClickHouse report; the Inspector shows them).
const COMMENTS: &[(&str, &str, &str)] = &[
    ("Track", "Composer", "Credited songwriters, comma separated"),
    ("Track", "UnitPrice", "List price in USD"),
    ("Invoice", "Total", "Sum of the invoice lines"),
    ("Customer", "SupportRepId", "Employee who looks after the account"),
];

/// Every table of the catalog (the whole-database diagram's table list).
pub fn table_names() -> Vec<String> {
    TABLES.iter().map(|spec| spec.name.to_string()).collect()
}

/// Capture-only (`TABLEX_SEED_DIAGRAM=legacy,...`): a schema whose foreign keys
/// were never declared, `(owner, columns)`. The columns keep their
/// `<table>Id` names, which is what the Suggestions panel infers from.
const LEGACY_UNDECLARED: &[(&str, &str)] = &[("Track", "AlbumId"), ("Track", "GenreId"), ("InvoiceLine", "InvoiceId")];

fn legacy() -> bool {
    std::env::var("TABLEX_SEED_DIAGRAM").is_ok_and(|spec| spec.split(',').any(|token| token == "legacy"))
}

/// Whether the catalog declares `owner`'s foreign key on `columns`.
fn declared(owner: &str, columns: &[&str], legacy: bool) -> bool {
    !(legacy && LEGACY_UNDECLARED.iter().any(|&(table, column)| table == owner && columns == [column]))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn foreign_key(owner: &TableSpec, (columns, target, target_columns): &ForeignKeySpec) -> CatalogForeignKey {
    CatalogForeignKey {
        name: format!("FK_{}{}", owner.name, columns.join("")),
        table: owner.name.into(),
        columns: strings(columns),
        referenced_table: (*target).into(),
        referenced_columns: strings(target_columns),
        on_update: Some("NO ACTION".into()),
        on_delete: Some("NO ACTION".into()),
        definition: None,
    }
}

/// What `describe_object` would answer for `table`; unknown tables
/// fail like a missing object does.
pub fn table_properties(table: &str) -> Result<TableProperties, String> {
    let legacy = legacy();
    let spec = TABLES
        .iter()
        .find(|spec| spec.name == table)
        .ok_or_else(|| format!("no such table: {table}"))?;
    let primary: Vec<&str> = spec
        .columns
        .iter()
        .filter(|column| column.2)
        .map(|column| column.0)
        .collect();
    Ok(TableProperties {
        columns: spec
            .columns
            .iter()
            .map(|&(name, data_type, primary_key, nullable)| CatalogColumn {
                name: name.into(),
                data_type: data_type.into(),
                nullable,
                primary_key,
                is_generated: None,
                default_expression: None,
                auto_increment: None,
                comment: COMMENTS
                    .iter()
                    .find(|(owner, column, _)| *owner == spec.name && *column == name)
                    .map(|(_, _, comment)| comment.to_string()),
                attributes: None,
            })
            .collect(),
        keys: Some(vec![CatalogConstraint {
            name: format!("PK_{}", spec.name),
            kind: CatalogConstraintKind::PrimaryKey,
            columns: strings(&primary),
            definition: None,
        }]),
        foreign_keys: Some(
            spec.foreign_keys
                .iter()
                .filter(|key| declared(spec.name, key.0, legacy))
                .map(|key| foreign_key(spec, key))
                .collect(),
        ),
        // Like Chinook: one index per foreign key, named `IFK_<table><column>`.
        indexes: Some(
            spec.foreign_keys
                .iter()
                .map(|key| CatalogIndex {
                    name: format!("IFK_{}{}", spec.name, key.0.join("")),
                    columns: strings(key.0),
                    unique: false,
                    primary: false,
                    method: Some("btree".into()),
                    definition: None,
                })
                .collect(),
        ),
        references: Some(
            TABLES
                .iter()
                .flat_map(|owner| {
                    owner
                        .foreign_keys
                        .iter()
                        .filter(|key| key.1 == spec.name && declared(owner.name, key.0, legacy))
                        .map(|key| foreign_key(owner, key))
                })
                .collect(),
        ),
        triggers: Some(Vec::new()),
        ddl: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_has_three_parents_and_two_children() {
        let track = table_properties(FOCUS_TABLE).unwrap();
        let parents: Vec<_> = track
            .foreign_keys
            .unwrap()
            .into_iter()
            .map(|key| key.referenced_table)
            .collect();
        assert_eq!(parents, ["Album", "Genre", "MediaType"]);
        let children: Vec<_> = track
            .references
            .unwrap()
            .into_iter()
            .map(|key| key.table)
            .collect();
        assert_eq!(children, ["InvoiceLine", "PlaylistTrack"]);
    }

    #[test]
    fn employee_references_itself_and_unknown_tables_fail() {
        let employee = table_properties("Employee").unwrap();
        assert_eq!(employee.foreign_keys.unwrap()[0].referenced_table, "Employee");
        assert_eq!(employee.references.unwrap().len(), 2);
        assert!(table_properties("Nope").is_err());
    }
}
