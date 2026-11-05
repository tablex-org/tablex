//! `tablex-sqruff`: reads one format request on stdin, answers on stdout (see
//! `engine_driver_api::sql_format`), and exits. With stdin closed it only says hello.

fn main() -> std::process::ExitCode {
    let (mut stdin, mut stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    match engine_driver_api::sql_format::serve(&mut stdin, &mut stdout, tablex_sqruff::format) {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tablex-sqruff: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
