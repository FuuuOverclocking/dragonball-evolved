use std::path::PathBuf;

use anyhow::{Context, Result};
use api::Config;
use api::logger::{CrashTarget, Format, LevelFilter, Target};
use clap::{Parser, Subcommand};

use crate::{VERSION_LONG, config};

/// Start a Dragonball VMM instance.
///
/// Pass boot sources to launch directly, or use --api-sock to set up a Unix socket for REST API control.
#[derive(Debug, Parser)]
#[command(
    version = VERSION_LONG,
    args_conflicts_with_subcommands = true,
)]
struct Cli {
    #[command(subcommand)]
    subcommand: Option<SubCommand>,

    /// Set the config file path (JSON or TOML). CLI args and env vars will override these settings.
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// The ID of the dragonball instance, default to a random string.
    #[arg(long, value_name = "id", env = "DRAGONBALL_ID")]
    id: Option<String>,

    /// Launch an API server and specify the path to the api socket.
    #[arg(long, env, value_name = "PATH")]
    api_sock: Option<PathBuf>,

    /// Specify the path to the KVM device.
    #[arg(long, env, value_name = "PATH")]
    kvm_dev: Option<PathBuf>,

    /// The lowest level a record has to reach to be logged: off, error, warn, info, debug or trace. Default: info.
    #[arg(
        long,
        env,
        value_name = "LEVEL",
        value_parser = |value: &str| LevelFilter::try_from(value.to_owned()),
    )]
    log_level: Option<LevelFilter>,

    /// Where log records go: stderr or file=<path>. Default: stderr.
    #[arg(
        long,
        env,
        value_name = "TARGET",
        value_parser = |value: &str| Target::try_from(value.to_owned()),
    )]
    log_target: Option<Target>,

    /// Where crash records go: same_as_log, stderr or file=<path>. Default: same_as_log.
    #[arg(
        long,
        env,
        value_name = "TARGET",
        value_parser = |value: &str| CrashTarget::try_from(value.to_owned()),
    )]
    log_crash_target: Option<CrashTarget>,

    /// How a log line is laid out: text or json. Default: text.
    #[arg(
        long,
        env,
        value_name = "FORMAT",
        value_parser = |value: &str| serde_json::from_value::<Format>(value.into()),
    )]
    log_format: Option<Format>,

    /// Whether a log line carries the thread id. Default: true.
    #[arg(long, env, value_name = "BOOL")]
    log_show_tid: Option<bool>,

    /// Whether a log line carries the thread name. Default: true.
    #[arg(long, env, value_name = "BOOL")]
    log_show_thread_name: Option<bool>,

    /// Whether a log line carries the target, i.e. the module that logged it. Default: true.
    #[arg(long, env, value_name = "BOOL")]
    log_show_target: Option<bool>,

    /// Whether a log line carries the source file and line. Default: true.
    #[arg(long, env, value_name = "BOOL")]
    log_show_file_line: Option<bool>,

    /// Whether a log line carries the instance id. Default: true.
    #[arg(long, env, value_name = "BOOL")]
    log_show_id: Option<bool>,
}

#[derive(Debug, Subcommand)]
pub enum SubCommand {
    /// Load a configuration and print process settings and commands without starting a VMM.
    CheckConfig {
        #[arg(value_name = "PATH")]
        path: PathBuf,
    },
}

pub fn parse() -> Result<(Config, Option<SubCommand>)> {
    Cli::parse().into_config()
}

impl Cli {
    fn into_config(self) -> Result<(Config, Option<SubCommand>)> {
        if let Some(subcommand) = self.subcommand {
            return Ok((Config::default(), Some(subcommand)));
        }
        let mut config = self
            .config
            .as_deref()
            .map(config::load)
            .transpose()
            .context("parse config file")?
            .unwrap_or_default();

        self.patch_config(&mut config);
        apply_defaults(&mut config);
        Ok((config, None))
    }

    fn patch_config(self, cfg: &mut Config) {
        let dragonball = &mut cfg.dragonball;
        dragonball.id = self.id.or(dragonball.id.take());
        dragonball.api_sock = self.api_sock.or(dragonball.api_sock.take());
        dragonball.kvm_dev = self.kvm_dev.or(dragonball.kvm_dev.take());

        let logger = &mut dragonball.logger;
        logger.level = self.log_level.or(logger.level.take());
        logger.target = self.log_target.or(logger.target.take());
        logger.crash_target = self.log_crash_target.or(logger.crash_target.take());
        logger.format = self.log_format.or(logger.format.take());
        logger.show_tid = self.log_show_tid.or(logger.show_tid.take());
        logger.show_thread_name = self.log_show_thread_name.or(logger.show_thread_name.take());
        logger.show_target = self.log_show_target.or(logger.show_target.take());
        logger.show_file_line = self.log_show_file_line.or(logger.show_file_line.take());
        logger.show_id = self.log_show_id.or(logger.show_id.take());
    }
}

