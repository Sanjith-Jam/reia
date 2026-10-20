use crate::{Chat, Message, SourceScope};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params, params_from_iter};
use std::{collections::BTreeSet, path::Path};

const MAX_QUERY_CHARS: usize = 2048;
const MAX_QUERY_TERMS: usize = 16;
const MAX_RESULTS: usize = 100;

pub struct KnowledgeStore {
    db: Connection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeHit {
    pub text: String,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    Message {
        chat_id: String,
        message_id: String,
        timestamp: i64,
        sender_id: String,
        sender_name: String,
    },
    Note {
        note_id: String,
    },
}

impl KnowledgeStore {
    pub fn open(path: impl AsRef<Path>, key: &[u8]) -> Result<Self> {
        anyhow::ensure!(key.len() == 32, "knowledge database key must be 32 bytes");
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let key = key
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        db.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))
            .context("apply knowledge database key")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                chat_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                body TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                PRIMARY KEY (chat_id, message_id)
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(
                body, chat_id UNINDEXED, message_id UNINDEXED,
                timestamp UNINDEXED, sender_id UNINDEXED, sender_name UNINDEXED,
                tokenize='unicode61'
            );
            CREATE TABLE IF NOT EXISTS notes (
                note_id TEXT PRIMARY KEY,
                body TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS chats (
                chat_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                is_group INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS note_search USING fts5(
                body, note_id UNINDEXED, tokenize='unicode61'
            );",
        )
        .context("create local knowledge schema")?;
        Ok(Self { db })
    }

    pub fn record_chat(&mut self, chat: &Chat) -> Result<()> {
        self.db.execute(
            "INSERT INTO chats (chat_id, name, is_group) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET name = excluded.name, is_group = excluded.is_group",
            params![chat.id, chat.name, chat.is_group],
        )?;
        Ok(())
    }

