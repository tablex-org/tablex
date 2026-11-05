use super::*;
use crate::settings::KeywordCase;

fn upper() -> SqlFormatter {
    SqlFormatter::BuiltIn(BuiltInOptions {
        keyword_case: KeywordCase::Upper,
        indent: 2,
        lines_between_statements: 1,
    })
}

fn pg() -> SqlLexOptions {
    SqlLexOptions::postgres()
}

fn format_with(formatter: &SqlFormatter, sql: &str, lex: &SqlLexOptions) -> String {
    formatter.format(sql, lex).unwrap_or_else(|error| panic!("{sql:?}: {error}")).text
}

#[test]
fn lays_a_statement_out_and_sets_keyword_case() {
    let text = format_with(&upper(), "select a,b from t where x=1 and y='k' order by a", &pg());
    assert_eq!(
        text,
        "SELECT\n  a,\n  b\nFROM\n  t\nWHERE\n  x = 1\n  AND y = 'k'\nORDER BY\n  a"
    );
    let lower = SqlFormatter::BuiltIn(BuiltInOptions {
        keyword_case: KeywordCase::Lower,
        indent: 4,
        lines_between_statements: 2,
    });
    assert_eq!(
        format_with(&lower, "SELECT A FROM T WHERE X=1; SELECT 2", &pg()),
        "select\n    A\nfrom\n    T\nwhere\n    X = 1;\n\n\nselect\n    2"
    );
    let keep = SqlFormatter::BuiltIn(BuiltInOptions {
        keyword_case: KeywordCase::Keep,
        indent: 2,
        lines_between_statements: 1,
    });
    assert_eq!(format_with(&keep, "Select a From t", &pg()), "Select\n  a\nFrom\n  t");
}

#[test]
fn an_identifier_that_looks_like_a_keyword_keeps_its_case() {
    // `action` is in sqlformat's word list; here it is a column.
    let text = format_with(&upper(), "select action, key, name from audit where action = 'x'", &pg());
    assert!(text.contains("action,") && text.contains("key,") && text.contains("name"), "{text}");
    assert!(text.contains("SELECT") && text.contains("FROM") && text.contains("WHERE"), "{text}");
    assert!(!text.contains("ACTION") && !text.contains("KEY") && !text.contains("NAME"), "{text}");
}

#[test]
fn on_delete_set_null_stays_the_same_tokens_whatever_the_layout() {
    let sql = "create table c (id int primary key, p int references p (id) on delete set null on update cascade)";
    let text = format_with(&upper(), sql, &pg());
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("PRIMARY KEY") && flat.contains("ON DELETE SET NULL ON UPDATE CASCADE"), "{text}");
}

#[test]
fn dialect_quoting_survives() {
    let mysql = SqlLexOptions::mysql();
    let text = format_with(&upper(), "select `Order Id`, \"a b\" from `My Table` where x=1", &mysql);
    assert!(text.contains("`Order Id`") && text.contains("\"a b\"") && text.contains("`My Table`"), "{text}");
    let ansi = SqlLexOptions::mysql().with_ansi_quotes();
    let text = format_with(&upper(), "select \"Order Id\" from t", &ansi);
    assert!(text.contains("\"Order Id\""), "{text}");
}

#[test]
fn dollar_quoted_bodies_are_kept_whole_or_the_format_is_refused() {
    let sql = "create function f() returns int as $$ begin return 1; end $$ language plpgsql";
    match upper().format(sql, &pg()) {
        Ok(done) => assert!(done.text.contains("$$ begin return 1; end $$"), "{}", done.text),
        Err(FormatError::Refused(_)) => {}
        Err(other) => panic!("{other}"),
    }
}

#[test]
fn comments_and_blank_text() {
    let text = format_with(&upper(), "-- first\nselect 1 /* one */, 2 -- two\nfrom t", &pg());
    assert!(text.contains("-- first") && text.contains("/* one */") && text.contains("-- two"), "{text}");
    assert_eq!(upper().format(" \n ", &pg()), Err(FormatError::Empty));
}

#[test]
fn a_formatter_that_changes_a_token_is_refused_by_the_guard() {
    // The guard is the same one whoever produced the output.
    let refused = guard::verify("select a from t", "select b from t", &pg());
    assert!(matches!(refused, Err(GuardViolation::TokenChanged { index: 1 })));
}