fn apply_defaults(cfg: &mut Config) {
    cfg.dragonball
        .id
        .get_or_insert_with(|| names::Generator::default().next().unwrap());
    cfg.dragonball.kvm_dev.get_or_insert("/dev/kvm".into());

    let logger = &mut cfg.dragonball.logger;
    logger.level.get_or_insert(LevelFilter::Info);
    logger.target.get_or_insert(Target::Stderr);
    logger.crash_target.get_or_insert(CrashTarget::SameAsLog);
    logger.format.get_or_insert(Format::Text);
    logger.show_tid.get_or_insert(true);
    logger.show_thread_name.get_or_insert(true);
    logger.show_target.get_or_insert(true);
    logger.show_file_line.get_or_insert(true);
    logger.show_id.get_or_insert(true);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use api::ProcessConfig;
    use api::logger::UpdateLogger;
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

    fn parse_cli(args: impl IntoIterator<Item = &'static str>) -> Result<Cli, clap::Error> {
        let matches = Cli::command()
            .mut_args(|arg| arg.env(None::<&str>))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn command_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn schema_command_is_not_available() {
        assert!(parse_cli(["dragonball-evolved", "schema", "export"]).is_err());
    }

    #[test]
    fn check_config_parses_without_loading_a_file() {
        let args = parse_cli(["dragonball-evolved", "check-config", "missing.toml"]).unwrap();
        let (config, subcommand) = args.into_config().unwrap();
        let Some(SubCommand::CheckConfig { path }) = subcommand else {
            panic!("expected configuration check");
        };
        assert_eq!(path, PathBuf::from("missing.toml"));
        assert!(config.dragonball.id.is_none());
        assert!(config.dragonball.logger.level.is_none());
    }

    #[test]
    fn check_config_rejects_missing_path_and_runtime_options() {
        for args in [
            vec!["dragonball-evolved", "check-config"],
            vec![
                "dragonball-evolved",
                "check-config",
                "vm.toml",
                "--id",
                "vm",
            ],
            vec![
                "dragonball-evolved",
                "--id",
                "vm",
                "check-config",
                "vm.toml",
            ],
            vec![
                "dragonball-evolved",
                "--config",
                "base.toml",
                "check-config",
                "vm.toml",
            ],
        ] {
            assert!(parse_cli(args).is_err());
        }
    }

    #[test]
    fn run_without_subcommand_applies_defaults() {
        let args = parse_cli(["dragonball-evolved"]).unwrap();
        let (config, subcommand) = args.into_config().unwrap();
        assert!(subcommand.is_none());
        assert!(config.dragonball.id.is_some());
        assert_eq!(
            config.dragonball.kvm_dev.as_deref(),
            Some(Path::new("/dev/kvm"))
        );
        assert_eq!(config.dragonball.logger.level, Some(LevelFilter::Info));
    }

    #[test]
    fn cli_patch_defaults_and_accessors_work_together() {
        let mut config = Config {
            dragonball: ProcessConfig {
                id: Some("from-file".into()),
                api_sock: None,
                kvm_dev: None,
                logger: UpdateLogger {
                    level: Some(LevelFilter::Warn),
                    show_tid: Some(true),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let args = parse_cli([
            "dragonball-evolved",
            "--api-sock",
            "/tmp/dragonball.sock",
            "--log-target",
            "stderr",
            "--log-show-tid",
            "false",
        ])
        .unwrap();

        args.patch_config(&mut config);
        apply_defaults(&mut config);

        assert_eq!(config.dragonball.id.as_deref(), Some("from-file"));
        assert_eq!(
            config.dragonball.api_sock.as_deref(),
            Some(Path::new("/tmp/dragonball.sock"))
        );
        assert_eq!(
            config.dragonball.kvm_dev.as_deref(),
            Some(Path::new("/dev/kvm"))
        );

        let logger = config.dragonball.logger;
        assert_eq!(logger.level, Some(LevelFilter::Warn));
        assert_eq!(logger.target, Some(Target::Stderr));
        assert_eq!(logger.show_tid, Some(false));
        assert_eq!(logger.show_id, Some(true));
    }
}
