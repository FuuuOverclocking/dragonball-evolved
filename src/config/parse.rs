use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::config::cli::Cli;
use crate::config::{Config, DragonballConfig, LoggerConfig, SubCommand};

pub(crate) fn parse_cli() -> Result<(Config, Option<SubCommand>)> {
    let mut cli = Cli::parse();
    let subcommand = cli.command.take();

    let mut config = cli
        .config
        .as_deref()
        .map(parse_config)
        .transpose()
        .context("parse config")?
        .unwrap_or_default();

    patch_dragonball_config(&mut config.dragonball, cli.dragonball);
    apply_defaults(&mut config);

    Ok((config, subcommand))
}

fn parse_config(path: &Path) -> Result<Config> {
    let value = parse_config_internal(path, &mut Default::default())?;
    serde_json::from_value::<Config>(value)
        .with_context(|| format!("interpret config fields from {}", path.display()))
}

fn parse_config_internal(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<serde_json::Value> {
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

    if let Some(extends_val) = value.get("extends").cloned() {
        let Some(extends_str) = extends_val.as_str() else {
            bail!("invalid `extends` field type in {}", path.display());
        };

        let parent_dir = path.parent().unwrap_or(Path::new(""));
        let base_path = parent_dir.join(extends_str);
        let mut base_value = parse_config_internal(&base_path, visited).with_context(|| {
            format!(
                "parse base config {} extended by {}",
                base_path.display(),
                path.display()
            )
        })?;

        if let serde_json::Value::Object(map) = &mut value {
            map.remove("extends");
        }

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
        (serde_json::Value::Array(base), serde_json::Value::Array(overlay)) => {
            base.extend(overlay);
        }
        (base, overlay) => *base = overlay,
    }
}

fn patch_dragonball_config(cfg: &mut DragonballConfig, from: DragonballConfig) {
    let DragonballConfig {
        id,
        api_sock,
        kvm_dev,
        logger,
    } = from;

    cfg.id = id.or(cfg.id.take());
    cfg.api_sock = api_sock.or(cfg.api_sock.take());
    cfg.kvm_dev = kvm_dev.or(cfg.kvm_dev.take());

    let LoggerConfig {
        level,
        target,
        crash_target,
        format,
        show_tid,
        show_thread_name,
        show_target,
        show_file_line,
        show_id,
    } = logger;
    let cfg_logger = &mut cfg.logger;

    cfg_logger.level = level.or(cfg_logger.level.take());
    cfg_logger.target = target.or(cfg_logger.target.take());
    cfg_logger.crash_target = crash_target.or(cfg_logger.crash_target.take());
    cfg_logger.format = format.or(cfg_logger.format.take());
    cfg_logger.show_tid = show_tid.or(cfg_logger.show_tid.take());
    cfg_logger.show_thread_name = show_thread_name.or(cfg_logger.show_thread_name.take());
    cfg_logger.show_target = show_target.or(cfg_logger.show_target.take());
    cfg_logger.show_file_line = show_file_line.or(cfg_logger.show_file_line.take());
    cfg_logger.show_id = show_id.or(cfg_logger.show_id.take());
}

fn apply_defaults(cfg: &mut Config) {
    cfg.dragonball
        .id
        .get_or_insert_with(|| names::Generator::default().next().unwrap());
    cfg.dragonball.kvm_dev.get_or_insert("/dev/kvm".into());

    let logger = &mut cfg.dragonball.logger;
    logger.level.get_or_insert(logger::LevelFilter::Info);
    logger.target.get_or_insert(logger_backend::Target::Stderr);
    logger
        .crash_target
        .get_or_insert(logger_backend::CrashTarget::SameAsLog);
    logger.format.get_or_insert(logger_backend::Format::Text);
    logger.show_tid.get_or_insert(true);
    logger.show_thread_name.get_or_insert(true);
    logger.show_target.get_or_insert(true);
    logger.show_file_line.get_or_insert(true);
    logger.show_id.get_or_insert(true);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dragonball-config-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn merge_value_applies_config_inheritance_semantics() {
        let mut base = json!({
            "object": {
                "inherited": 1,
                "overridden": "parent"
            },
            "array": [1, { "parent": true }],
            "cleared": {
                "inherited": true
            },
            "replaced_type": {
                "old": true
            }
        });
        let overlay = json!({
            "object": {
                "overridden": "child",
                "added": 2
            },
            "array": [2, { "child": true }],
            "cleared": null,
            "replaced_type": ["new"]
        });

        merge_value(&mut base, overlay);

        assert_eq!(
            base,
            json!({
                "object": {
                    "inherited": 1,
                    "overridden": "child",
                    "added": 2
                },
                "array": [1, { "parent": true }, 2, { "child": true }],
                "cleared": null,
                "replaced_type": ["new"]
            })
        );
    }

    #[test]
    fn parse_config_resolves_and_merges_relative_parent() {
        let dir = TestDir::new();
        let parent = dir.join("parent.toml");
        let child = dir.join("child.json");
        fs::write(
            &parent,
            r#"
array = [1]
cleared = "parent"

[object]
inherited = 1
overridden = "parent"
"#,
        )
        .unwrap();
        fs::write(
            &child,
            r#"{
                "extends": "parent.toml",
                "object": {"overridden": "child", "added": 2},
                "array": [2],
                "cleared": null
            }"#,
        )
        .unwrap();

        let value = parse_config_internal(&child, &mut Default::default()).unwrap();

        assert_eq!(
            value,
            json!({
                "object": {
                    "inherited": 1,
                    "overridden": "child",
                    "added": 2
                },
                "array": [1, 2],
                "cleared": null
            })
        );
    }

    #[test]
    fn parse_config_errors_identify_relevant_files() {
        let dir = TestDir::new();
        let malformed = dir.join("malformed.json");
        fs::write(&malformed, "{").unwrap();

        let error = parse_config_internal(&malformed, &mut Default::default()).unwrap_err();
        assert!(format!("{error:#}").contains(malformed.to_str().unwrap()));

        let child = dir.join("child.json");
        let missing_parent = dir.join("missing.json");
        fs::write(&child, r#"{"extends": "missing.json"}"#).unwrap();

        let error = parse_config_internal(&child, &mut Default::default()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(child.to_str().unwrap()));
        assert!(message.contains(missing_parent.to_str().unwrap()));
    }

    #[test]
    fn cli_patch_defaults_accessors_and_logger_conversion_work_together() {
        let mut config = Config {
            dragonball: DragonballConfig {
                id: Some("from-file".into()),
                api_sock: None,
                kvm_dev: None,
                logger: LoggerConfig {
                    level: Some(logger::LevelFilter::Warn),
                    show_tid: Some(true),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let cli_config = DragonballConfig {
            api_sock: Some("/tmp/dragonball.sock".into()),
            logger: LoggerConfig {
                target: Some(logger_backend::Target::Stderr),
                show_tid: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };

        patch_dragonball_config(&mut config.dragonball, cli_config);
        apply_defaults(&mut config);

        assert_eq!(config.dragonball.id(), "from-file");
        assert_eq!(
            config.dragonball.api_sock.as_deref(),
            Some(Path::new("/tmp/dragonball.sock"))
        );
        assert_eq!(
            config.dragonball.kvm_dev.as_deref(),
            Some(Path::new("/dev/kvm"))
        );

        let logger: logger_backend::Config = config.dragonball.logger().clone().into();
        assert_eq!(logger.id, None);
        assert_eq!(logger.level, Some(logger::LevelFilter::Warn));
        assert_eq!(logger.target, Some(logger_backend::Target::Stderr));
        assert_eq!(logger.show_tid, Some(false));
        assert_eq!(logger.show_id, Some(true));
    }
}
