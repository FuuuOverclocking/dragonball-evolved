use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use api::Config;

pub fn load(path: &Path) -> Result<Config> {
    let value = load_config_value(path, &mut Default::default())?;
    Config::parse(value).with_context(|| format!("interpret config fields from {}", path.display()))
}

fn load_config_value(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<serde_json::Value> {
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalize config path {}", path.display()))?;
    if !visited.insert(path.clone()) {
        bail!(
            "circular inheritance detected in config file: {}",
            path.display()
        );
    }

    let bytes = fs::read(&path).with_context(|| format!("read config file {}", path.display()))?;

    let mut value: serde_json::Value = match path.extension() {
        Some(s) if s.eq_ignore_ascii_case("toml") => toml::from_slice(&bytes)
            .with_context(|| format!("parse toml config {}", path.display()))?,
        _ => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse json config {}", path.display()))?,
    };

    let serde_json::Value::Object(object) = &mut value else {
        bail!("expected a config object in {}", path.display());
    };
    if let Some(schema) = object.remove("$schema")
        && !schema.is_string()
    {
        bail!("invalid `$schema` field type in {}", path.display());
    }

    if let Some(extends_val) = object.remove("extends") {
        let Some(extends_str) = extends_val.as_str().filter(|path| !path.is_empty()) else {
            bail!("expected a nonempty `extends` path in {}", path.display());
        };

        let parent_dir = path.parent().unwrap_or(Path::new(""));
        let base_path = parent_dir.join(extends_str);
        let mut base_value = load_config_value(&base_path, visited).with_context(|| {
            format!(
                "parse base config {} extended by {}",
                base_path.display(),
                path.display()
            )
        })?;

        merge_value(&mut base_value, value);

        visited.remove(&path);
        return Ok(base_value);
    }

    visited.remove(&path);
    Ok(value)
}

fn merge_value(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(base_value) => merge_value(base_value, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        // Anything else -- scalars, nulls, type changes and arrays -- replaces what
        // it lands on. Extending an inherited array instead would leave no way to
        // shorten one, and no way to clear it.
        (base, overlay) => *base = overlay,
    }
}
