use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref AI_CHAT_DB: Mutex<Option<Connection>> = Mutex::new(None);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIConversationScope {
    pub connection_uuid: String,
    pub connection_name: String,
    pub database_name: String,
    pub database_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIMessageRecord {
    pub id: String,
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    pub agent_steps_json: Option<String>,
    pub created_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIConversationRecord {
    pub id: String,
    pub connection_uuid: String,
    pub connection_name: String,
    pub database_name: String,
    pub database_type: String,
    pub title: String,
    pub mode: String,
    pub action: String,
    pub provider: String,
    pub model: String,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: f64,
    pub updated_at: f64,
    pub messages: Vec<AIMessageRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIConversationSummary {
    pub id: String,
    pub connection_uuid: String,
    pub connection_name: String,
    pub database_name: String,
    pub database_type: String,
    pub title: String,
    pub mode: String,
    pub action: String,
    pub provider: String,
    pub model: String,
    pub pinned: bool,
    pub archived: bool,
    pub message_count: i64,
    pub created_at: f64,
    pub updated_at: f64,
}

fn get_db_path() -> PathBuf {
    crate::paths::data_dir("TableLeaf").join("ai_chat.db")
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS ai_conversations (
             id TEXT PRIMARY KEY,
             connection_uuid TEXT NOT NULL DEFAULT '',
             connection_name TEXT NOT NULL DEFAULT '',
             database_name TEXT NOT NULL DEFAULT '',
             database_type TEXT NOT NULL DEFAULT '',
             title TEXT NOT NULL,
             mode TEXT NOT NULL,
             action TEXT NOT NULL,
             provider TEXT NOT NULL DEFAULT '',
             model TEXT NOT NULL DEFAULT '',
             pinned INTEGER NOT NULL DEFAULT 0,
             archived INTEGER NOT NULL DEFAULT 0,
             created_at REAL NOT NULL,
             updated_at REAL NOT NULL
         );
         CREATE TABLE IF NOT EXISTS ai_messages (
             id TEXT PRIMARY KEY,
             conversation_id TEXT NOT NULL,
             role TEXT NOT NULL,
             content TEXT NOT NULL,
             reasoning TEXT,
             agent_steps_json TEXT,
             ordinal INTEGER NOT NULL,
             created_at REAL NOT NULL,
             FOREIGN KEY(conversation_id) REFERENCES ai_conversations(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_ai_conversations_scope_updated
             ON ai_conversations(connection_uuid, database_name, updated_at DESC);
         CREATE INDEX IF NOT EXISTS idx_ai_messages_conversation_ordinal
             ON ai_messages(conversation_id, ordinal);",
    )?;
    Ok(())
}

fn ensure_db() -> Result<()> {
    let mut db = AI_CHAT_DB.lock().expect("ai chat db lock poisoned");
    if db.is_none() {
        let path = get_db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        }
        let conn = Connection::open(&path)?;
        create_schema(&conn)?;
        *db = Some(conn);
    }
    Ok(())
}

pub fn upsert_conversation(conversation: &AIConversationRecord) -> Result<()> {
    ensure_db()?;
    let mut db = AI_CHAT_DB.lock().expect("ai chat db lock poisoned");
    let conn = db.as_mut().expect("ai chat db initialized");
    upsert_conversation_impl(conn, conversation)
}

fn upsert_conversation_impl(
    conn: &mut Connection,
    conversation: &AIConversationRecord,
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO ai_conversations
         (id, connection_uuid, connection_name, database_name, database_type, title, mode, action,
          provider, model, pinned, archived, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
             connection_uuid = excluded.connection_uuid,
             connection_name = excluded.connection_name,
             database_name = excluded.database_name,
             database_type = excluded.database_type,
             title = excluded.title,
             mode = excluded.mode,
             action = excluded.action,
             provider = excluded.provider,
             model = excluded.model,
             pinned = excluded.pinned,
             archived = excluded.archived,
             updated_at = excluded.updated_at",
        params![
            conversation.id,
            conversation.connection_uuid,
            conversation.connection_name,
            conversation.database_name,
            conversation.database_type,
            conversation.title,
            conversation.mode,
            conversation.action,
            conversation.provider,
            conversation.model,
            conversation.pinned as i64,
            conversation.archived as i64,
            conversation.created_at,
            conversation.updated_at,
        ],
    )?;
    tx.execute(
        "DELETE FROM ai_messages WHERE conversation_id = ?1",
        params![conversation.id],
    )?;
    for (index, message) in conversation.messages.iter().enumerate() {
        tx.execute(
            "INSERT INTO ai_messages
             (id, conversation_id, role, content, reasoning, agent_steps_json, ordinal, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message.id,
                conversation.id,
                message.role,
                message.content,
                message.reasoning,
                message.agent_steps_json,
                index as i64,
                message.created_at,
            ],
        )?;
    }
    tx.commit()
}

pub fn list_conversations(
    scope: &AIConversationScope,
    limit: i64,
) -> Result<Vec<AIConversationSummary>> {
    ensure_db()?;
    let db = AI_CHAT_DB.lock().expect("ai chat db lock poisoned");
    let conn = db.as_ref().expect("ai chat db initialized");
    list_conversations_impl(conn, scope, limit)
}

fn list_conversations_impl(
    conn: &Connection,
    scope: &AIConversationScope,
    limit: i64,
) -> Result<Vec<AIConversationSummary>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.connection_uuid, c.connection_name, c.database_name, c.database_type,
                c.title, c.mode, c.action, c.provider, c.model, c.pinned, c.archived,
                COUNT(m.id) AS message_count, c.created_at, c.updated_at
         FROM ai_conversations c
         LEFT JOIN ai_messages m ON m.conversation_id = c.id
         WHERE c.connection_uuid = ?1 AND c.database_name = ?2 AND c.archived = 0
         GROUP BY c.id
         ORDER BY c.pinned DESC, c.updated_at DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![scope.connection_uuid, scope.database_name, limit],
        |row| {
            Ok(AIConversationSummary {
                id: row.get(0)?,
                connection_uuid: row.get(1)?,
                connection_name: row.get(2)?,
                database_name: row.get(3)?,
                database_type: row.get(4)?,
                title: row.get(5)?,
                mode: row.get(6)?,
                action: row.get(7)?,
                provider: row.get(8)?,
                model: row.get(9)?,
                pinned: row.get::<_, i64>(10)? != 0,
                archived: row.get::<_, i64>(11)? != 0,
                message_count: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
            })
        },
    )?;
    rows.collect()
}

pub fn get_conversation(id: &str) -> Result<Option<AIConversationRecord>> {
    ensure_db()?;
    let db = AI_CHAT_DB.lock().expect("ai chat db lock poisoned");
    let conn = db.as_ref().expect("ai chat db initialized");
    get_conversation_impl(conn, id)
}

fn get_conversation_impl(conn: &Connection, id: &str) -> Result<Option<AIConversationRecord>> {
    let mut conversation = conn
        .query_row(
            "SELECT id, connection_uuid, connection_name, database_name, database_type,
                    title, mode, action, provider, model, pinned, archived, created_at, updated_at
             FROM ai_conversations WHERE id = ?1",
            params![id],
            |row| {
                Ok(AIConversationRecord {
                    id: row.get(0)?,
                    connection_uuid: row.get(1)?,
                    connection_name: row.get(2)?,
                    database_name: row.get(3)?,
                    database_type: row.get(4)?,
                    title: row.get(5)?,
                    mode: row.get(6)?,
                    action: row.get(7)?,
                    provider: row.get(8)?,
                    model: row.get(9)?,
                    pinned: row.get::<_, i64>(10)? != 0,
                    archived: row.get::<_, i64>(11)? != 0,
                    created_at: row.get(12)?,
                    updated_at: row.get(13)?,
                    messages: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(conversation) = conversation.as_mut() else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT id, role, content, reasoning, agent_steps_json, created_at
         FROM ai_messages WHERE conversation_id = ?1 ORDER BY ordinal ASC",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok(AIMessageRecord {
            id: row.get(0)?,
            role: row.get(1)?,
            content: row.get(2)?,
            reasoning: row.get(3)?,
            agent_steps_json: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    conversation.messages = rows.collect::<Result<Vec<_>>>()?;
    Ok(Some(conversation.clone()))
}

pub fn delete_conversation(id: &str) -> Result<()> {
    ensure_db()?;
    let db = AI_CHAT_DB.lock().expect("ai chat db lock poisoned");
    let conn = db.as_ref().expect("ai chat db initialized");
    delete_conversation_impl(conn, id)
}

fn delete_conversation_impl(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM ai_conversations WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_list_get_and_delete_conversation() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let conversation = AIConversationRecord {
            id: "c1".into(),
            connection_uuid: "conn-a".into(),
            connection_name: "Local".into(),
            database_name: "app".into(),
            database_type: "mongodb".into(),
            title: "Find bills".into(),
            mode: "ask".into(),
            action: "generate".into(),
            provider: "openAI".into(),
            model: "gpt-4o".into(),
            pinned: false,
            archived: false,
            created_at: 1.0,
            updated_at: 2.0,
            messages: vec![AIMessageRecord {
                id: "m1".into(),
                role: "user".into(),
                content: "hello".into(),
                reasoning: None,
                agent_steps_json: None,
                created_at: 1.5,
            }],
        };
        let mut conn = conn;
        upsert_conversation_impl(&mut conn, &conversation).unwrap();
        let scope = AIConversationScope {
            connection_uuid: "conn-a".into(),
            connection_name: "Local".into(),
            database_name: "app".into(),
            database_type: "mongodb".into(),
        };
        let summaries = list_conversations_impl(&conn, &scope, 20).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].message_count, 1);
        let loaded = get_conversation_impl(&conn, "c1").unwrap().unwrap();
        assert_eq!(loaded.messages[0].content, "hello");
        delete_conversation_impl(&conn, "c1").unwrap();
        assert!(get_conversation_impl(&conn, "c1").unwrap().is_none());
    }
}
