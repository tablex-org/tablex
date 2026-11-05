//! ER diagram presentation of a SQL table: the focus table plus every table
//! related to it by a foreign key (depth 1), shown in a "Diagram" tab.
//!
//! Split so three workers can build it in parallel against fixed types:
//! - `model`  — pure graph built from `TableProperties` (owner: DG-M)
//! - `layout` — pure auto-layout, orthogonal routing, viewport math (owner: DG-M)
//! - `data`   — background fetch + cache of related tables' properties (owner: DG-D)
//! - `view`   — GPUI rendering, interactions, toolbar, rail hook (owner: DG)
//! - `annotations`, `infer`, `history` — D2: what the user adds (logical
//!   relationships, notes, colors, groups), relationship suggestions, undo
//!
//! The types below are the contract between them; change them only through
//! the root.

// Parts are filled in by parallel workers; remove once all are wired.
#![allow(dead_code)]

pub(in crate::app) mod annotations;
pub(in crate::app) mod apply;
pub(in crate::app) mod data;
pub(in crate::app) mod export;
pub(in crate::app) mod history;
pub(in crate::app) mod infer;
pub(in crate::app) mod join_sql;
pub(in crate::app) mod layout;
pub(in crate::app) mod model;
pub(in crate::app) mod notation;
pub(in crate::app) mod overlay;
pub(in crate::app) mod store;
pub(in crate::app) mod view;

#[cfg(test)]
mod tests;
