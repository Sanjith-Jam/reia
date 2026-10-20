use crate::{
    PollDraft,
    whatsapp::{CreatedPoll, PollUpdate, WhatsApp, poll_choices},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollStatus {
    pub chat_id: String,
    pub poll_id: String,
    pub question: String,
    pub options: Vec<String>,
    pub selected_voters: Vec<usize>,
}

pub struct TodoStore {
    db: Connection,
}

struct StoredPoll {
    options: Vec<String>,
    secret: Vec<u8>,
    creator_jid: String,
}

impl TodoStore {
    pub fn open(path: impl AsRef<Path>, key: &[u8]) -> Result<Self> {
        anyhow::ensure!(key.len() == 32, "invalid local database key");
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(&format!("PRAGMA key = \"x'{}'\";", hex::encode(key)))
            .context("unlock encrypted todo database")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS polls (
                poll_id TEXT PRIMARY KEY,
                chat_id TEXT NOT NULL,
                question TEXT NOT NULL,
                options TEXT NOT NULL,
                secret BLOB NOT NULL,
                creator_jid TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS poll_votes (
                poll_id TEXT NOT NULL,
                voter_id TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                update_id TEXT NOT NULL,
                choices TEXT NOT NULL,
                PRIMARY KEY (poll_id, voter_id),
                UNIQUE (update_id)
            );",
        )
        .context("create encrypted todo schema")?;
        Ok(Self { db })
    }

    pub fn record_poll(
        &mut self,
        chat_id: &str,
        draft: &PollDraft,
        created: &CreatedPoll,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO polls (poll_id, chat_id, question, options, secret, creator_jid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                created.message_id,
                chat_id,
                draft.question,
                serde_json::to_string(&draft.options)?,
                created.secret,
                created.creator_jid,
            ],
        )?;
        Ok(())
    }

    pub async fn apply_vote(&mut self, whatsapp: &WhatsApp, update: &PollUpdate) -> Result<bool> {
        let Some(poll) = self.poll_details(&update.poll_id)? else {
            return Ok(false);
        };
        let selected = whatsapp
            .decrypt_poll_vote(update, &poll.secret, &poll.creator_jid)
            .await
            .context("decode todo poll vote")?;
        let Some(choices) = poll_choices(&poll.options, &selected) else {
            return Ok(false);
        };
        self.save_vote(update, &choices)
    }

    pub fn latest(&self, chat_id: &str) -> Result<Vec<PollStatus>> {
        let mut statement = self.db.prepare(
            "SELECT poll_id, question, options, creator_jid FROM polls
             WHERE chat_id = ?1 ORDER BY rowid DESC LIMIT 10",
        )?;
        let polls = statement
            .query_map([chat_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        polls
            .into_iter()
            .map(|(poll_id, question, options_json, creator_jid)| {
                let options: Vec<String> = serde_json::from_str(&options_json)?;
                let mut counts = vec![0; options.len()];
                let mut votes = self.db.prepare(
                    "SELECT choices FROM poll_votes WHERE poll_id = ?1 AND voter_id = ?2",
                )?;
                for choices in votes.query_map(params![&poll_id, creator_jid], |row| {
                    row.get::<_, String>(0)
                })? {
                    for choice in serde_json::from_str::<Vec<usize>>(&choices?)? {
                        if let Some(count) = counts.get_mut(choice) {
                            *count += 1;
                        }
                    }
                }
                Ok(PollStatus {
                    chat_id: chat_id.to_owned(),
                    poll_id,
                    question,
                    options,
                    selected_voters: counts,
                })
            })
            .collect()
    }

    fn poll_details(&self, poll_id: &str) -> Result<Option<StoredPoll>> {
        self.db
            .query_row(
                "SELECT options, secret, creator_jid FROM polls WHERE poll_id = ?1",
                [poll_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(|(options, secret, creator_jid)| {
                Ok(StoredPoll {
                    options: serde_json::from_str(&options)?,
                    secret,
                    creator_jid,
                })
            })
            .transpose()
    }

    fn save_vote(&mut self, update: &PollUpdate, choices: &[usize]) -> Result<bool> {
        let tx = self.db.transaction()?;
        let duplicate = tx
            .query_row(
                "SELECT 1 FROM poll_votes WHERE update_id = ?1",
                [&update.update_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if duplicate {
            return Ok(false);
        }
        let current = tx
            .query_row(
                "SELECT timestamp FROM poll_votes WHERE poll_id = ?1 AND voter_id = ?2",
                params![update.poll_id, update.sender_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if current.is_some_and(|timestamp| timestamp > update.timestamp) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO poll_votes (poll_id, voter_id, timestamp, update_id, choices)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(poll_id, voter_id) DO UPDATE SET
               timestamp = excluded.timestamp, update_id = excluded.update_id,
               choices = excluded.choices",
            params![
                update.poll_id,
                update.sender_id,
                update.timestamp,
                update.update_id,
                serde_json::to_string(choices)?,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::TodoStore;
    use crate::{
        PollDraft,
        whatsapp::{CreatedPoll, PollUpdate},
    };

    #[test]
    fn poll_secrets_and_latest_vote_are_kept_in_encrypted_store() {
        let mut store = TodoStore::open(":memory:", &[7; 32]).unwrap();
        let draft = PollDraft {
            question: "Tasks?".into(),
            options: vec!["Write tests".into(), "Review code".into()],
            multiple: true,
        };
        store
            .record_poll(
                "group:1",
                &draft,
                &CreatedPoll {
                    message_id: "poll-1".into(),
                    secret: vec![9; 32],
                    creator_jid: "owner@s.whatsapp.net".into(),
                },
            )
            .unwrap();
        let first = PollUpdate {
            chat_id: "group:1".into(),
            update_id: "vote-1".into(),
            poll_id: "poll-1".into(),
            sender_id: "member@s.whatsapp.net".into(),
            from_me: false,
            timestamp: 2000,
            enc_payload: None,
            enc_iv: None,
            selected_hashes: Some(vec![]),
        };
        assert!(store.save_vote(&first, &[0]).unwrap());
        assert!(!store.save_vote(&first, &[1]).unwrap());
        assert_eq!(store.latest("group:1").unwrap()[0].selected_voters, [0, 0]);
        let stale = PollUpdate {
            update_id: "vote-old".into(),
            timestamp: 1000,
            ..first.clone()
        };
        assert!(!store.save_vote(&stale, &[1]).unwrap());
        let latest = PollUpdate {
            update_id: "vote-2".into(),
            timestamp: 3000,
            sender_id: "owner@s.whatsapp.net".into(),
            ..first
        };
        assert!(store.save_vote(&latest, &[1]).unwrap());
        assert_eq!(store.latest("group:1").unwrap()[0].selected_voters, [0, 1]);
    }
}
