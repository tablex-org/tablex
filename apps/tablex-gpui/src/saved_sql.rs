use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_SQL_BYTES: u64 = 4_194_304;
const MAX_METADATA_BYTES: u64 = 8_388_608;
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn no_symlinks(root: &Path, relative: &Path) -> Result<(), String> {
    let mut path = root.to_path_buf();
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err("SQL project path is a symlink".into());
    }
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err("Invalid SQL script path".into());
        };
        path.push(part);
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err("SQL script path is a symlink".into());
        }
    }
    Ok(())
}

fn bounded_read(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|_| "Could not read SQL script file")?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read SQL script file")?;
    if bytes.len() as u64 > max {
        return Err("SQL script file is too large".into());
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedSqlEntry {
    pub path: PathBuf,
    pub title: String,
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
    pub dialect: String,
    pub updated_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Binding {
    #[serde(rename = "connectionUUID")]
    connection_uuid: String,
    database: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dialect: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: f64,
    #[serde(rename = "updatedAt")]
    updated_at: f64,
}

fn metadata_path(root: &Path) -> PathBuf {
    root.join(".tablex/resource-metadata.json")
}

fn metadata(root: &Path) -> Result<HashMap<String, serde_json::Value>, String> {
    no_symlinks(root, Path::new(".tablex/resource-metadata.json"))?;
    match bounded_read(&metadata_path(root), MAX_METADATA_BYTES) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "Invalid SQL script metadata".into())
        }
        Err(_) if !metadata_path(root).exists() => Ok(HashMap::new()),
        Err(_) => Err("Could not read SQL script metadata".into()),
    }
}

pub fn save_sql(
    root: &Path,
    connection_id: &str,
    database: &str,
    name: &str,
    sql: &str,
    now: f64,
) -> Result<SavedSqlEntry, String> {
    save_with_dialect(root, connection_id, database, name, sql, now, "sql", "sql")
}

pub fn save_script(
    root: &Path,
    connection_id: &str,
    database: &str,
    name: &str,
    script: &str,
    now: f64,
) -> Result<SavedSqlEntry, String> {
    save_with_dialect(
        root,
        connection_id,
        database,
        name,
        script,
        now,
        crate::query_mode::QueryMode::MongoScript.as_str(),
        "js",
    )
}

