//! The user's own formatter: a program that reads SQL on stdin and writes the formatted SQL on
//! stdout (what Zed calls an external formatter). This file turns the settings text into a command
//! and finds the program; `engine_driver_host::run_filter` runs it (timeout, cancel, output cap,
//! stderr in the error).
//!
//! # Finding the program
//!
//! A program name is looked up on PATH. A macOS app started from the Dock or Finder does not
//! inherit the PATH of the user's shell (Homebrew's `/opt/homebrew/bin`, `~/.cargo/bin`, ...), so
//! on macOS the PATH is read once from the user's login shell ([`search_path`]: cached, a few
//! seconds at most, never on the UI thread) and put in front of the inherited one. A full path
//! needs no lookup. On Windows a name without an extension is tried with each of `PATHEXT`
//! (`.exe`, `.cmd`, ...), as the command prompt does.

use engine_driver_api::sql_format::Dialect;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Why the settings text is not a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentsError {
    UnclosedQuote,
}

impl std::fmt::Display for ArgumentsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedQuote => f.write_str("a quote in the arguments is not closed"),
        }
    }
}

impl std::error::Error for ArgumentsError {}

/// Splits `text` on blanks. A double or single quote groups what is inside it (the quotes go), so
/// an argument may contain blanks; nothing else is special, in particular a backslash is literal
/// (Windows paths).
pub fn split_arguments(text: &str) -> Result<Vec<String>, ArgumentsError> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(ArgumentsError::UnclosedQuote);
    }
    if started {
        arguments.push(current);
    }
    Ok(arguments)
}

/// `{dialect}` -> sqruff / sqlfluff's dialect name; `{language}` -> sql-formatter's. Any other
/// `{...}` stays as it is.
pub fn expand_placeholders(argument: &str, dialect: Dialect) -> String {
    let language = match dialect {
        Dialect::Ansi => "sql",
        Dialect::Postgres => "postgresql",
        other => other.as_str(),
    };
    argument.replace("{dialect}", dialect.as_str()).replace("{language}", language)
}

/// The directories to search, in order: `login` (the user's shell, when known) first, then
/// `inherited` (this process), without repeating one.
pub fn merge_search_paths(login: Option<&OsStr>, inherited: Option<&OsStr>) -> OsString {
    let mut directories: Vec<PathBuf> = Vec::new();
    for list in [login, inherited].into_iter().flatten() {
        for directory in std::env::split_paths(list) {
            if !directory.as_os_str().is_empty() && !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    std::env::join_paths(directories).unwrap_or_default()
}

/// How long the login shell may take to answer.
const LOGIN_SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// After a failed ask, how long before the next format asks again (Settings › Editor asks sooner).
const LOGIN_SHELL_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// What the login shell said, kept only when it answered: a shell that hung or failed is asked
/// again later instead of costing every format its PATH for the rest of the session.
#[derive(Default)]
struct LoginPathCache {
    path: Option<OsString>,
    failed_at: Option<std::time::Instant>,
}

impl LoginPathCache {
    /// The cached PATH, else `ask`'s answer (cached when there is one). After a failure `ask` is
    /// not called again before `retry_after` has passed.
    fn get(
        &mut self,
        now: std::time::Instant,
        retry_after: std::time::Duration,
        ask: impl FnOnce() -> Option<OsString>,
    ) -> Option<OsString> {
        if self.path.is_none() && self.failed_at.is_none_or(|failed| now.duration_since(failed) >= retry_after) {
            self.path = ask();
            self.failed_at = self.path.is_none().then_some(now);
        }
        self.path.clone()
    }
}

#[cfg(target_os = "macos")]
static LOGIN_PATH: std::sync::Mutex<Option<LoginPathCache>> = std::sync::Mutex::new(None);

/// The next format (or Settings › Editor) asks the login shell again if the last ask failed.
pub fn retry_login_shell() {
    #[cfg(target_os = "macos")]
    if let Ok(mut slot) = LOGIN_PATH.lock() {
        if let Some(cache) = slot.as_mut() {
            cache.failed_at = None;
        }
    }
}

/// The PATH to find formatters on. On macOS the first call (and the first after a failure) asks the
/// login shell, for at most `budget` (and at most five seconds), so call it from a background
/// thread and let the caller take `budget` out of its own time limit.
pub fn search_path(budget: std::time::Duration) -> OsString {
    #[cfg(target_os = "macos")]
    let login = {
        let Ok(mut slot) = LOGIN_PATH.lock() else { return std::env::var_os("PATH").unwrap_or_default() };
        let ask = || login_shell_path(&shell_program(), budget.min(LOGIN_SHELL_TIMEOUT));
        slot.get_or_insert_with(LoginPathCache::default).get(std::time::Instant::now(), LOGIN_SHELL_RETRY, ask)
    };
    #[cfg(not(target_os = "macos"))]
    let login: Option<OsString> = {
        let _ = budget;
        None
    };
    merge_search_paths(login.as_deref(), std::env::var_os("PATH").as_deref())
}

/// The program `program` names, as an absolute path: itself when it is one, else the first match in
/// the absolute directories of `path` (a relative entry, which would mean "wherever the app happens
/// to run", is skipped, and so is a relative program with a directory part).
///
/// `pathext` is the Windows rule when it is not empty (`PATHEXT`: `.COM;.EXE;.BAT;.CMD`): a name
/// without one of those extensions is tried only with each of them appended, never as it is, as
/// `cmd.exe` does. npm puts an extensionless shell script next to `sql-formatter.cmd`, and the
/// script cannot be started. Empty `pathext` is the Unix rule: the name itself.
pub fn resolve_program(program: &str, path: &OsStr, pathext: &[String]) -> Option<PathBuf> {
    let names = |base: PathBuf| -> Vec<PathBuf> {
        let extension = base.extension().map(|extension| format!(".{}", extension.to_string_lossy()));
        let known = extension.is_some_and(|extension| pathext.iter().any(|ext| ext.eq_ignore_ascii_case(&extension)));
        if pathext.is_empty() || known {
            return vec![base];
        }
        pathext
            .iter()
            .map(|extension| {
                let mut name = base.clone().into_os_string();
                name.push(extension);
                PathBuf::from(name)
            })
            .collect()
    };
    let program_path = Path::new(program);
    if program_path.components().count() > 1 || program_path.is_absolute() {
        return program_path
            .is_absolute()
            .then(|| names(program_path.to_path_buf()))?
            .into_iter()
            .find(|candidate| is_runnable(candidate));
    }
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| names(directory.join(program)))
        .find(|candidate| is_runnable(candidate))
}

/// `PATHEXT` of this machine (`.COM;.EXE;...`); none off Windows.
pub fn system_pathext() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let list = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    list.split(';').filter(|extension| !extension.is_empty()).map(str::to_owned).collect()
}

fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ---- the user's login shell (macOS) ------------------------------------------------------------

#[cfg(target_os = "macos")]
fn shell_program() -> PathBuf {
    std::env::var_os("SHELL").filter(|shell| !shell.is_empty()).map_or_else(|| PathBuf::from("/bin/zsh"), PathBuf::from)
}

const PATH_START: &str = "__TABLEX_PATH_START__";
const PATH_END: &str = "__TABLEX_PATH_END__";

/// The PATH between the markers of the output of the script below; whatever a shell's startup files
/// print before or after it is ignored.
pub fn parse_path_output(output: &str) -> Option<OsString> {
    let after = output.split_once(PATH_START)?.1;
    let path = after.split_once(PATH_END)?.0;
    (!path.is_empty()).then(|| OsString::from(path))
}

/// Asks `shell` (a login, interactive shell, so the files that set PATH run) for its PATH. `None`
/// when it fails or takes longer than `timeout`. The answer ends at its closing marker, not at the end
/// of the shell's output: a startup file may leave a background program holding the pipe. The shell
/// and whatever it started (its own process group) are killed once the answer is in or time is up.
#[cfg(target_os = "macos")]
pub fn login_shell_path(shell: &Path, timeout: std::time::Duration) -> Option<OsString> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let script = format!("printf '%s%s%s' {PATH_START} \"$PATH\" {PATH_END}");
    let mut child = Command::new(shell)
        .args(["-ilc", &script])
        // As VS Code does: a startup file may skip what is slow or interactive.
        .env("TABLEX_RESOLVING_ENVIRONMENT", "1")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (done, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut seen, mut chunk) = (Vec::new(), [0u8; 4096]);
        while let Ok(count @ 1..) = stdout.read(&mut chunk) {
            seen.extend_from_slice(&chunk[..count]);
            if String::from_utf8_lossy(&seen).contains(PATH_END) {
                break;
            }
        }
        let _ = done.send(seen);
    });
    let seen = output.recv_timeout(timeout).ok();
    engine_driver_host::kill_process_tree(&mut child);
    let _ = child.wait();
    parse_path_output(&String::from_utf8_lossy(&seen?))
}

// ---- presets -----------------------------------------------------------------------------------

/// A well-known formatter, offered in Settings when its program is on PATH. Choosing one fills the
/// command and the arguments; the user can change them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub program: &'static str,
    pub args: &'static str,
    /// The exit codes that are a success (see [`parse_exit_codes`]). The linters' `fix` writes the
    /// fixed text and exits 1 when something it could not fix is left.
    pub exit_codes: &'static str,
}

