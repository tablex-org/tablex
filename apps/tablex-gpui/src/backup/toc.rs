//! A `pg_dump` archive: which kind it is, and its table of contents as `pg_restore --list` prints it, so the
//! user can choose what is restored (`pg_restore --use-list`).

use std::collections::HashSet;
use std::io::Read as _;
use std::path::Path;

/// What a file chosen for a restore is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `pg_dump --format=custom` (starts with `PGDMP`).
    Custom,
    /// `pg_dump --format=tar`.
    Tar,
    /// `pg_dump --format=directory` (a directory with a `toc.dat`).
    Directory,
    /// An SQL script: not for `pg_restore`.
    Plain,
}

impl ArchiveKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Custom => "custom archive",
            Self::Tar => "tar archive",
            Self::Directory => "directory archive",
            Self::Plain => "SQL script",
        }
    }
}

/// Looks at `path`: the first bytes of a file, the `toc.dat` of a directory.
pub fn detect_archive(path: &Path) -> std::io::Result<ArchiveKind> {
    if path.is_dir() {
        return if path.join("toc.dat").is_file() {
            Ok(ArchiveKind::Directory)
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "this folder is not a pg_dump directory archive: it has no toc.dat"))
        };
    }
    let mut head = Vec::with_capacity(512);
    std::fs::File::open(path)?.take(512).read_to_end(&mut head)?;
    Ok(if head.starts_with(b"PGDMP") {
        ArchiveKind::Custom
    } else if head.get(257..262) == Some(b"ustar".as_slice()) {
        ArchiveKind::Tar
    } else {
        ArchiveKind::Plain
    })
}

/// The kinds of object a table of contents names, longest first where one is the start of another.
const DESCRIPTIONS: &[&str] = &[
    "TEXT SEARCH CONFIGURATION",
    "TEXT SEARCH DICTIONARY",
    "TEXT SEARCH TEMPLATE",
    "TEXT SEARCH PARSER",
    "PUBLICATION TABLES IN SCHEMA",
    "REFRESH MATERIALIZED VIEW",
    "MATERIALIZED VIEW DATA",
    "FOREIGN DATA WRAPPER",
    "PROCEDURAL LANGUAGE",
    "SEQUENCE OWNED BY",
    "MATERIALIZED VIEW",
    "PUBLICATION TABLE",
    "OPERATOR FAMILY",
    "OPERATOR CLASS",
    "CHECK CONSTRAINT",
    "FK CONSTRAINT",
    "EVENT TRIGGER",
    "FOREIGN TABLE",
    "ACCESS METHOD",
    "SEQUENCE SET",
    "ROW SECURITY",
    "LARGE OBJECT",
    "DEFAULT ACL",
    "USER MAPPING",
    "INDEX ATTACH",
    "TABLE ATTACH",
    "SHELL TYPE",
    "TABLE DATA",
    "CONSTRAINT",
    "COLLATION",
    "CONVERSION",
    "STATISTICS",
    "SUBSCRIPTION",
    "TRANSFORM",
    "AGGREGATE",
    "PUBLICATION",
    "SEARCHPATH",
    "STDSTRINGS",
    "ENCODING",
    "EXTENSION",
    "SEQUENCE",
    "FUNCTION",
    "PROCEDURE",
    "OPERATOR",
    "DATABASE",
    "DEFAULT",
    "COMMENT",
    "TRIGGER",
    "SCHEMA",
    "SERVER",
    "DOMAIN",
    "POLICY",
    "INDEX",
    "TABLE",
    "BLOBS",
    "BLOB",
    "CAST",
    "RULE",
    "TYPE",
    "VIEW",
    "ACL",
];

/// One object of the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    /// The archive's own number for it; what `--use-list` is keyed on.
    pub id: u32,
    pub description: String,
    /// Empty when the object has none.
    pub schema: String,
    pub name: String,
    pub owner: String,
}

