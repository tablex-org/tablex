//! Running a [`Planned`] change-set and reporting what happened (no GPUI, no
//! driver): the caller supplies a [`StatementRunner`] that sends one statement
//! to the connection and waits for it.
//!
//! A dialect with transactional DDL runs the statements between `BEGIN` and
//! `COMMIT` and rolls everything back on the first error. The others run
//! statement by statement and stop at the first error; what ran stays applied
//! and the [`Outcome`] says exactly which statements those were.

use super::{Planned, Step};

/// Sends one statement and waits for it. The error is the database's message.
pub trait StatementRunner {
    fn run(&mut self, sql: &str) -> Result<(), String>;

    /// Runs `COMMIT` / `ROLLBACK`: unlike [`Self::run`] it is not refused after a cancel.
    fn finish(&mut self, sql: &str) -> Result<(), String> {
        self.run(sql)
    }

    /// The user asked to stop.
    fn cancelled(&self) -> bool {
        false
    }
}

/// The statement that stopped the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Index into the steps.
    pub step: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Steps that ran and are still applied (empty after a rollback).
    pub applied: Vec<usize>,
    pub failed: Option<Failure>,
    pub transactional: bool,
    /// The transaction was rolled back (so nothing is applied).
    pub rolled_back: bool,
    /// The rollback itself failed: the connection may still hold an open transaction.
    pub rollback_error: Option<String>,
    /// The run was stopped by the user.
    pub cancelled: bool,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.failed.is_none()
    }

    /// Indices of the changes whose statements all ran (and stayed applied).
    pub fn completed_changes(&self, planned: &Planned) -> Vec<usize> {
        let steps = planned.steps();
        let step_ran = |change: usize| {
            let own: Vec<usize> = steps.iter().enumerate().filter(|(_, s)| s.change == change).map(|(i, _)| i).collect();
            !own.is_empty() && own.iter().all(|i| self.applied.contains(i))
        };
        // A change that needs no statement (nothing to do) is done with a run that succeeded.
        let nothing_to_do = |entry: &super::Entry| entry.statements.as_ref().is_ok_and(Vec::is_empty);
        let mut done: Vec<usize> = planned
            .entries
            .iter()
            .filter(|entry| entry.folded_into.is_none() && entry.statements.is_ok())
            .filter(|entry| if nothing_to_do(entry) { self.succeeded() } else { step_ran(entry.change) })
            .map(|entry| entry.change)
            .collect();
        // A folded change (a foreign key inside CREATE TABLE) is done with its host.
        let folded: Vec<usize> = planned
            .entries
            .iter()
            .filter(|entry| entry.folded_into.is_some_and(|host| done.contains(&host)))
            .map(|entry| entry.change)
            .collect();
        done.extend(folded);
        done.sort_unstable();
        done
    }

    /// Changes some but not all of whose statements ran (a database without
    /// transactional DDL stopped in the middle of a change that needs several).
    pub fn partial_changes(&self, planned: &Planned) -> Vec<usize> {
        let steps = planned.steps();
        planned
            .entries
            .iter()
            .filter(|entry| entry.folded_into.is_none())
            .filter(|entry| {
                let own: Vec<usize> = steps.iter().enumerate().filter(|(_, s)| s.change == entry.change).map(|(i, _)| i).collect();
                let ran = own.iter().filter(|i| self.applied.contains(i)).count();
                ran > 0 && ran < own.len()
            })
            .map(|entry| entry.change)
            .collect()
    }

    /// A message for the user: what ran, what failed, what is left.
    pub fn report(&self, steps: &[Step]) -> String {
        let total = steps.len();
        let Some(failure) = &self.failed else {
            return format!("Applied {total} {}.", if total == 1 { "statement" } else { "statements" });
        };
        if self.cancelled {
            let ran = if let (true, Some(error)) = (self.transactional, &self.rollback_error) {
                format!("The rollback failed ({error}): the transaction may still be open on the server, so what ran is not known. Check the connection before trying again.")
            } else if self.transactional {
                "The transaction was rolled back: nothing was applied.".to_owned()
            } else if self.applied.is_empty() {
                "Nothing had run yet.".to_owned()
            } else {
                let ran: Vec<String> = self
                    .applied
                    .iter()
                    .map(|&i| format!("  {}. {}", i + 1, steps.get(i).map_or("", |step| step.sql.as_str()).lines().next().unwrap_or("")))
                    .collect();
                format!("This database cannot roll DDL back. These {} statement(s) ran and stay applied:\n{}", self.applied.len(), ran.join("\n"))
            };
            return format!("Cancelled. {ran}");
        }
        let sql = steps.get(failure.step).map_or("", |step| step.sql.as_str());
        let head = format!("Statement {} of {total} failed: {}\n{sql}", failure.step + 1, failure.message);
        if self.transactional {
            let tail = match (&self.rollback_error, self.rolled_back) {
                (Some(error), _) => format!("The rollback failed too ({error}); check the connection for an open transaction."),
                (None, true) => "The transaction was rolled back: nothing was applied.".to_owned(),
                (None, false) => "Nothing was applied.".to_owned(),
            };
            format!("{head}\n{tail}")
        } else if self.applied.is_empty() {
            format!("{head}\nNothing was applied.")
        } else {
            let ran: Vec<String> = self
                .applied
                .iter()
                .map(|&i| format!("  {}. {}", i + 1, steps.get(i).map_or("", |step| step.sql.as_str()).lines().next().unwrap_or("")))
                .collect();
            format!(
                "{head}\nThis database cannot roll DDL back. These {} statement(s) ran and stay applied:\n{}\nThe remaining statements were not run.",
                self.applied.len(),
                ran.join("\n")
            )
        }
    }
}

