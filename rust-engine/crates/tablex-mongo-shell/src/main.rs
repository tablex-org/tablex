//! `tablex-mongo-shell`: runs one mongosh-style script per process. The host
//! spawns it, speaks the framed protocol on stdin / stdout (see
//! `engine_driver_api::mongo_shell`) and kills it to stop a run.

use std::io::{BufReader, BufWriter};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut reader = BufReader::new(std::io::stdin());
    let mut writer = BufWriter::new(std::io::stdout());
    match tablex_mongo_shell::serve(&mut reader, &mut writer) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tablex-mongo-shell: {error}");
            ExitCode::from(2)
        }
    }
}
