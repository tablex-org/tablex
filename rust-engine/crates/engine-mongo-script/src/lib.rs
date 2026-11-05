//! MongoDB shell-script parsing and the pipeline model it compiles to.
//!
//! Client-free (BSON and tree-sitter only, no `mongodb` driver): the Swift-side
//! `dbengine` executes the plans, the GPUI app only inspects read scripts
//! (`db.coll.find(...)`) before handing them to the MongoDB driver.

pub mod pipeline;
pub mod script_parser;
pub mod shell_value;

pub use script_parser::{inspect_read_script, MongoReadScript};
