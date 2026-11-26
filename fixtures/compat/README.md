# Upgrade compatibility fixtures

One folder per released version holds the **data dir that release wrote**, filled with made-up data
(example.com hosts, fake names, a throwaway secrets key). The tests load each folder with the current
code, assert every value survived, then save and load again and assert nothing was dropped. A change
that would make a user's saved data unreadable, or silently drop part of it, fails there.

Never copy anything from a real data dir (`~/Library/Application Support/tablex`, the Keychain, ...)
into a fixture: this repository is public.

```
v0.1.10/
  tablex/                      <data dir>/tablex
    tablex.db                  app database: connections, projects, history, aggregates, view state,
                               pinned rows, session, update check, diagram layouts, assistant
                               conversations, sealed secrets (fake master key, see the test)
    settings.json              every setting away from its default, AI section, custom connection
                               type, shortcut overrides (an unknown id included)
    sidebar-preferences.json   groups, colors, hidden, sort, widths, visible filters, ...
    Drivers/v2/                driver store: records, manifest watermark, update-check marker
    Projects/General/          saved SQL and scripts (+ .tablex/resource-metadata.json)
    logs/errors.log            error log
    key-home, master.key       (v0.1.11 on) the Linux choice to keep the key in a file, and that file
                               (the same fake master key)
  config/TableX/               <config dir>/TableX (a folder of its own here: the data dir is
                               `tablex`, and macOS compares names without case)
    grid-copy-default.json, snowflake/worker.json
```

## What checks them

| test | covers |
|---|---|
| `rust-engine/crates/engine-storage/tests/upgrade_compat.rs` | `tablex.db` as a file: every row of every table survives `app_db::open`, the schema of the old release is created the same by this build (a shipped migration was not edited), `min_reader_version` stays low enough for the old release to read the file again (downgrade), a newer or damaged file is refused untouched |
| `rust-engine/crates/engine-connect/tests/upgrade_compat.rs` | what the stores make of the rows (connections, folders/projects, colors, types, overrides, history, aggregates, pins) and the sealed secrets; the round trip through the stores |
| `rust-engine/crates/engine-driver-host/tests/upgrade_compat.rs` | driver store records, rollback, the manifest watermark, the legacy-location move |
| `apps/tablex-gpui/src/app/upgrade_compat.rs` | settings (also as `tablex-mcp` reads them), sidebar preferences, session and tab restore, update check, diagram layouts, assistant conversations, pinned-row keys, saved scripts, error log, small files; the persisted forms of the newest features are pinned there |

Generic checks do most of the work: *every JSON value of the old file must be at the same place in
what the current code writes back* (a renamed or dropped field, a changed enum name or a new
default-less field fails), and *a setting the old file does not have reads as its default* (this
covers every setting a release adds later, the Vim ones included: a new field needs `#[serde(default)]`
or a registry default, or the old fixture stops loading).

Each test runs over `VERSIONS`, so older fixture sets keep being checked by every later release.

## Adding the fixtures of a release (at release time)

Do this on the release commit, before tagging, so the folder is what *that* build writes.
Mac rules apply: one cargo process at a time, `-j 4`, `--test-threads=4`.

```sh
OUT=$PWD/fixtures/compat/vX.Y.Z
# 1. the app database (connections, history, ..., sealed secrets)
TABLEX_COMPAT_OUT=$OUT cargo test -j 4 -p engine-connect --test upgrade_compat \
  generate_fixture_set -- --ignored --test-threads=4
# 2. the files the app owns, and the documents inside the database
TABLEX_COMPAT_OUT=$OUT cargo test -j 4 -p tablex-gpui app::upgrade_compat::generate_fixture_set \
  -- --ignored --test-threads=4
# 3. the driver store: copy tablex/Drivers/v2 of the previous set, change what the release changed
# 4. remove the write-ahead files; the database must be one file
rm -f $OUT/tablex/tablex.db-wal $OUT/tablex/tablex.db-shm
```

Then:

1. Append `"vX.Y.Z"` to `VERSIONS` in the four test files above.
2. Run the four test targets (`cargo test -j 4 -p engine-storage --test upgrade_compat`, the same for
   `engine-connect` and `engine-driver-host`, and `cargo test -j 4 -p tablex-gpui app::upgrade_compat`).
3. If the release changed a format on purpose, make the older sets pass again with the old reading
   kept (an alias, a default) rather than by editing their fixtures. A value a *migration* rewrites on
   purpose goes into `MIGRATED` in `apps/tablex-gpui/src/app/upgrade_compat.rs`, with the version.
4. Add assertions for what the release added (new tables, new files) in the module of the release,
   and extend the data in the generators; the data only grows (keep every existing value, add new
   rows), because older sets are checked against the same expectations.
5. If the app database got a migration, `the_release_that_wrote_the_database_can_still_read_it...`
   states the downgrade rule: raising `min_reader_version` above the previous release's schema
   version makes that release refuse the file. Do it only on purpose and say so in the release notes.

### v0.1.10 was reconstructed, not generated

The generators did not exist at v0.1.10. `tablex.db` is made by `engine-storage` code that is
byte-identical to v0.1.10's (`git diff v0.1.10 -- rust-engine/crates/engine-storage` is empty). The
JSON files and the session were written by the generators of the release after it and then had
the fields removed that did not exist at v0.1.10 (`editor.vim_*` (8), `navigation.connection_double_click`,
`expand_on_connect`, `group_by_driver`, `restore_tree_state`, `show_table_stats`; in the sidebar
preferences `folder_filters` and `bookmarks`; in the session `tree`). The remaining key sets were
checked against `git show v0.1.10:<path>` for `Settings` and its five sections,
`SidebarPreferences`, `GpuiSession` and `SessionTab`. The driver store records were written by hand
from `InstalledDriverRecord` (camelCase) of v0.1.10. From the next release on, use the steps above
on the release commit.

### v0.1.9 was written by the release binary (settings, sidebar preferences, session only)

`v0.1.9/tablex/` is a small set, checked by one test of its own
(`the_files_of_v0_1_9_load_with_every_value_and_the_settings_added_since_read_as_defaults` in
`apps/tablex-gpui/src/app/upgrade_compat.rs`), not by the `VERSIONS` loops: it has no connections,
so the checks of the other test files that expect rows do not apply.

* `settings.json`: every setting of v0.1.9 away from its default, in v0.1.9's key set. It was made by
  starting the released `TableX.app` 0.1.9 with `TABLEX_DATA_DIR` (and a throwaway `HOME`) on a file
  filled from the v0.1.10 fixture minus `appearance.error_log_place`; the build read the file and
  wrote it back with the same keys (only the copy-format setting moved, from the old
  `grid-copy-default.json`), so the keys are the ones 0.1.9 itself writes.
* `sidebar-preferences.json`: the format did not change from 0.1.9 to 0.1.10, so this is the
  v0.1.10 file (no `folder_filters`, no `bookmarks`).
* `tablex.db`: created by the same 0.1.9 run (it moved a made-up `gpui-workspace-session.json` in);
  its only content is the session of two tabs. `VACUUM INTO` made it one file. The session text was
  then written out with every field of v0.1.9's `SessionTab` (the run had kept the short made-up
  file as it was), so a load and save leaves it as it is, as with a session 0.1.9 saved itself.