#[test]
fn the_target_is_the_touched_statements_or_everything() {
    let text = "  select 1;\nselect 2;\nselect 3  \n";
    let lex = pg();
    assert_eq!(&text[target_range(text, 0..0, &lex).unwrap()], "select 1;\nselect 2;\nselect 3");
    // A selection inside the second statement.
    let at = text.find("elect 2").unwrap();
    assert_eq!(&text[target_range(text, at..at + 3, &lex).unwrap()], "select 2");
    // From the middle of the first to the middle of the third.
    let (a, b) = (text.find("1;").unwrap(), text.find("3").unwrap());
    assert_eq!(&text[target_range(text, a..b, &lex).unwrap()], "select 1;\nselect 2;\nselect 3");
    // Only blanks selected, or nothing to format.
    assert_eq!(target_range(text, 10..11, &lex), None);
    assert_eq!(target_range("  \n", 0..0, &lex), None);
}

/// `cargo test --release -p tablex-gpui format_time_of -- --ignored --nocapture`
#[test]
#[ignore = "on-demand timing; run with --release --ignored --nocapture"]
fn format_time_of_a_200_statement_script() {
    let script: String = (0..200)
        .map(|i| match i % 4 {
            0 => format!("select o.id, o.total, c.name from orders o join customers c on c.id=o.customer_id where o.total>{i} and c.region in ('eu','us') order by o.total desc limit 50;\n"),
            1 => format!("insert into audit(action, at, payload) values ('login', now(), '{{\"user\": {i}}}');\n"),
            2 => format!("update accounts set balance=balance-{i}, updated_at=now() where id={i} and balance>={i}; -- debit\n"),
            _ => format!("with recent as (select * from events where at>now()-interval '1 day') select kind, count(*) from recent group by kind having count(*)>{i};\n"),
        })
        .collect();
    let formatter = upper();
    let lex = pg();
    let started = std::time::Instant::now();
    let done = formatter.format(&script, &lex).unwrap();
    let elapsed = started.elapsed();
    let raw_started = std::time::Instant::now();
    let raw = super::built_in(&script, &lex, &BuiltInOptions { keyword_case: KeywordCase::Upper, indent: 2, lines_between_statements: 1 });
    let raw_elapsed = raw_started.elapsed();
    eprintln!(
        "FORMAT 200 statements ({} bytes -> {} bytes): {elapsed:?} with the guard, {raw_elapsed:?} for sqlformat alone ({} bytes)",
        script.len(),
        done.text.len(),
        raw.len()
    );
}

// ---- the formatters that run a process ----------------------------------------------------------

mod processes {
    use super::*;
    use crate::sql_format::external::PRESETS;
    use engine_driver_api::sql_format::Dialect;
    use engine_driver_host::{FormatCancel, ToolStore};
    use std::sync::Arc;
    use std::time::Duration;

    fn context(dialect: Dialect, tools: Option<Arc<ToolStore>>) -> FormatContext {
        FormatContext { dialect, tools, cancel: FormatCancel::new() }
    }

    fn external(program: &str, args: &str) -> SqlFormatter {
        SqlFormatter::External(ExternalOptions {
            context: context(Dialect::Postgres, None),
            program: program.into(),
            args: args.into(),
            exit_codes: "0".into(),
            timeout: Duration::from_secs(20),
        })
    }