#[cfg(test)]
/// Runs `planned`'s statements. Returns the outcome; an unexecutable plan
/// (a change that could not become SQL) runs nothing.
pub fn apply<R: StatementRunner + ?Sized>(planned: &Planned, runner: &mut R) -> Outcome {
    match apply_checked(planned, runner, |_: &mut R| Ok::<(), std::convert::Infallible>(())) {
        Ok(outcome) => outcome,
        Err(never) => match never {},
    }
}

/// [`apply`], with `check` run on the session once it is set up (inside the
/// transaction where there is one) and before the first statement. A check
/// that fails stops the run with nothing applied (the transaction is rolled
/// back) and its error is returned: the caller re-reads the catalog there, so
/// what it saw is what the transaction then changes.
pub fn apply_checked<R: StatementRunner + ?Sized, E>(
    planned: &Planned,
    runner: &mut R,
    check: impl FnOnce(&mut R) -> Result<(), E>,
) -> Result<Outcome, E> {
    // The statements are only borrowed: a statement that carries a password is not copied here.
    let steps: Vec<&str> = planned.entries.iter().flat_map(|entry| entry.statements.iter().flatten().map(String::as_str)).collect();
    let transactional = planned.transactional;
    let mut outcome =
        Outcome { applied: Vec::new(), failed: None, transactional, rolled_back: false, rollback_error: None, cancelled: false };
    if !planned.executable() {
        outcome.failed = Some(Failure { step: 0, message: "The change-set has changes that cannot be turned into SQL.".into() });
        return Ok(outcome);
    }
    if steps.is_empty() {
        check(runner)?;
        return Ok(outcome);
    }
    // Session settings (lock wait) before the transaction, `SET LOCAL` ones inside it.
    for sql in &planned.setup.before {
        if let Err(message) = runner.run(sql) {
            outcome.failed = Some(Failure { step: 0, message: format!("could not prepare the session ({sql}): {message}") });
            outcome.cancelled = runner.cancelled();
            return Ok(outcome);
        }
    }
    if transactional {
        if let Err(message) = runner.run("BEGIN") {
            outcome.failed = Some(Failure { step: 0, message: format!("could not start a transaction: {message}") });
            outcome.cancelled = runner.cancelled();
            return Ok(outcome);
        }
        for sql in &planned.setup.in_transaction {
            if let Err(message) = runner.run(sql) {
                outcome.failed = Some(Failure { step: 0, message: format!("could not prepare the transaction ({sql}): {message}") });
                let _ = runner.finish("ROLLBACK");
                outcome.rolled_back = true;
                outcome.cancelled = runner.cancelled();
                return Ok(outcome);
            }
        }
    }
    if let Err(error) = check(runner) {
        if transactional {
            let _ = runner.finish("ROLLBACK");
        }
        return Err(error);
    }
    for (index, sql) in steps.iter().enumerate() {
        match runner.run(sql) {
            Ok(()) => outcome.applied.push(index),
            Err(message) => {
                outcome.failed = Some(Failure { step: index, message });
                break;
            }
        }
    }
    outcome.cancelled = outcome.failed.is_some() && runner.cancelled();
    if transactional {
        let finish = if outcome.failed.is_none() { "COMMIT" } else { "ROLLBACK" };
        match runner.finish(finish) {
            Ok(()) if outcome.failed.is_some() => {
                outcome.applied.clear();
                outcome.rolled_back = true;
            }
            Ok(()) => {}
            Err(message) if outcome.failed.is_some() => {
                // Whatever the connection still holds is unknown.
                outcome.applied.clear();
                outcome.rollback_error = Some(message);
            }
            Err(message) => {
                // The commit failed: the database rolls the transaction back, but try anyway.
                let _ = runner.finish("ROLLBACK");
                outcome.failed = Some(Failure { step: steps.len().saturating_sub(1), message: format!("COMMIT failed: {message}") });
                outcome.applied.clear();
                outcome.rolled_back = true;
            }
        }
    }
    Ok(outcome)
}
