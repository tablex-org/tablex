//! Backup and restore with the server's own client tools (R6): `pg_dump` and `pg_restore` for PostgreSQL,
//! `mysqldump` and `mariadb-dump` for MySQL and MariaDB. The tools run as a separate process; this module
//! knows nothing of windows.
//!
//! * `tools`: finding the programs on each operating system and telling their versions;
//! * `endpoint`: what a connection's URI says about reaching the server;
//! * `secrets`: the private files that carry the password (never a command line);
//! * `command`: the options and the command each becomes;
//! * `toc`: the archive kinds and the table of contents of a `pg_dump` archive;
//! * `process`: running the tool, its capped log, progress and cancel;
//! * `job`: one backup or restore from the tunnel to the finished file.

pub mod command;
pub mod endpoint;
pub mod job;
pub mod process;
pub mod secrets;
pub mod toc;
pub mod tools;