impl TocEntry {
    /// `TABLE DATA public.items`.
    pub fn label(&self) -> String {
        match (self.schema.is_empty(), self.name.is_empty()) {
            (true, true) => self.description.clone(),
            (true, false) => format!("{} {}", self.description, self.name),
            (false, true) => format!("{} {}", self.description, self.schema),
            (false, false) => format!("{} {}.{}", self.description, self.schema, self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// A `;` line: kept as it is.
    Other(String),
    Entry(usize, String),
}

/// The table of contents of an archive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Toc {
    pub entries: Vec<TocEntry>,
    lines: Vec<Line>,
}

/// An entry line is `<id>; <catalog oid> <object oid> <DESCRIPTION> [<schema>] <name> <owner>`.
fn parse_entry(line: &str) -> Option<TocEntry> {
    let (id, rest) = line.split_once(';')?;
    let id: u32 = id.trim().parse().ok()?;
    let mut tokens = rest.split_whitespace();
    tokens.next()?.parse::<u64>().ok()?;
    tokens.next()?.parse::<u64>().ok()?;
    let remainder = tokens.collect::<Vec<_>>().join(" ");
    let description = DESCRIPTIONS
        .iter()
        .find(|description| remainder.strip_prefix(**description).is_some_and(|after| after.is_empty() || after.starts_with(' ')))
        .map_or_else(|| remainder.split(' ').next().unwrap_or_default(), |description| *description);
    let after = remainder[description.len()..].trim_start();
    let mut words: Vec<&str> = after.split(' ').filter(|word| !word.is_empty()).collect();
    let owner = if words.len() >= 2 { words.pop().unwrap_or_default() } else { "" };
    let schema = if words.len() >= 2 { words.remove(0) } else { "" };
    Some(TocEntry {
        id,
        description: description.to_owned(),
        schema: if schema == "-" { String::new() } else { schema.to_owned() },
        name: if words.len() == 1 && words[0] == "-" { String::new() } else { words.join(" ") },
        owner: owner.to_owned(),
    })
}

impl Toc {
    /// Reads the output of `pg_restore --list`.
    pub fn parse(listing: &str) -> Self {
        let mut toc = Self::default();
        for line in listing.lines() {
            let entry = if line.starts_with(';') || line.trim().is_empty() { None } else { parse_entry(line) };
            match entry {
                Some(entry) => {
                    toc.lines.push(Line::Entry(toc.entries.len(), line.to_owned()));
                    toc.entries.push(entry);
                }
                None => toc.lines.push(Line::Other(line.to_owned())),
            }
        }
        toc
    }

    /// The listing with every entry not in `wanted` commented out: the file for `pg_restore --use-list`.
    pub fn use_list(&self, wanted: &HashSet<u32>) -> String {
        let mut text = String::new();
        for line in &self.lines {
            match line {
                Line::Other(other) => text.push_str(other),
                Line::Entry(index, original) if wanted.contains(&self.entries[*index].id) => text.push_str(original),
                Line::Entry(_, original) => {
                    text.push(';');
                    text.push_str(original);
                }
            }
            text.push('\n');
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = ";\n; Archive created at 2026-10-26 10:00:00 CEST\n;     dbname: tablex\n;     TOC Entries: 6\n;\n;\n; Selected TOC Entries:\n;\n10; 2615 2200 SCHEMA - public tablex\n218; 1259 16385 TABLE public items tablex\n3352; 0 16385 TABLE DATA public items tablex\n3201; 2606 16390 CONSTRAINT public items items_pkey tablex\n77; 1255 16400 FUNCTION public add(integer, integer) tablex\n5; 0 0 ENCODING - ENCODING \n6; 0 0 SEQUENCE OWNED BY public items_id_seq tablex\n";

    #[test]
    fn the_entries_of_a_listing_are_read_with_their_kind_schema_name_and_owner() {
        let toc = Toc::parse(LISTING);
        let seen: Vec<(u32, &str, &str, &str, &str)> = toc.entries.iter().map(|e| (e.id, e.description.as_str(), e.schema.as_str(), e.name.as_str(), e.owner.as_str())).collect();
        assert_eq!(
            seen,
            [
                (10, "SCHEMA", "", "public", "tablex"),
                (218, "TABLE", "public", "items", "tablex"),
                (3352, "TABLE DATA", "public", "items", "tablex"),
                (3201, "CONSTRAINT", "public", "items items_pkey", "tablex"),
                (77, "FUNCTION", "public", "add(integer, integer)", "tablex"),
                (5, "ENCODING", "", "", "ENCODING"),
                (6, "SEQUENCE OWNED BY", "public", "items_id_seq", "tablex"),
            ]
        );
        assert_eq!(toc.entries[2].label(), "TABLE DATA public.items");
        assert_eq!(toc.entries[0].label(), "SCHEMA public");
    }

    #[test]
    fn the_use_list_keeps_the_wanted_entries_and_comments_out_the_rest() {
        let toc = Toc::parse(LISTING);
        let list = toc.use_list(&HashSet::from([218, 3352]));
        assert!(list.contains("\n218; 1259 16385 TABLE public items tablex\n"));
        assert!(list.contains("\n3352; 0 16385 TABLE DATA public items tablex\n"));
        assert!(list.contains("\n;10; 2615 2200 SCHEMA - public tablex\n"));
        assert!(list.contains("\n;77; 1255 16400 FUNCTION public add(integer, integer) tablex\n"));
        assert!(list.starts_with(";\n; Archive created at"), "the header is kept");
        // Read back, the commented lines are comments and the wanted ones are the entries.
        let again = Toc::parse(&list);
        assert_eq!(again.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [218, 3352]);
    }

    #[test]
    fn archives_are_told_apart_by_what_they_start_with() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("a.dump");
        std::fs::write(&custom, b"PGDMP\x01\x0e\x00").unwrap();
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        let tar_path = dir.path().join("a.tar");
        std::fs::write(&tar_path, tar).unwrap();
        let plain = dir.path().join("a.sql");
        std::fs::write(&plain, "-- PostgreSQL database dump\nCREATE TABLE t();\n").unwrap();
        let archive_dir = dir.path().join("d");
        std::fs::create_dir(&archive_dir).unwrap();
        assert!(detect_archive(&archive_dir).is_err(), "no toc.dat");
        std::fs::write(archive_dir.join("toc.dat"), b"PGDMP").unwrap();
        assert_eq!(
            [&custom, &tar_path, &plain, &archive_dir].map(|path| detect_archive(path).unwrap()),
            [ArchiveKind::Custom, ArchiveKind::Tar, ArchiveKind::Plain, ArchiveKind::Directory]
        );
        assert!(detect_archive(&dir.path().join("absent")).is_err());
    }
}
