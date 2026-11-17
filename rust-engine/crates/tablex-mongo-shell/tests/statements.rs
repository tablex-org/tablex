//! A script runs statement by statement, as a shell does: one that does not parse fails alone, at
//! its own line, and "On script error" decides whether the ones after it run.
mod common;
use common::*;
use engine_driver_api::mongo_shell::FailureKind;

#[test]
fn a_statement_that_does_not_parse_stops_the_run_at_its_line_after_the_ones_before_it() {
    let reply = run("print('first');\n\nselect * from c where a = 1;\n\nprint('never');", &FakePort::ok());
    let error = reply.error.as_ref().expect("the broken statement fails");
    assert_eq!(error.kind, FailureKind::Syntax);
    assert_eq!(error.line, Some(3), "its own line in the script");
    assert_eq!(reply.output, ["first"], "the statement before it ran, the one after did not");
}

#[test]
fn with_continue_on_error_the_failure_is_printed_and_the_rest_runs() {
    let mut request = request("print('first');\n\nselect * from c;\n\nvar n = 40;\nn + 2");
    request.continue_on_error = true;
    let reply = run_with(&request, &FakePort::ok());
    assert_ok(&reply);
    assert_eq!(reply.output.len(), 2, "{:?}", reply.output);
    assert_eq!(reply.output[0], "first");
    assert!(reply.output[1].starts_with("Syntax error (line 3, column "), "{:?}", reply.output[1]);
    assert_eq!(value_text(&reply), "42", "a declaration made by one statement is seen by the next");
}

#[test]
fn the_value_is_the_last_expressions_not_a_later_declarations() {
    let reply = run("var a = 1;\nlet b = 2;\nconst c = a + b;\nc * 10\nvar d = 5;\nfunction f() {}", &FakePort::ok());
    assert_ok(&reply);
    assert_eq!(value_text(&reply), "30");
}

#[test]
fn a_runtime_error_stops_or_goes_on_by_the_same_choice() {
    let script = "print('a');\nundefinedName.call();\nprint('b');";
    let stopped = run(script, &FakePort::ok());
    assert_eq!(stopped.error.as_ref().map(|error| error.line), Some(Some(2)));
    assert_eq!(stopped.output, ["a"]);
    let mut request = request(script);
    request.continue_on_error = true;
    let went_on = run_with(&request, &FakePort::ok());
    assert_ok(&went_on);
    assert_eq!(went_on.output.first().map(String::as_str), Some("a"));
    assert_eq!(went_on.output.last().map(String::as_str), Some("b"));
}
