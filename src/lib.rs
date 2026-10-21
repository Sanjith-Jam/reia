pub mod agent;
pub mod app;
pub mod config;
pub mod knowledge;
pub mod polls;
pub mod sponsors;
pub mod storage;
pub mod whatsapp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceScope {
    All,
    Selected(std::collections::BTreeSet<String>),
}

impl SourceScope {
    pub fn allows(&self, chat_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Selected(chats) => chats.contains(chat_id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chat {
    pub id: String,
    pub name: String,
    pub is_group: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub chat_id: String,
    pub id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub timestamp: i64,
    pub text: String,
    pub is_live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollVote {
    pub chat_id: String,
    pub poll_id: String,
    pub voter_id: String,
    pub timestamp: i64,
    pub choices: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollDraft {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

#[cfg(test)]
mod tests {
    use super::SourceScope;
    use std::collections::BTreeSet;

    #[test]
    fn source_scope_defaults_and_selection_are_explicit() {
        assert!(SourceScope::All.allows("group:1"));
        let selected = SourceScope::Selected(BTreeSet::from(["chat:1".to_owned()]));
        assert!(selected.allows("chat:1"));
        assert!(!selected.allows("group:1"));
    }
}
