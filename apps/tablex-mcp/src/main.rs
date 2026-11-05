//! `tablex-mcp`: see the library docs. This file is the process shell: arguments, the panic hook,
//! the runtime, and the exit code.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use tablex_mcp::cli::{self, Command};
use tablex_mcp::server::{McpServer, MAX_CONCURRENT_CALLS};
use tablex_mcp::setup;
use tablex_mcp::transport::Signals;

fn main() -> ExitCode {
    install_panic_hook();
    match Command::parse(std::env::args_os().skip(1)) {
        Ok(Command::Serve) => serve(),
        Ok(Command::Version) => print_and_exit(&cli::version_line()),
        Ok(Command::Help) => print_and_exit(cli::USAGE),
        Ok(Command::PrintConfig) => match cli::current_client_config() {
            Ok(config) => print_and_exit(&config),
            Err(error) => fail(&error.to_string()),
        },
        Err(error) => {
            eprintln!("tablex-mcp: {error}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}

/// The three informational commands are the only code that writes to stdout besides the MCP
/// transport, and they never run together with it.
fn print_and_exit(text: &str) -> ExitCode {
    // A closed pipe (`tablex-mcp --help | head -1`) is not an error worth a panic.
    let _ = std::io::stdout().write_all(text.as_bytes());
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("tablex-mcp: {message}");
    ExitCode::FAILURE
}

/// stdout is the protocol, so nothing may be printed to it, and a panic message can carry
/// connection text: log the location only, to stderr. (A panic inside a tool call is caught on
/// its blocking thread and answered as an error result; the protocol task never writes half a
/// frame because a frame is written in one `write_all` + flush after it is fully encoded.)
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let place = info
            .location()
            .map_or_else(String::new, |at| format!(" at {}:{}", at.file(), at.line()));
        let _ = writeln!(
            std::io::stderr(),
            "tablex-mcp: internal error (panic){place}"
        );
    }));
}

/// The blocking pool is shared by everything that blocks: up to [`MAX_CONCURRENT_CALLS`] tool
/// calls (16), the stdin read (tokio reads stdin on this pool), the stdout write (one at a time),
/// and `close_all` at shutdown: 16 + 1 + 1 + 1 = 19 threads at most, so 32 can never be exhausted
/// and the protocol never starves behind running statements. (Left alone it is 512, which more
/// than 512 calls could exhaust.)
const MAX_BLOCKING_THREADS: usize = 32;
const _: () = assert!(MAX_CONCURRENT_CALLS + 3 <= MAX_BLOCKING_THREADS);

fn serve() -> ExitCode {
    for notice in setup::startup_notices() {
        eprintln!("tablex-mcp: {notice}");
    }
    let (layer, audit) = match setup::production_layer() {
        Ok(parts) => parts,
        Err(error) => return fail(&error.to_string()),
    };
    let server = match McpServer::new(Arc::new(layer)) {
        Ok(server) => server,
        Err(error) => return fail(&error.to_string()),
    };
    // One protocol task and a few short-lived blocking threads (each tool call, the stdin read):
    // a current-thread runtime is enough and keeps the process light. The calls themselves run
    // on tokio's blocking pool, not on this thread.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&format!("cannot start the runtime: {error}")),
    };
    let result = runtime.block_on(async {
        let mut signals = Signals::install();
        server
            .serve(tokio::io::stdin(), tokio::io::stdout(), async || {
                signals.next().await;
            })
            .await
    });
    // Queued audit lines reach the file even when a hung call still holds the layer.
    audit.flush();
    // A blocking read of stdin (or a driver call that hangs) may still be pending after a
    // signal; do not wait for it. Returning from `main` ends the process whatever other threads
    // are blocked, so no `process::exit` is needed.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error.to_string()),
    }
}
