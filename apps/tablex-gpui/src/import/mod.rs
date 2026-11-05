//! Reading the file of a data import (R4a): streamed parsers for delimited text and JSON, the
//! encodings they can be in, and what a sample of the rows says about their types. Nothing here
//! knows about the app, a connection or a table: [`crate::app::data_import`] maps the rows onto one.
//!
//! Every reader works one row at a time over a buffered stream, so the memory an import holds is
//! bounded by the largest row, never by the file.

pub mod delimited;
pub mod encoding;
pub mod infer;
pub mod json;
pub mod source;
