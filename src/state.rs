use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct SessionState {
    path: PathBuf,
    values: Arc<Mutex<Map<String, Value>>>,
}

impl SessionState {
    pub fn load(path: PathBuf) -> Result<Self> {
        let values = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read state file {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse state file {}", path.display()))?
        } else {
            Map::new()
        };
        Ok(Self {
            path,
            values: Arc::new(Mutex::new(values)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set(&self, key: impl Into<String>, value: Value) -> Result<()> {
        let mut values = self.lock_values()?;
        let key = key.into();
        if values.get(&key) == Some(&value) {
            return Ok(());
        }
        values.insert(key, value);
        let snapshot = values.clone();
        drop(values);
        self.save_snapshot(&snapshot)
    }

    pub fn merge_json_object(&self, object: Map<String, Value>) -> Result<()> {
        let mut values = self.lock_values()?;
        let mut changed = false;
        for (key, value) in object {
            if values.get(&key) != Some(&value) {
                values.insert(key, value);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        let snapshot = values.clone();
        drop(values);
        self.save_snapshot(&snapshot)
    }

    pub fn get_string(&self, key: &str) -> Result<Option<String>> {
        let values = self.lock_values()?;
        Ok(values
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    pub fn snapshot(&self) -> Result<Map<String, Value>> {
        let values = self.lock_values()?;
        Ok(values.clone())
    }

    #[cfg(test)]
    pub fn render_template(&self, template: &str) -> Result<String> {
        let values = self.lock_values()?;
        render_template_values(&values, template)
    }

    fn lock_values(&self) -> Result<MutexGuard<'_, Map<String, Value>>> {
        self.values
            .lock()
            .map_err(|_| anyhow!("session state mutex was poisoned"))
    }

    fn save_snapshot(&self, values: &Map<String, Value>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }
        let raw = serde_json::to_string_pretty(values)?;
        fs::write(&self.path, raw)
            .with_context(|| format!("failed to write state file {}", self.path.display()))?;
        Ok(())
    }
}

pub(crate) fn render_template_values(
    values: &Map<String, Value>,
    template: &str,
) -> Result<String> {
    let pattern = Regex::new(r"\{\{\s*([a-zA-Z0-9_.-]+)\s*\}\}")?;
    let rendered = pattern.replace_all(template, |captures: &regex::Captures<'_>| {
        values
            .get(&captures[1])
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    });
    Ok(rendered.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_state_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("devloop-state-{unique}.json"))
    }

    #[test]
    fn clones_share_in_memory_state() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");
        let other = state.clone();

        state
            .set("current_post_slug", Value::String("example-post".into()))
            .expect("set state key");

        assert_eq!(
            other
                .get_string("current_post_slug")
                .expect("get state key")
                .as_deref(),
            Some("example-post")
        );

        fs::remove_file(state_path).expect("cleanup state file");
    }

    #[test]
    fn render_template_substitutes_known_state_keys() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");

        state
            .set(
                "tunnel_url",
                Value::String("https://example.trycloudflare.com".into()),
            )
            .expect("set tunnel url");
        state
            .set("current_post_slug", Value::String("example-post".into()))
            .expect("set slug");

        let rendered = state
            .render_template("{{ tunnel_url }}/posts/{{current_post_slug}}")
            .expect("render template");

        assert_eq!(
            rendered,
            "https://example.trycloudflare.com/posts/example-post"
        );

        fs::remove_file(state_path).expect("cleanup state file");
    }

    #[test]
    fn set_persists_latest_values_to_disk() {
        let state_path = unique_state_path();
        let state = SessionState::load(state_path.clone()).expect("load state");

        state
            .set("current_post_slug", Value::String("example-post".into()))
            .expect("set state key");

        let written = fs::read_to_string(&state_path).expect("read saved state");
        let json: Value = serde_json::from_str(&written).expect("parse state");
        assert_eq!(
            json["current_post_slug"],
            Value::String("example-post".into())
        );

        fs::remove_file(state_path).expect("cleanup state file");
    }
}
