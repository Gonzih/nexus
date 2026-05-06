use serde::{Deserialize, Serialize};
use soul_core::types::Message;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub total_turns: usize,
    pub messages: Vec<Message>,
}

impl SessionState {
    pub fn new(session_id: &str) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            session_id: session_id.to_string(),
            created_at: now.clone(),
            updated_at: now,
            total_turns: 0,
            messages: Vec::new(),
        }
    }

    pub fn state_dir(cwd: &str) -> PathBuf {
        Path::new(cwd).join(".nexus-state")
    }

    pub fn state_path(cwd: &str, session_id: &str) -> PathBuf {
        Self::state_dir(cwd).join(format!("{session_id}.json"))
    }

    pub fn load(cwd: &str, session_id: &str) -> Option<Self> {
        let path = Self::state_path(cwd, session_id);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Find the most recently modified state file in .nexus-state/
    pub fn load_latest(cwd: &str) -> Option<Self> {
        let dir = Self::state_dir(cwd);
        let entries = std::fs::read_dir(&dir).ok()?;

        let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        match &latest {
                            Some((prev_time, _)) if modified > *prev_time => {
                                latest = Some((modified, path));
                            }
                            None => {
                                latest = Some((modified, path));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let (_, path) = latest?;
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&mut self, cwd: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.updated_at = chrono::Utc::now().to_rfc3339();
        let dir = Self::state_dir(cwd);
        std::fs::create_dir_all(&dir)?;
        let path = Self::state_path(cwd, &self.session_id);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        tracing::info!(
            path = %path.display(),
            messages = self.messages.len(),
            turns = self.total_turns,
            "Session state saved"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_has_empty_messages() {
        let state = SessionState::new("test-session");
        assert_eq!(state.session_id, "test-session");
        assert_eq!(state.total_turns, 0);
        assert!(state.messages.is_empty());
        assert!(!state.created_at.is_empty());
        assert!(!state.updated_at.is_empty());
    }

    #[test]
    fn state_dir_is_under_cwd() {
        let dir = SessionState::state_dir("/tmp/project");
        assert_eq!(dir, PathBuf::from("/tmp/project/.nexus-state"));
    }

    #[test]
    fn state_path_includes_session_id() {
        let path = SessionState::state_path("/tmp/project", "session-123");
        assert_eq!(
            path,
            PathBuf::from("/tmp/project/.nexus-state/session-123.json")
        );
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();

        let mut state = SessionState::new("roundtrip-test");
        state.messages.push(Message::user("hello"));
        state.total_turns = 3;
        state.save(cwd).unwrap();

        let loaded = SessionState::load(cwd, "roundtrip-test").unwrap();
        assert_eq!(loaded.session_id, "roundtrip-test");
        assert_eq!(loaded.total_turns, 3);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text_content(), "hello");
    }

    #[test]
    fn load_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert!(SessionState::load(cwd, "nonexistent").is_none());
    }

    #[test]
    fn load_latest_returns_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();

        let mut state1 = SessionState::new("session-old");
        state1.messages.push(Message::user("old"));
        state1.save(cwd).unwrap();

        // Small delay so mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut state2 = SessionState::new("session-new");
        state2.messages.push(Message::user("new"));
        state2.save(cwd).unwrap();

        let latest = SessionState::load_latest(cwd).unwrap();
        assert_eq!(latest.session_id, "session-new");
    }

    #[test]
    fn load_latest_returns_none_for_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        assert!(SessionState::load_latest(cwd).is_none());
    }

    #[test]
    fn save_updates_updated_at() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();

        let mut state = SessionState::new("update-test");
        let original_updated = state.updated_at.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));
        state.save(cwd).unwrap();

        // updated_at should have changed
        assert_ne!(state.updated_at, original_updated);
    }

    #[test]
    fn serialization_format_is_valid_json() {
        let mut state = SessionState::new("json-test");
        state.messages.push(Message::user("test message"));
        state.total_turns = 5;

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("\"session_id\""));
        assert!(json.contains("\"json-test\""));
        assert!(json.contains("\"total_turns\""));

        // Can deserialize back
        let parsed: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.session_id, "json-test");
        assert_eq!(parsed.total_turns, 5);
    }
}
