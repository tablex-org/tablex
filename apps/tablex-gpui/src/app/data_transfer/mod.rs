//! Data transfer (R4b): copy the rows of one or more tables, or of a query, into tables of any SQL connection or
//! into files (CSV, JSON, SQL INSERT, XLSX), streamed and batched, on sessions of their own.
//!
//! * `typemap`: the type of a column in the words of another dialect (for the tables a transfer creates);
//! * `reader`: the pages of a query over a session, and the rows of those pages for the load of R4a;
//! * `sink`: the file formats, written a row at a time;
//! * `job`: the jobs of a transfer one after the other (generic over the feed of pages and the writer of batches).

pub(in crate::app) mod job;
mod model;
pub(in crate::app) mod order;
mod panels;
mod problem;
pub(in crate::app) mod reader;
pub(in crate::app) mod sink;
mod tab;
pub(in crate::app) mod typemap;
pub(in crate::app) mod view;

pub(in crate::app) use tab::{TableNames, TransferCreate, TransferViews};
pub(in crate::app) use view::{Namespace, TargetConnection};
#[cfg(test)]
pub(in crate::app) use view::TransferInit;