/// Each reads SQL on stdin and writes the result on stdout; checked against the tools' own sources and
/// documentation (see the notes in the code comment of each), not run here.
pub const PRESETS: &[Preset] = &[
    // sqruff-cli-lib 0.42 `commands_fix.rs`: `fix -` reads stdin and writes the fixed SQL to stdout; it
    // exits 1 when a violation it cannot fix remains (the output is still written).
    Preset { id: "sqruff", label: "sqruff fix", program: "sqruff", args: "fix --dialect {dialect} -", exit_codes: "0,1" },
    // sqlfluff `cli/commands.py`: `format -` and `fix -` ("handle stdin case. should output formatted
    // sql to stdout and nothing else"); the dialect is required.
    Preset { id: "sqlfluff-format", label: "sqlfluff format", program: "sqlfluff", args: "format --dialect {dialect} -", exit_codes: "0,1" },
    Preset { id: "sqlfluff-fix", label: "sqlfluff fix", program: "sqlfluff", args: "fix --dialect {dialect} -", exit_codes: "0,1" },
    // sql-formatter 15 `bin/sql-formatter-cli.cjs`: no FILE reads stdin; `--language` takes its own names.
    Preset { id: "sql-formatter", label: "sql-formatter", program: "sql-formatter", args: "--language {language}", exit_codes: "0" },
    // pgFormatter `doc/pg_format.pod`: `-` reads stdin, the result goes to stdout.
    Preset { id: "pg_format", label: "pg_format", program: "pg_format", args: "-", exit_codes: "0" },
];

/// The presets whose program is on the search path. Blocks like [`search_path`]; looking again
/// also lets a login shell that failed before be asked again.
pub fn detect_presets() -> Vec<Preset> {
    retry_login_shell();
    let (path, pathext) = (search_path(LOGIN_SHELL_TIMEOUT), system_pathext());
    PRESETS.iter().copied().filter(|preset| resolve_program(preset.program, &path, &pathext).is_some()).collect()
}

