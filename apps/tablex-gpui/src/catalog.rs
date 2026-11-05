//! Catalog model of the Properties, editing and diagram views. These are the
//! SQL driver contract's types (`engine_driver_api::sql`, what `describe_object`
//! and the navigator's column nodes carry), under the names the views use, so
//! a driver's answer is shown without a conversion.

pub use engine_driver_api::sql::{
    ColumnAttributes as CatalogColumnAttributes, ConstraintKind as CatalogConstraintKind,
    TableColumn as CatalogColumn, TableConstraint as CatalogConstraint,
    TableForeignKey as CatalogForeignKey, TableIndex as CatalogIndex, TableProperties,
    TableTrigger as CatalogTrigger,
};