    #[test]
    fn an_unset_command_and_a_missing_program_say_so() {
        assert_eq!(external("", "").format("select 1", &pg()), Err(FormatError::NotConfigured));
        match external("tablex-no-such-formatter", "-").format("select 1", &pg()) {
            Err(FormatError::Failed(message)) => assert!(message.contains("tablex-no-such-formatter") && message.contains("PATH"), "{message}"),
            other => panic!("{other:?}"),
        }
        match external("cat", "\"unclosed").format("select 1", &pg()) {
            Err(FormatError::Failed(message)) => assert!(message.contains("quote"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(external("cat", "").format(" \n", &pg()), Err(FormatError::Empty));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// An executable shell script in a fresh directory.
        pub fn script(name: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
            let dir = std::env::temp_dir().join(format!("tablex-fmt-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{name}.sh"));
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        }

        fn run(body: &str, args: &str, sql: &str) -> Result<Verified, FormatError> {
            let (dir, path) = script("filter", body);
            let outcome = external(path.to_str().unwrap(), args).format(sql, &pg());
            let _ = std::fs::remove_dir_all(dir);
            outcome
        }

        #[test]
        fn a_command_that_lays_the_text_out_is_applied_with_identifier_case_kept() {
            // Breaks lines after commas and shouts every word: the guard gives `a` and `t` their spelling back.
            let done = run("sed 's/,/,\\n  /g' | tr '[:lower:]' '[:upper:]'", "", "select a,b from t").unwrap();
            assert_eq!(done.text, "SELECT a,\n  b FROM t");
        }

        #[test]
        fn the_dialect_reaches_the_arguments() {
            let (dir, path) = script("args", "echo \"$@\" > \"$(dirname \"$0\")/args.txt\"\ncat");
            let formatter = SqlFormatter::External(ExternalOptions {
                context: context(Dialect::Postgres, None),
                program: path.to_str().unwrap().into(),
                args: "fix --dialect {dialect} --language {language} \"a b\" -".into(),
            exit_codes: "0".into(),
                timeout: Duration::from_secs(20),
            });
            formatter.format("select 1", &pg()).unwrap();
            assert_eq!(std::fs::read_to_string(dir.join("args.txt")).unwrap().trim(), "fix --dialect postgres --language postgresql a b -");
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn output_the_guard_disagrees_with_is_offered_not_applied() {
            // The command rewrites the column: the first differing pair of tokens is named.
            match run("echo 'select b from t'", "", "select a from t") {
                Err(FormatError::Disagreement(found)) => {
                    assert_eq!(found.source, "The external command");
                    assert_eq!(found.formatted, "select b from t");
                    assert_eq!(found.violation, GuardViolation::TokenChanged { index: 1 });
                    assert_eq!(found.differing, Some((Some("a".into()), Some("b".into()))));
                    assert_eq!(found.first_difference().as_deref(), Some("The first difference: a became b."));
                }
                other => panic!("{other:?}"),
            }
            // A command that adds a clause: the new token has no partner.
            match run("echo 'select a from t where 1'", "", "select a from t") {
                Err(FormatError::Disagreement(found)) => {
                    assert_eq!(found.differing, Some((None, Some("where".into()))));
                    assert_eq!(found.first_difference().as_deref(), Some("The first difference: where was added."));
                }
                other => panic!("{other:?}"),
            }
            // The built-in formatter, by contrast, is refused outright.
            assert!(matches!(
                upper().format("select /*!50000 1 */", &SqlLexOptions::mysql()),
                Err(FormatError::Refused(GuardViolation::ExecutableComment))
            ));
        }

        #[test]
        fn a_failing_command_shows_its_stderr_and_a_silent_one_is_an_error() {
            match run("echo 'syntax error near line 1' >&2; exit 3", "", "select 1") {
                Err(FormatError::Failed(message)) => assert!(message.contains("syntax error near line 1") && message.contains('3'), "{message}"),
                other => panic!("{other:?}"),
            }
            match run("cat >/dev/null", "", "select 1") {
                Err(FormatError::Failed(message)) => assert!(message.contains("no text"), "{message}"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn the_timeout_stops_a_command_that_hangs() {
            let (dir, path) = script("hang", "sleep 30");
            let formatter = SqlFormatter::External(ExternalOptions {
                context: context(Dialect::Postgres, None),
                program: path.to_str().unwrap().into(),
                args: String::new(),
            exit_codes: "0".into(),
                timeout: Duration::from_secs(1),
            });
            let started = std::time::Instant::now();
            match formatter.format("select 1", &pg()) {
                Err(FormatError::Failed(message)) => assert!(message.contains("longer than 1 s"), "{message}"),
                other => panic!("{other:?}"),
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_cancelled_run_ends_quietly() {
            let (dir, path) = script("cancel", "sleep 30");
            let cancel = FormatCancel::new();
            let formatter = SqlFormatter::External(ExternalOptions {
                context: FormatContext { dialect: Dialect::Ansi, tools: None, cancel: cancel.clone() },
                program: path.to_str().unwrap().into(),
                args: String::new(),
            exit_codes: "0".into(),
                timeout: Duration::from_secs(30),
            });
            let worker = std::thread::spawn(move || formatter.format("select 1", &pg()));
            std::thread::sleep(Duration::from_millis(400));
            cancel.cancel();
            assert_eq!(worker.join().unwrap(), Err(FormatError::Cancelled));
            let _ = std::fs::remove_dir_all(dir);
        }

        /// A tools directory holding a `tablex-sqruff` that is a script speaking the protocol: it
        /// answers every request with `answer` (a JSON `done` frame is written for it).
        pub fn store_with_tool(answer: &str) -> (std::path::PathBuf, Arc<ToolStore>) {
            store_with_tool_speaking(answer, engine_driver_api::sql_format::PROTOCOL_VERSION)
        }

        pub fn store_with_tool_speaking(answer: &str, protocol: u32) -> (std::path::PathBuf, Arc<ToolStore>) {
            use engine_driver_api::mongo_shell::write_frame;
            use engine_driver_api::sql_format::FromTool;
            use engine_driver_host::{DriverManifest, ManifestTool, NoProgress, ToolInstaller};
            let escapes = |frame: &FromTool| {
                let mut bytes = Vec::new();
                write_frame(&mut bytes, frame).unwrap();
                bytes.iter().map(|byte| format!("\\{byte:03o}")).collect::<String>()
            };
            let body = format!(
                "#!/bin/sh\nprintf '{}'\nhead -c 1 >/dev/null\nprintf '{}'\n",
                escapes(&FromTool::Hello { protocol }),
                escapes(&FromTool::Done { formatted: answer.into() })
            );
            let dir = std::env::temp_dir().join(format!("tablex-fmt-tools-{}", uuid::Uuid::new_v4()));
            let store = Arc::new(ToolStore::new(dir.join("tools")));
            let target = crate::drivers::current_target();
            let sha256: String = {
                use sha2::{Digest, Sha256};
                Sha256::digest(body.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
            };
            let manifest = serde_json::json!({"schemaVersion": 2, "drivers": [], "tools": [{
                "id": "tablex-sqruff", "kind": "sqlFormatter", "displayName": "SQL Formatter (sqruff)", "version": "0.1.0",
                "artifacts": [{"target": target.id(), "url": "https://example.com/x", "sha256": sha256, "sizeBytes": body.len()}],
            }]});
            let manifest: DriverManifest = serde_json::from_value(manifest).unwrap();
            let Some(ManifestTool::Known(entry)) = manifest.tools.into_iter().next() else { panic!("a known tool") };
            let bytes = body.into_bytes();
            ToolInstaller::new(target, &store)
                .install(&entry, move |_, _| Ok(Box::new(std::io::Cursor::new(bytes))), &std::sync::atomic::AtomicBool::new(false), &NoProgress)
                .expect("installs");
            (dir, store)
        }

        #[test]
        fn the_tool_formats_through_the_installed_executable() {
            let (dir, store) = store_with_tool("SELECT\n  a\nFROM t");
            let formatter = SqlFormatter::Tool(ToolOptions {
                context: context(Dialect::Postgres, Some(store)),
                keyword_case: KeywordCase::Upper,
                indent: 2,
                timeout: Duration::from_secs(20),
            });
            assert_eq!(formatter.format("select a from t", &pg()).unwrap().text, "SELECT\n  a\nFROM t");
            // What the tool answers goes through the same check: a rewritten column is offered, not applied.
            let (other, store) = store_with_tool("SELECT\n  b\nFROM t");
            let formatter = SqlFormatter::Tool(ToolOptions {
                context: context(Dialect::Postgres, Some(store)),
                keyword_case: KeywordCase::Upper,
                indent: 2,
                timeout: Duration::from_secs(20),
            });
            match formatter.format("select a from t", &pg()) {
                Err(FormatError::Disagreement(found)) => {
                    assert_eq!(found.source, "SQL Formatter (sqruff)");
                    assert_eq!(found.differing, Some((Some("a".into()), Some("b".into()))));
                }
                other => panic!("{other:?}"),
            }
            for dir in [dir, other] {
                let _ = std::fs::remove_dir_all(dir);
            }
        }

        #[test]
        fn a_tool_of_another_protocol_version_says_to_update_it() {
            let (dir, store) = store_with_tool_speaking("SELECT 1", 99);
            let formatter = SqlFormatter::Tool(ToolOptions {
                context: context(Dialect::Postgres, Some(store)),
                keyword_case: KeywordCase::Upper,
                indent: 2,
                timeout: Duration::from_secs(20),
            });
            match formatter.format("select 1", &pg()) {
                Err(FormatError::Failed(message)) => {
                    assert!(message.contains("protocol 99") && message.contains("Update SQL Formatter (sqruff) in the Driver Manager"), "{message}")
                }
                other => panic!("{other:?}"),
            }
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_linter_that_exits_1_with_its_output_is_accepted_when_the_exit_code_says_so() {
            // `sqruff fix -`: the fixed text on stdout, exit 1 because something is left unfixed.
            let (dir, path) = script("linter", "cat >/dev/null\nprintf 'select a,\\n  b from t'\necho 'LT05 left' >&2\nexit 1");
            let with = |codes: &str| {
                SqlFormatter::External(ExternalOptions {
                    context: context(Dialect::Postgres, None),
                    program: path.to_str().unwrap().into(),
                    args: String::new(),
                    exit_codes: codes.into(),
                    timeout: Duration::from_secs(20),
                })
            };
            match with("0").format("select a,b from t", &pg()) {
                Err(FormatError::Failed(message)) => assert!(message.contains("LT05 left"), "{message}"),
                other => panic!("{other:?}"),
            }
            assert_eq!(with("0,1").format("select a,b from t", &pg()).unwrap().text, "select a,\n  b from t");
            // The guard still judges what was accepted.
            assert!(matches!(with("0 1").format("select z,b from t", &pg()), Err(FormatError::Disagreement(_))));
            match with("0,x").format("select a,b from t", &pg()) {
                Err(FormatError::Failed(message)) => assert!(message.contains("exit code"), "{message}"),
                other => panic!("{other:?}"),
            }
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_replaced_executable_is_not_run() {
            let (dir, store) = store_with_tool("SELECT 1");
            let executable = store.executable_path("tablex-sqruff").unwrap();
            std::fs::write(&executable, "#!/bin/sh\necho owned > /tmp/tablex-should-not-exist\n").unwrap();
            let formatter = SqlFormatter::Tool(ToolOptions {
                context: context(Dialect::Postgres, Some(store)),
                keyword_case: KeywordCase::Upper,
                indent: 2,
                timeout: Duration::from_secs(20),
            });
            assert_eq!(formatter.format("select 1", &pg()), Err(FormatError::ToolNotInstalled));
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn the_tool_is_not_installed_without_a_tools_folder_or_a_record() {
        let options = |tools| ToolOptions {
            context: context(Dialect::Postgres, tools),
            keyword_case: KeywordCase::Upper,
            indent: 2,
            timeout: Duration::from_secs(5),
        };
        assert_eq!(SqlFormatter::Tool(options(None)).format("select 1", &pg()), Err(FormatError::ToolNotInstalled));
        let empty = Arc::new(ToolStore::new(std::env::temp_dir().join(format!("tablex-fmt-empty-{}", uuid::Uuid::new_v4()))));
        assert_eq!(SqlFormatter::Tool(options(Some(empty))).format("select 1", &pg()), Err(FormatError::ToolNotInstalled));
    }

    #[test]
    fn explain_dialects_map_to_the_wire_dialects() {
        use engine_plan::ExplainDialect::*;
        let wire = [Postgres, MySql, MariaDb, Sqlite, ClickHouse].map(wire_dialect);
        assert_eq!(wire, [Dialect::Postgres, Dialect::Mysql, Dialect::Mariadb, Dialect::Sqlite, Dialect::Clickhouse]);
    }

    #[test]
    fn the_presets_cover_the_four_tools() {
        let programs: Vec<_> = PRESETS.iter().map(|preset| preset.program).collect();
        for program in ["sqruff", "sqlfluff", "sql-formatter", "pg_format"] {
            assert!(programs.contains(&program), "{program}");
        }
    }
}