/// `"0,1"` / `"0 1"` -> `[0, 1]`: the exit codes the user accepts as success. Empty means `[0]`.
pub fn parse_exit_codes(text: &str) -> Result<Vec<i32>, String> {
    let codes = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<i32>().map_err(|_| format!("{part:?} is not an exit code")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if codes.is_empty() { vec![0] } else { codes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tablex-ext-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn arguments_split_on_blanks_and_quotes_group() {
        assert_eq!(split_arguments("").unwrap(), Vec::<String>::new());
        assert_eq!(split_arguments("  fix  --dialect {dialect} - ").unwrap(), ["fix", "--dialect", "{dialect}", "-"]);
        assert_eq!(split_arguments(r#"--config "my dir/c.json" 'a b' x"#).unwrap(), ["--config", "my dir/c.json", "a b", "x"]);
        // An empty quoted argument is an argument; a backslash is literal; the other quote is text.
        assert_eq!(split_arguments(r#"'' C:\tools\x "it's""#).unwrap(), ["", r"C:\tools\x", "it's"]);
        assert_eq!(split_arguments("--a=\"b c\"d").unwrap(), ["--a=b cd"]);
        assert_eq!(split_arguments("a \"b"), Err(ArgumentsError::UnclosedQuote));
    }

    #[test]
    fn placeholders_name_the_dialect_for_each_tool() {
        assert_eq!(expand_placeholders("--dialect={dialect}", Dialect::Postgres), "--dialect=postgres");
        assert_eq!(expand_placeholders("{language}", Dialect::Postgres), "postgresql");
        assert_eq!(expand_placeholders("{language}", Dialect::Ansi), "sql");
        assert_eq!(expand_placeholders("{language}/{dialect}", Dialect::Mysql), "mysql/mysql");
        assert_eq!(expand_placeholders("{other} {Dialect}", Dialect::Sqlite), "{other} {Dialect}");
    }

    #[test]
    fn the_search_path_puts_the_login_shell_first_without_repeats() {
        let merged = merge_search_paths(Some(OsStr::new("/opt/homebrew/bin:/usr/bin")), Some(OsStr::new("/usr/bin:/bin::")));
        let directories: Vec<PathBuf> = std::env::split_paths(&merged).collect();
        assert_eq!(directories, [PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        assert_eq!(merge_search_paths(None, Some(OsStr::new("/bin"))), OsString::from("/bin"));
        assert_eq!(merge_search_paths(None, None), OsString::new());
    }

    #[cfg(unix)]
    #[test]
    fn a_program_is_found_on_the_given_path_only() {
        let (first, second) = (temp_dir("first"), temp_dir("second"));
        let in_second = executable(&second, "pg_format");
        let search = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(resolve_program("pg_format", &search, &[]), Some(in_second.clone()));
        // The first match wins.
        let in_first = executable(&first, "pg_format");
        assert_eq!(resolve_program("pg_format", &search, &[]), Some(in_first));
        assert_eq!(resolve_program("not-installed", &search, &[]), None);
        // A file without the execute bit is not a program.
        std::fs::write(second.join("data.txt"), "x").unwrap();
        assert_eq!(resolve_program("data.txt", &search, &[]), None);
        // A full path needs no PATH; a missing one is not found.
        assert_eq!(resolve_program(in_second.to_str().unwrap(), OsStr::new(""), &[]), Some(in_second));
        assert_eq!(resolve_program("/nonexistent/pg_format", &search, &[]), None);
        for dir in [first, second] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn windows_extensions_are_tried_after_the_name() {
        let dir = temp_dir("pathext");
        let cmd = executable(&dir, "sqlfluff.cmd");
        let search = std::env::join_paths([&dir]).unwrap();
        let pathext = [".exe".to_owned(), ".cmd".to_owned()];
        assert_eq!(resolve_program("sqlfluff", &search, &pathext), Some(cmd.clone()));
        // A name that already has an extension is not extended.
        assert_eq!(resolve_program("sqlfluff.cmd", &search, &pathext), Some(cmd));
        assert_eq!(resolve_program("sqlfluff", &search, &[]), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_windows_rule_never_runs_the_extensionless_shim_next_to_the_cmd_file() {
        // npm puts `sql-formatter` (a shell script) beside `sql-formatter.cmd`; Windows cannot run the script.
        let dir = temp_dir("shim");
        let (shim, cmd) = (executable(&dir, "sql-formatter"), executable(&dir, "sql-formatter.cmd"));
        let search = std::env::join_paths([&dir]).unwrap();
        let pathext = [".com".to_owned(), ".exe".to_owned(), ".bat".to_owned(), ".cmd".to_owned()];
        assert_eq!(resolve_program("sql-formatter", &search, &pathext), Some(cmd.clone()));
        // With only the shim present there is nothing to run (the Unix rule would run it).
        std::fs::remove_file(&cmd).unwrap();
        assert_eq!(resolve_program("sql-formatter", &search, &pathext), None);
        assert_eq!(resolve_program("sql-formatter", &search, &[]), Some(shim.clone()), "the Unix rule: the file itself");
        // The extension decides case-insensitively, and a known one is used as typed.
        let exe = executable(&dir, "pg_format.exe");
        assert_eq!(resolve_program("pg_format.exe", &search, &pathext), Some(exe.clone()));
        assert_eq!(resolve_program("pg_format", &search, &pathext), Some(exe));
        let upper = executable(&dir, "tool.CMD");
        assert_eq!(resolve_program("tool.CMD", &search, &[".cmd".to_owned()]), Some(upper));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_path_entry_or_program_is_never_run() {
        let dir = temp_dir("relative");
        executable(&dir, "pg_format");
        let search = std::env::join_paths([PathBuf::from("."), PathBuf::from("bin"), dir.clone()]).unwrap();
        // The relative entries are skipped; the answer is an absolute path.
        let found = resolve_program("pg_format", &search, &[]).unwrap();
        assert!(found.is_absolute() && found.starts_with(&dir), "{found:?}");
        let relative_only = std::env::join_paths([PathBuf::from("."), PathBuf::from(dir.file_name().unwrap())]).unwrap();
        assert_eq!(resolve_program("pg_format", &relative_only, &[]), None);
        // A program with a directory part must be absolute.
        assert_eq!(resolve_program("./pg_format", &search, &[]), None);
        assert!(resolve_program(&format!("{}/../{}/pg_format", dir.display(), dir.file_name().unwrap().to_string_lossy()), &search, &[]).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exit_codes_are_numbers_separated_by_commas_or_blanks() {
        assert_eq!(parse_exit_codes("0,1"), Ok(vec![0, 1]));
        assert_eq!(parse_exit_codes(" 0  1 , 2"), Ok(vec![0, 1, 2]));
        assert_eq!(parse_exit_codes(""), Ok(vec![0]));
        assert!(parse_exit_codes("0,one").unwrap_err().contains("one"));
        // The linters' presets accept 1 (a result with something left unfixed); the others do not.
        let accepted = |id: &str| parse_exit_codes(PRESETS.iter().find(|preset| preset.id == id).unwrap().exit_codes).unwrap();
        for id in ["sqruff", "sqlfluff-fix", "sqlfluff-format"] {
            assert_eq!(accepted(id), [0, 1], "{id}");
        }
        for id in ["sql-formatter", "pg_format"] {
            assert_eq!(accepted(id), [0], "{id}");
        }
    }

    #[test]
    fn only_an_answer_from_the_login_shell_is_remembered() {
        use std::time::{Duration, Instant};
        let (mut cache, now) = (LoginPathCache::default(), Instant::now());
        let mut asked = 0;
        // A failure is not kept, and is not retried at once.
        assert_eq!(cache.get(now, Duration::from_secs(30), || { asked += 1; None }), None);
        assert_eq!(cache.get(now + Duration::from_secs(1), Duration::from_secs(30), || { asked += 1; Some("/x".into()) }), None);
        assert_eq!(asked, 1, "within the retry delay the shell is not asked again");
        // Later (or when Settings asks sooner) it is asked again; the answer is kept for good.
        let found = cache.get(now + Duration::from_secs(31), Duration::from_secs(30), || { asked += 1; Some("/opt/homebrew/bin".into()) });
        assert_eq!(found, Some(OsString::from("/opt/homebrew/bin")));
        assert_eq!(cache.get(now + Duration::from_secs(99), Duration::from_secs(30), || panic!("cached")), found);
        assert_eq!(asked, 2);
        // `retry_login_shell` forgets only the failure time.
        let mut failed = LoginPathCache::default();
        assert_eq!(failed.get(now, Duration::from_secs(30), || None), None);
        failed.failed_at = None;
        assert_eq!(failed.get(now, Duration::from_secs(30), || Some("/y".into())), Some(OsString::from("/y")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_startup_file_that_leaves_a_program_running_does_not_hold_the_answer_back() {
        use std::time::{Duration, Instant};
        let dir = temp_dir("rc");
        let pid_file = dir.join("pid.txt");
        // Like an rc file that starts a daemon on the shell's stdout, then the shell answers.
        let shell = dir.join("shell.sh");
        std::fs::write(
            &shell,
            format!("#!/bin/sh\nsleep 60 &\necho $! > '{}'\nprintf '%s%s%s' {PATH_START} /opt/homebrew/bin:/usr/bin {PATH_END}\n", pid_file.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let started = Instant::now();
        let path = login_shell_path(&shell, Duration::from_secs(20));
        assert_eq!(path, Some(OsString::from("/opt/homebrew/bin:/usr/bin")));
        assert!(started.elapsed() < Duration::from_secs(5), "the answer ends at the marker: {:?}", started.elapsed());
        // The program it left behind was killed with the shell's process group.
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let alive = |pid: u32| std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|status| status.success());
        let waited = Instant::now();
        while alive(pid) && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "the leftover program is gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_path_is_read_between_the_markers_whatever_the_shell_printed() {
        let output = format!("Last login: Tue\nwelcome!\n{PATH_START}/opt/homebrew/bin:/usr/bin{PATH_END}\nbye");
        assert_eq!(parse_path_output(&output), Some(OsString::from("/opt/homebrew/bin:/usr/bin")));
        assert_eq!(parse_path_output("no markers"), None);
        assert_eq!(parse_path_output(&format!("{PATH_START}{PATH_END}")), None);
        assert_eq!(parse_path_output(&format!("{PATH_START}/a")), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_login_shell_is_asked_with_a_time_limit() {
        use std::time::{Duration, Instant};
        // A real shell: whatever PATH it has comes back.
        let path = login_shell_path(Path::new("/bin/sh"), Duration::from_secs(10)).expect("a PATH");
        assert!(std::env::split_paths(&path).count() > 0);
        // A shell that hangs is killed at the limit; one that does not exist is no PATH.
        let dir = temp_dir("shell");
        let hang = dir.join("hang.sh");
        std::fs::write(&hang, "#!/bin/sh\nsleep 30\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let started = Instant::now();
        assert_eq!(login_shell_path(&hang, Duration::from_millis(300)), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(login_shell_path(Path::new("/nonexistent/shell"), Duration::from_secs(1)), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_preset_names_a_program_and_only_known_placeholders() {
        for preset in PRESETS {
            assert!(!preset.program.is_empty() && !preset.id.is_empty());
            let arguments = split_arguments(preset.args).unwrap();
            for argument in arguments {
                let expanded = expand_placeholders(&argument, Dialect::Postgres);
                assert!(!expanded.contains("{dialect}") && !expanded.contains("{language}"), "{preset:?}");
            }
        }
    }
}