    pub fn chats(&self) -> Result<Vec<Chat>> {
        let mut statement = self
            .db
            .prepare("SELECT chat_id, name, is_group FROM chats ORDER BY lower(name), chat_id")?;
        let chats = statement
            .query_map([], |row| {
                Ok(Chat {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    is_group: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(chats)
    }

    pub fn ingest(&mut self, message: &Message) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM message_search WHERE chat_id = ?1 AND message_id = ?2",
            params![message.chat_id, message.id],
        )?;
        tx.execute(
            "INSERT INTO messages (chat_id, message_id, body, timestamp, sender_id, sender_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(chat_id, message_id) DO UPDATE SET
               body = excluded.body, timestamp = excluded.timestamp,
               sender_id = excluded.sender_id, sender_name = excluded.sender_name",
            params![
                message.chat_id,
                message.id,
                message.text,
                message.timestamp,
                message.sender_id,
                message.sender_name,
            ],
        )?;
        tx.execute(
            "INSERT INTO message_search (body, chat_id, message_id, timestamp, sender_id, sender_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message.text,
                message.chat_id,
                message.id,
                message.timestamp,
                message.sender_id,
                message.sender_name,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete(&mut self, chat_id: &str, message_id: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM message_search WHERE chat_id = ?1 AND message_id = ?2",
            params![chat_id, message_id],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE chat_id = ?1 AND message_id = ?2",
            params![chat_id, message_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_note(&mut self, note_id: &str, text: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM note_search WHERE note_id = ?1", [note_id])?;
        tx.execute(
            "INSERT INTO notes (note_id, body) VALUES (?1, ?2)
             ON CONFLICT(note_id) DO UPDATE SET body = excluded.body",
            params![note_id, text],
        )?;
        tx.execute(
            "INSERT INTO note_search (body, note_id) VALUES (?1, ?2)",
            params![text, note_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn forget_note(&mut self, note_id: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM note_search WHERE note_id = ?1", [note_id])?;
        tx.execute("DELETE FROM notes WHERE note_id = ?1", [note_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn revoke_chats(&mut self, allowed: &BTreeSet<String>) -> Result<()> {
        let tx = self.db.transaction()?;
        if allowed.is_empty() {
            tx.execute("DELETE FROM message_search", [])?;
            tx.execute("DELETE FROM messages", [])?;
        } else {
            let marks = std::iter::repeat_n("?", allowed.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM message_search WHERE chat_id NOT IN ({marks})");
            tx.execute(&sql, params_from_iter(allowed.iter()))?;
            let sql = format!("DELETE FROM messages WHERE chat_id NOT IN ({marks})");
            tx.execute(&sql, params_from_iter(allowed.iter()))?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        scope: &SourceScope,
        limit: usize,
        include_notes: bool,
    ) -> Result<Vec<KnowledgeHit>> {
        let terms = search_terms(query)?;
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit = limit.min(MAX_RESULTS);
        let fts_query = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut hits = Vec::new();

        let (message_sql, selected): (String, Vec<String>) = match scope {
            SourceScope::All => (
                "SELECT body, chat_id, message_id, timestamp, sender_id, sender_name
                 FROM message_search WHERE message_search MATCH ?1
                 ORDER BY rank LIMIT ?2"
                    .into(),
                Vec::new(),
            ),
            SourceScope::Selected(chats) if chats.is_empty() => (String::new(), Vec::new()),
            SourceScope::Selected(chats) => {
                let selected = chats.iter().cloned().collect::<Vec<_>>();
                let placeholders = std::iter::repeat_n("?", selected.len())
                    .collect::<Vec<_>>()
                    .join(",");
                (
                    format!(
                        "SELECT body, chat_id, message_id, timestamp, sender_id, sender_name
                         FROM message_search WHERE message_search MATCH ?1
                         AND chat_id IN ({placeholders})
                         ORDER BY rank LIMIT ?{}",
                        selected.len() + 2
                    ),
                    selected,
                )
            }
        };
        if !message_sql.is_empty() {
            let mut statement = self.db.prepare(&message_sql)?;
            let mut values = vec![rusqlite::types::Value::Text(fts_query.clone())];
            values.extend(selected.into_iter().map(rusqlite::types::Value::Text));
            values.push(rusqlite::types::Value::Integer(limit as i64));
            let rows = statement.query_map(params_from_iter(values), |row| {
                Ok(KnowledgeHit {
                    text: row.get(0)?,
                    provenance: Provenance::Message {
                        chat_id: row.get(1)?,
                        message_id: row.get(2)?,
                        timestamp: row.get(3)?,
                        sender_id: row.get(4)?,
                        sender_name: row.get(5)?,
                    },
                })
            })?;
            hits.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }

        if include_notes && hits.len() < limit {
            let mut statement = self.db.prepare(
                "SELECT body, note_id FROM note_search WHERE note_search MATCH ?1
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows =
                statement.query_map(params![fts_query, (limit - hits.len()) as i64], |row| {
                    Ok(KnowledgeHit {
                        text: row.get(0)?,
                        provenance: Provenance::Note {
                            note_id: row.get(1)?,
                        },
                    })
                })?;
            hits.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(hits)
    }
}

fn search_terms(query: &str) -> Result<Vec<String>> {
    if query.len() > MAX_QUERY_CHARS {
        bail!("search query exceeds {MAX_QUERY_CHARS} bytes");
    }
    Ok(query
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '\''))
        .filter(|word| !word.is_empty())
        .take(MAX_QUERY_TERMS)
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{KnowledgeStore, Provenance, search_terms};
    use crate::{Chat, Message, SourceScope};
    use std::collections::BTreeSet;

    fn store() -> KnowledgeStore {
        KnowledgeStore::open(":memory:", &[7; 32]).unwrap()
    }

    #[test]
    fn empty_database_key_is_rejected() {
        assert!(KnowledgeStore::open(":memory:", &[]).is_err());
        assert!(KnowledgeStore::open(":memory:", b"short").is_err());
    }

    #[test]
    fn database_reopens_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.db");
        {
            let mut store = KnowledgeStore::open(&path, &[7; 32]).unwrap();
            store.add_note("n1", "local memory").unwrap();
        }
        assert!(KnowledgeStore::open(&path, &[8; 32]).is_err());
        let store = KnowledgeStore::open(&path, &[7; 32]).unwrap();
        assert_eq!(
            store
                .search("memory", &SourceScope::All, 10, true)
                .unwrap()
                .len(),
            1
        );
    }

    fn message(chat: &str, id: &str, text: &str) -> Message {
        Message {
            chat_id: chat.into(),
            id: id.into(),
            sender_id: "sender:1".into(),
            sender_name: "Ada".into(),
            timestamp: 123,
            text: text.into(),
            is_live: false,
        }
    }

    #[test]
    fn search_keeps_provenance_and_enforces_selected_scope() {
        let mut store = store();
        store
            .ingest(&message("group:1", "m1", "release notes are ready"))
            .unwrap();
        store
            .ingest(&message("group:2", "m2", "release notes are ready"))
            .unwrap();
        let scope = SourceScope::Selected(BTreeSet::from(["group:2".into()]));
        let hits = store.search("release notes", &scope, 10, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].provenance,
            Provenance::Message {
                chat_id: "group:2".into(),
                message_id: "m2".into(),
                timestamp: 123,
                sender_id: "sender:1".into(),
                sender_name: "Ada".into(),
            }
        );
    }

    #[test]
    fn edits_replace_old_searchable_text_and_deletions_remove_it() {
        let mut store = store();
        store
            .ingest(&message("chat:1", "m1", "old phrase"))
            .unwrap();
        store
            .ingest(&message("chat:1", "m1", "edited phrase"))
            .unwrap();
        assert!(
            store
                .search("old", &SourceScope::All, 10, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search("edited", &SourceScope::All, 10, false)
                .unwrap()
                .len(),
            1
        );
        store.delete("chat:1", "m1").unwrap();
        assert!(
            store
                .search("edited", &SourceScope::All, 10, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn revocation_purges_messages_but_keeps_notes() {
        let mut store = store();
        store
            .ingest(&message("group:1", "m1", "private phrase"))
            .unwrap();
        store.add_note("n1", "private phrase note").unwrap();
        store.revoke_chats(&BTreeSet::new()).unwrap();
        let private_chat = SourceScope::Selected(BTreeSet::from(["group:1".into()]));
        assert!(
            store
                .search("private", &private_chat, 10, false)
                .unwrap()
                .is_empty()
        );
        let hits = store
            .search("private", &SourceScope::All, 10, true)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].provenance,
            Provenance::Note {
                note_id: "n1".into()
            }
        );
    }

    #[test]
    fn note_edits_and_forgetting_update_the_index() {
        let mut store = store();
        store.add_note("n1", "remember the blue key").unwrap();
        store.add_note("n1", "remember the green key").unwrap();
        assert!(
            store
                .search("blue", &SourceScope::All, 10, true)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .search("green", &SourceScope::All, 10, true)
                .unwrap()
                .len(),
            1
        );
        store.forget_note("n1").unwrap();
        assert!(
            store
                .search("green", &SourceScope::All, 10, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_query_is_bounded_and_fts_operators_are_literal_terms() {
        assert!(search_terms(&"x".repeat(2049)).is_err());
        let terms = search_terms("OR \"message\"").unwrap();
        assert_eq!(terms, ["OR", "message"]);
    }

    #[test]
    fn chat_list_tracks_names_and_group_kind() {
        let mut store = store();
        store
            .record_chat(&Chat {
                id: "group:1".into(),
                name: "Team".into(),
                is_group: true,
            })
            .unwrap();
        store
            .record_chat(&Chat {
                id: "group:1".into(),
                name: "Renamed team".into(),
                is_group: true,
            })
            .unwrap();
        assert_eq!(
            store.chats().unwrap(),
            [Chat {
                id: "group:1".into(),
                name: "Renamed team".into(),
                is_group: true,
            }]
        );
    }
}
