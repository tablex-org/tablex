//! Shared ABI v2 glue for TableX driver dylibs.
//!
//! A driver implements [`DriverBackend`] with plain Rust types and invokes
//! [`export_driver!`] once; the kit owns everything else that touches raw
//! pointers: the connection and page registries, page id allocation, zero-copy
//! Arrow page leases, cell-batch rendering, `inspect_row`, release, the
//! operation/cancel state machine, C string allocation and the JSON success and
//! error envelopes. Every exported entry point runs under `catch_unwind`, so a
//! panicking backend returns an error instead of aborting the host process.
//!
//! Wire types (`QueryRequest`, `QueryPage`, ...) come from `engine-driver-api`,
//! re-exported as [`api`].

#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
mod cell_text;
mod driver;
mod ffi;
mod macros;
mod operation;
mod session;
pub mod sql;
pub mod sql_guard;

pub use backend::{
    batch_with_row_count, default_inspect_row, empty_batch, page_columns, BackendError,
    ConnectRequest, DriverBackend, MutationOutcome, Page, QueryOutput,
};
pub use cell_text::{default_cell_text, write_hex_blob_literal, CellFormatter};
pub use driver::{Driver, ExportedSymbols};
pub use engine_driver_api as api;
pub use operation::{ActiveOperation, OperationGuard};
