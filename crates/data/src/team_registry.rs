//! Team registry — which sessions form a team (lead + workers + board space).
//!
//! Port of `coworker/teams/registry.py`. Persisted as JSON at `data_dir/teams.json`.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorker {
    /// Lead-given name — board actor id / assignee handle.
    pub actor: String,
    pub persona: String,
    pub session_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Team {
    pub team_id: String,
    pub space: String,
    pub lead_session: String,
    pub lead_actor: String,
    #[serde(default)]
    pub workers: Vec<TeamWorker>,
    #[serde(default)]
    pub chat_enabled: bool,
    #[serde(default)]
    pub chat_group: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub created_at: String,
    /// Rolling budget gate: UTC hour key `YYYY-MM-DDTHH`.
    #[serde(default)]
    pub wake_hour: String,
    #[serde(default)]
    pub wakes_this_hour: i32,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    teams: Vec<Team>,
}

/// Roster the wake plumbing walks each tick.
pub struct TeamRegistry {
    path: Option<PathBuf>,
    lock: Mutex<()>,
    teams: Mutex<HashMap<String, Team>>,
}

impl TeamRegistry {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut teams = HashMap::new();
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(file) = serde_json::from_str::<RegistryFile>(&text) {
                    for team in file.teams {
                        teams.insert(team.team_id.clone(), team);
                    }
                }
            }
        }
        Self {
            path: Some(path),
            lock: Mutex::new(()),
            teams: Mutex::new(teams),
        }
    }

    /// In-memory registry (tests).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            lock: Mutex::new(()),
            teams: Mutex::new(HashMap::new()),
        }
    }

    fn save_unlocked(&self, teams: &HashMap<String, Team>) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = RegistryFile {
            teams: teams.values().cloned().collect(),
        };
        let Ok(text) = serde_json::to_string_pretty(&file) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    pub fn create(
        &self,
        space: &str,
        lead_session: &str,
        lead_actor: &str,
        workers: Vec<TeamWorker>,
        chat_enabled: bool,
        chat_group: &str,
    ) -> Team {
        let team = Team {
            team_id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            space: space.to_string(),
            lead_session: lead_session.to_string(),
            lead_actor: lead_actor.to_string(),
            workers,
            chat_enabled,
            chat_group: chat_group.to_string(),
            paused: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            wake_hour: String::new(),
            wakes_this_hour: 0,
        };
        let _g = self.lock.lock();
        let mut map = self.teams.lock();
        map.insert(team.team_id.clone(), team.clone());
        self.save_unlocked(&map);
        team
    }

    pub fn all(&self) -> Vec<Team> {
        self.teams.lock().values().cloned().collect()
    }

    pub fn get(&self, team_id: &str) -> Option<Team> {
        self.teams.lock().get(team_id).cloned()
    }

    pub fn for_lead_session(&self, session_id: &str) -> Option<Team> {
        self.teams
            .lock()
            .values()
            .find(|t| t.lead_session == session_id)
            .cloned()
    }

    pub fn for_worker_session(&self, session_id: &str) -> Option<(Team, TeamWorker)> {
        for team in self.teams.lock().values() {
            for worker in &team.workers {
                if worker.session_id == session_id {
                    return Some((team.clone(), worker.clone()));
                }
            }
        }
        None
    }

    pub fn set_paused(&self, team_id: &str, paused: bool) {
        let _g = self.lock.lock();
        let mut map = self.teams.lock();
        if let Some(team) = map.get_mut(team_id) {
            team.paused = paused;
            self.save_unlocked(&map);
        }
    }

    /// Budget gate: count one automatic wake. `false` = over cap this hour.
    pub fn count_wake(&self, team_id: &str, cap: i32) -> bool {
        let hour = chrono::Utc::now().format("%Y-%m-%dT%H").to_string();
        let _g = self.lock.lock();
        let mut map = self.teams.lock();
        let Some(team) = map.get_mut(team_id) else {
            return false;
        };
        if team.wake_hour != hour {
            team.wake_hour = hour;
            team.wakes_this_hour = 0;
        }
        if team.wakes_this_hour >= cap {
            self.save_unlocked(&map);
            return false;
        }
        team.wakes_this_hour += 1;
        self.save_unlocked(&map);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn registry_roundtrip_and_budget_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("teams.json");
        let reg = TeamRegistry::open(&path);
        let team = reg.create(
            "proj",
            "lead-sid",
            "lead-1",
            vec![TeamWorker {
                actor: "swe-worker".into(),
                persona: "swe-worker".into(),
                session_id: "w1".into(),
                model: String::new(),
                reason: String::new(),
            }],
            false,
            "",
        );
        let again = TeamRegistry::open(&path);
        let loaded = again.get(&team.team_id).unwrap();
        assert_eq!(loaded.workers[0].session_id, "w1");
        assert_eq!(
            again.for_lead_session("lead-sid").unwrap().team_id,
            team.team_id
        );
        assert_eq!(
            again.for_worker_session("w1").unwrap().1.actor,
            "swe-worker"
        );
        assert!(again.count_wake(&team.team_id, 3));
        assert!(again.count_wake(&team.team_id, 3));
        assert!(again.count_wake(&team.team_id, 3));
        assert!(!again.count_wake(&team.team_id, 3));
    }
}