#[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
fn save_with_dialect(
    root: &Path,
    connection_id: &str,
    database: &str,
    name: &str,
    sql: &str,
    now: f64,
    dialect: &str,
    extension: &str,
) -> Result<SavedSqlEntry, String> {
    let _guard = SAVE_LOCK
        .lock()
        .map_err(|_| "SQL script store unavailable")?;
    let id = Uuid::parse_str(connection_id).map_err(|_| "Invalid connection id")?;
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', ':', '\\'])
        || !now.is_finite()
    {
        return Err("Invalid SQL script name or timestamp".into());
    }
    let suffix = format!(".{extension}");
    let title = name.strip_suffix(&suffix).unwrap_or(name);
    if title.is_empty() {
        return Err("Invalid SQL script name".into());
    }
    let filename = format!("{title}.{extension}");
    let relative = format!("Scripts/{filename}");
    let path = root.join(&relative);
    no_symlinks(root, Path::new(&relative))?;
    if sql.len() as u64 > MAX_SQL_BYTES {
        return Err("SQL script is too large".into());
    }
    let mut all = metadata(root)?;
    if all.contains_key(&relative) {
        return Err("SQL script already exists".into());
    }
    std::fs::create_dir_all(root.join("Scripts"))
        .map_err(|_| "Could not create SQL script folder")?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|_| "Could not create SQL script (it may already exist)")?;
    if file
        .write_all(sql.as_bytes())
        .and_then(|_| file.sync_all())
        .is_err()
    {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err("Could not write SQL script".into());
    }
    drop(file);
    let binding = Binding {
        connection_uuid: id.to_string().to_uppercase(),
        database: database.into(),
        schema: None,
        dialect: (dialect != "sql").then(|| dialect.into()),
        created_at: now,
        updated_at: now,
    };
    all.insert(
        relative,
        serde_json::to_value(&binding).map_err(|_| "Could not encode SQL script metadata")?,
    );
    let meta_path = metadata_path(root);
    let result = (|| -> Result<(), String> {
        std::fs::create_dir_all(meta_path.parent().ok_or("Invalid metadata path")?)
            .map_err(|_| "Could not create SQL script metadata folder")?;
        let temp = meta_path.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let bytes =
            serde_json::to_vec_pretty(&all).map_err(|_| "Could not encode SQL script metadata")?;
        let write = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &meta_path)
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(temp);
            return Err("Could not save SQL script metadata".into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result?;
    Ok(SavedSqlEntry {
        path,
        title: title.into(),
        connection_id: id.to_string(),
        database: database.into(),
        schema: None,
        dialect: dialect.into(),
        updated_at: now,
    })
}

pub fn default_projects_base() -> Option<PathBuf> {
    crate::platform::app_data_dir().map(|dir| dir.join("Projects"))
}

/// Project roots include General and, in production, registered custom Swift projects.
pub fn project_roots_at(base: &Path, include_registered: bool) -> Result<Vec<PathBuf>, String> {
    let mut roots = vec![base.join("General")];
    if include_registered {
        for project in engine_storage::connection::list_projects()
            .map_err(|_| "Could not list SQL script projects")?
        {
            if project.id != "General" {
                roots.push(if project.uses_default_location {
                    base.join(project.id)
                } else {
                    PathBuf::from(project.path)
                });
            }
        }
    }
    if base.exists() {
        for item in std::fs::read_dir(base).map_err(|_| "Could not list SQL script projects")? {
            let item = item.map_err(|_| "Could not list SQL script projects")?;
            if item.file_type().is_ok_and(|kind| kind.is_dir()) && !roots.contains(&item.path()) {
                roots.push(item.path());
            }
        }
    }
    Ok(roots)
}

pub fn recent_sql(roots: &[PathBuf], connection_id: &str) -> Result<Vec<SavedSqlEntry>, String> {
    recent_for_dialect(roots, connection_id, "sql", "sql")
}

pub fn recent_scripts(
    roots: &[PathBuf],
    connection_id: &str,
) -> Result<Vec<SavedSqlEntry>, String> {
    recent_for_dialect(
        roots,
        connection_id,
        crate::query_mode::QueryMode::MongoScript.as_str(),
        "js",
    )
}

fn recent_for_dialect(
    roots: &[PathBuf],
    connection_id: &str,
    dialect: &str,
    extension: &str,
) -> Result<Vec<SavedSqlEntry>, String> {
    let id = Uuid::parse_str(connection_id).map_err(|_| "Invalid connection id")?;
    let mut entries = Vec::new();
    for root in roots {
        for (relative, value) in metadata(root)? {
            let Ok(binding) = serde_json::from_value::<Binding>(value) else {
                continue;
            };
            if Uuid::parse_str(&binding.connection_uuid).ok() != Some(id)
                || !binding.updated_at.is_finite()
            {
                continue;
            }
            let path = Path::new(&relative);
            if !relative.starts_with("Scripts/")
                || path
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
                || path.extension().and_then(|ext| ext.to_str()) != Some(extension)
                || binding.dialect.as_deref().unwrap_or("sql") != dialect
            {
                continue;
            }
            let file = root.join(path);
            if no_symlinks(root, path).is_err() {
                continue;
            }
            if !std::fs::symlink_metadata(&file)
                .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
            {
                continue;
            }
            let title = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            entries.push(SavedSqlEntry {
                path: file,
                title: title.into(),
                connection_id: id.to_string(),
                database: binding.database,
                schema: binding.schema,
                dialect: dialect.into(),
                updated_at: binding.updated_at,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.updated_at
            .total_cmp(&a.updated_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    entries.truncate(10);
    Ok(entries)
}

pub fn open_sql(entry: &SavedSqlEntry) -> Result<String, String> {
    let scripts = entry
        .path
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "Scripts"))
        .ok_or("Invalid SQL script path")?;
    let root = scripts.parent().ok_or("Invalid SQL script path")?;
    let relative = entry
        .path
        .strip_prefix(root)
        .map_err(|_| "Invalid SQL script path")?;
    no_symlinks(root, relative)?;
    if std::fs::symlink_metadata(&entry.path)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
    {
        String::from_utf8(bounded_read(&entry.path, MAX_SQL_BYTES)?)
            .map_err(|_| "Saved SQL script is not UTF-8".into())
    } else {
        Err("Saved SQL script is unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn saved_sql_round_trips_through_swift_project_layout_and_scopes_recent_ten() {
        let root = std::env::temp_dir().join(format!("tablex-saved-sql-{}", Uuid::new_v4()));
        let other = root.join("Other");
        let general = root.join("General");
        std::fs::create_dir_all(&other).unwrap();
        for i in 0..12 {
            save_sql(
                &general,
                "11111111-1111-1111-1111-111111111111",
                "app",
                &format!("Query {i}"),
                &format!("SELECT {i}"),
                i as f64,
            )
            .unwrap();
        }
        save_sql(
            &other,
            "22222222-2222-2222-2222-222222222222",
            "other",
            "Hidden",
            "SELECT 99",
            100.0,
        )
        .unwrap();
        let recent = recent_sql(
            &[general.clone(), other],
            "11111111-1111-1111-1111-111111111111",
        )
        .unwrap();
        assert_eq!(recent.len(), 10);
        assert_eq!(recent[0].title, "Query 11");
        assert_eq!(recent[9].title, "Query 2");
        assert_eq!(open_sql(&recent[0]).unwrap(), "SELECT 11");
        assert!(
            std::fs::read_to_string(general.join("Scripts/Query 11.sql"))
                .unwrap()
                .contains("SELECT 11")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_sql_rejects_overwrite_and_bad_names_and_open_read_errors() {
        let root = std::env::temp_dir().join(format!("tablex-saved-sql-{}", Uuid::new_v4()));
        let conn = "11111111-1111-1111-1111-111111111111";
        assert!(save_sql(&root, conn, "", "../escape", "SELECT 1", 1.0).is_err());
        let entry = save_sql(&root, conn, "", "Safe", "SELECT 1", 1.0).unwrap();
        assert!(save_sql(&root, conn, "", "Safe", "SELECT 2", 2.0).is_err());
        assert_eq!(open_sql(&entry).unwrap(), "SELECT 1");
        std::fs::remove_file(&entry.path).unwrap();
        assert!(open_sql(&entry).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_sql_rejects_symlinked_script_folder_and_oversized_contents() {
        let root = std::env::temp_dir().join(format!("tablex-saved-sql-{}", Uuid::new_v4()));
        let outside =
            std::env::temp_dir().join(format!("tablex-saved-sql-outside-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("Scripts")).unwrap();
        assert!(save_sql(
            &root,
            "11111111-1111-1111-1111-111111111111",
            "",
            "unsafe",
            "SELECT 1",
            1.0
        )
        .is_err());
        assert!(!outside.join("unsafe.sql").exists());
        std::fs::remove_file(root.join("Scripts")).unwrap();
        let entry = save_sql(
            &root,
            "11111111-1111-1111-1111-111111111111",
            "",
            "safe",
            "SELECT 1",
            1.0,
        )
        .unwrap();
        std::fs::write(&entry.path, vec![b'x'; 4_194_305]).unwrap();
        assert!(open_sql(&entry).is_err());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn reads_existing_swift_metadata_and_nested_scripts_without_executing_sql() {
        let root = std::env::temp_dir().join(format!("tablex-saved-sql-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("Scripts/Nested")).unwrap();
        std::fs::create_dir_all(root.join(".tablex")).unwrap();
        std::fs::write(root.join("Scripts/Nested/legacy.sql"), "DROP DATABASE app;").unwrap();
        std::fs::write(root.join(".tablex/resource-metadata.json"),
            r#"{"Scripts/Nested/legacy.sql":{"connectionUUID":"11111111-1111-1111-1111-111111111111","database":"app","schema":"public","createdAt":1.0,"updatedAt":2.0}}"#).unwrap();
        let recent = recent_sql(std::slice::from_ref(&root), "11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(recent[0].database, "app");
        assert_eq!(recent[0].schema.as_deref(), Some("public"));
        assert_eq!(open_sql(&recent[0]).unwrap(), "DROP DATABASE app;");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_project_base_does_not_require_real_home_or_registered_storage() {
        let base = std::env::temp_dir().join(format!("tablex-sql-base-{}", Uuid::new_v4()));
        let roots = project_roots_at(&base, false).unwrap();
        assert_eq!(roots, [base.join("General")]);
    }

    #[test]
    fn mongo_script_uses_typed_js_file_without_becoming_sql_history() {
        let root = std::env::temp_dir().join(format!("tablex-mongo-script-{}", Uuid::new_v4()));
        let id = "11111111-1111-1111-1111-111111111111";
        let entry = save_script(
            &root,
            id,
            "warehouse",
            "Read orders",
            "db.orders.find({})",
            12.0,
        )
        .unwrap();
        assert_eq!(
            entry.path.extension().and_then(|ext| ext.to_str()),
            Some("js")
        );
        assert_eq!(entry.dialect, "mongoScript");
        assert_eq!(open_sql(&entry).unwrap(), "db.orders.find({})");
        assert!(recent_sql(std::slice::from_ref(&root), id).unwrap().is_empty());
        assert_eq!(recent_scripts(std::slice::from_ref(&root), id).unwrap(), [entry]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
