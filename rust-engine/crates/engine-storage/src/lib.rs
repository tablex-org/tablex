pub mod aggregate;
pub mod ai_chat;
pub mod app_db;
pub mod connection;
pub mod documents;
pub mod history;
pub mod legacy;
pub mod paths;
pub mod pinned_nodes;
pub mod view_state;

#[cfg(test)]
mod import_tests;

/// Brings every store up, which moves the old files of each in the first time (see [`legacy`]).
/// The app calls this once at start on a thread of its own so that the work is done before the
/// stores are first read, and not on the thread that draws; a store used meanwhile waits for its
/// own import (it never reads before it). Errors are not reported here: they are the same ones
/// the first real use gets.
pub fn warm_up() {
    // The connections first: the sidebar needs them before anything else.
    let _ = connection::list_connections();
    let _ = history::fetch_entries("", "", "", 0);
    let _ = view_state::load_state("");
    let _ = aggregate::list_aggregates("", "");
    let db = app_db::default_path();
    for kind in [documents::SESSION, documents::UPDATE_CHECK, documents::DIAGRAM] {
        let _ = documents::count(&db, kind);
    }
    let _ = documents::check_conversations(&db);
}

/// Moves the old files of every store into the database now, without keeping any connection (what
/// [`warm_up`] does through the stores themselves): the first error is returned after the others
/// were tried. A process that may not import ([`legacy::set_import_allowed`]) moves nothing.
pub fn import_old_files() -> Result<(), String> {
    let db = app_db::default_path();
    let mut first: Option<String> = None;
    let mut note = |result: Result<(), app_db::AppDbError>| {
        if let Err(error) = result {
            first.get_or_insert(error.to_string());
        }
    };
    note(connection::open_at(&db, &connection::legacy_path()).map(drop));
    note(history::open_at(&db, &history::legacy_path()).map(drop));
    note(aggregate::open_at(&db, &aggregate::legacy_path()).map(drop));
    note(view_state::open_at(&db, &view_state::legacy_path()).map(drop));
    for kind in [documents::SESSION, documents::UPDATE_CHECK, documents::DIAGRAM] {
        note(documents::count(&db, kind).map(drop));
    }
    note(documents::check_conversations(&db));
    first.map_or(Ok(()), Err)
}
