//! Index and foreign key specs as the structure editors build them (the diagram's Inspector and
//! the Properties edit mode): the checks of a new spec, the default names, and the choices of the
//! ON DELETE / ON UPDATE selects. Pure, no entities.

use gpui::SharedString;

use crate::sql_ddl::{FkAction, ForeignKeySpec, IndexSpec};

/// The first choice of an ON DELETE / ON UPDATE select: no clause (the database's default).
pub(in crate::app) const FK_ACTION_NONE: &str = "No action (default)";

/// The rows of an ON DELETE / ON UPDATE select: none, then every [`FkAction`].
pub(in crate::app) fn fk_action_labels() -> Vec<SharedString> {
    std::iter::once(FK_ACTION_NONE).chain(FkAction::ALL.iter().map(|action| action.sql())).map(SharedString::from).collect()
}

/// The action behind row `row` of [`fk_action_labels`] (row 0: none).
pub(in crate::app) fn fk_action_at(row: usize) -> Option<FkAction> {
    row.checked_sub(1).and_then(|at| FkAction::ALL.get(at).copied())
}

/// A new index, after the checks the editors share. `taken`: the table already has an index of that name.
pub(in crate::app) fn index_spec(name: &str, columns: Vec<String>, unique: bool, taken: impl Fn(&str) -> bool) -> Result<IndexSpec, String> {
    let name = name.trim();
    if name.is_empty() || columns.is_empty() {
        return Err("An index needs a name and at least one column.".into());
    }
    if taken(name) {
        return Err(format!("There is an index named {name} already."));
    }
    Ok(IndexSpec { name: name.to_owned(), columns, unique })
}

/// A new foreign key, after the checks the editors share.
pub(in crate::app) fn check_foreign_key(spec: &ForeignKeySpec) -> Result<(), String> {
    if spec.name.trim().is_empty() || spec.columns.is_empty() || spec.columns.len() != spec.ref_columns.len() {
        return Err("A foreign key needs a name and as many referenced columns as local ones.".into());
    }
    Ok(())
}

/// `prefix_table_col1_col2`, numbered (`_2`, `_3`...) until `taken` no longer knows it.
fn unique_name(prefix: &str, table: &str, columns: &[String], taken: impl Fn(&str) -> bool) -> String {
    let base = std::iter::once(prefix).chain(std::iter::once(table)).chain(columns.iter().map(String::as_str)).collect::<Vec<_>>().join("_");
    std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}_{n}"))).find(|name| !taken(name)).unwrap_or(base)
}

/// The name a new index gets until the user types one: `idx_<table>_<columns>`.
pub(in crate::app) fn default_index_name(table: &str, columns: &[String], taken: impl Fn(&str) -> bool) -> String {
    unique_name("idx", table, columns, taken)
}

/// The name a new foreign key gets until the user types one: `fk_<table>_<columns>`.
pub(in crate::app) fn default_foreign_key_name(table: &str, columns: &[String], taken: impl Fn(&str) -> bool) -> String {
    unique_name("fk", table, columns, taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_action_rows_start_with_none_and_map_to_the_actions() {
        let labels = fk_action_labels();
        assert_eq!(labels.len(), FkAction::ALL.len() + 1);
        assert_eq!(labels[0], FK_ACTION_NONE);
        assert_eq!(fk_action_at(0), None);
        for (at, action) in FkAction::ALL.iter().enumerate() {
            assert_eq!(labels[at + 1], action.sql());
            assert_eq!(fk_action_at(at + 1), Some(*action));
        }
        assert_eq!(fk_action_at(99), None);
    }

    #[test]
    fn a_new_index_needs_a_free_name_and_columns() {
        let taken = |name: &str| name == "idx_t_a";
        assert_eq!(index_spec(" ", cols(&["a"]), false, taken).unwrap_err(), "An index needs a name and at least one column.");
        assert_eq!(index_spec("i", cols(&[]), false, taken).unwrap_err(), "An index needs a name and at least one column.");
        assert_eq!(index_spec("idx_t_a", cols(&["a"]), false, taken).unwrap_err(), "There is an index named idx_t_a already.");
        let spec = index_spec(" i ", cols(&["a", "b"]), true, taken).unwrap();
        assert_eq!((spec.name.as_str(), spec.columns, spec.unique), ("i", cols(&["a", "b"]), true));
    }

    #[test]
    fn a_foreign_key_needs_as_many_referenced_columns_as_local_ones() {
        let spec = ForeignKeySpec { name: "fk".into(), columns: cols(&["a"]), ref_columns: cols(&["id"]), ..ForeignKeySpec::default() };
        assert!(check_foreign_key(&spec).is_ok());
        assert!(check_foreign_key(&ForeignKeySpec { ref_columns: cols(&[]), ..spec.clone() }).is_err());
        assert!(check_foreign_key(&ForeignKeySpec { name: String::new(), ..spec.clone() }).is_err());
        assert!(check_foreign_key(&ForeignKeySpec { columns: cols(&[]), ref_columns: cols(&[]), ..spec }).is_err());
    }

    #[test]
    fn default_names_are_numbered_until_they_are_free() {
        let none = |_: &str| false;
        assert_eq!(default_index_name("orders", &cols(&["a", "b"]), none), "idx_orders_a_b");
        assert_eq!(default_foreign_key_name("orders", &cols(&["customer_id"]), none), "fk_orders_customer_id");
        assert_eq!(default_index_name("orders", &[], none), "idx_orders");
        let used = |name: &str| ["idx_orders_a", "idx_orders_a_2"].contains(&name);
        assert_eq!(default_index_name("orders", &cols(&["a"]), used), "idx_orders_a_3");
    }
}
