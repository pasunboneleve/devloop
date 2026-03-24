use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

#[derive(Debug)]
pub struct SessionState {
    path: PathBuf,
    values: Map<String, Value>,
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
        Ok(Self { path, values })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Result<()> {
        self.refresh()?;
        self.values.insert(key.into(), value);
        self.save()
    }

    pub fn merge_json_object(&mut self, object: Map<String, Value>) -> Result<()> {
        self.refresh()?;
        self.values.extend(object);
        self.save()
    }

    fn refresh(&mut self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read state file {}", self.path.display()))?;
        self.values = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse state file {}", self.path.display()))?;
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }
        let raw = serde_json::to_string_pretty(&self.values)?;
        fs::write(&self.path, raw)
            .with_context(|| format!("failed to write state file {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn set_preserves_external_file_updates() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let state_path = std::env::temp_dir().join(format!("devloop-state-{unique}.json"));
        let mut state = SessionState::load(state_path.clone()).expect("load state");

        fs::write(
            &state_path,
            r#"{"tunnel_url":"https://example.trycloudflare.com"}"#,
        )
        .expect("write external state");
        state
            .set("current_post_slug", Value::String("example-post".into()))
            .expect("set state key");

        let written = fs::read_to_string(&state_path).expect("read saved state");
        let json: Value = serde_json::from_str(&written).expect("parse state");
        assert_eq!(
            json["tunnel_url"],
            Value::String("https://example.trycloudflare.com".into())
        );
        assert_eq!(
            json["current_post_slug"],
            Value::String("example-post".into())
        );

        fs::remove_file(state_path).expect("cleanup state file");
    }
}
