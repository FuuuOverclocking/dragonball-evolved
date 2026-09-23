use std::path::PathBuf;

use anyhow::{Context, Result};
use api::logger::{CrashTarget, Format, Target};
use clap::{Arg, ArgMatches, Command, value_parser};

use crate::VERSION_LONG;
use crate::config::{self, Config};

#[derive(Debug)]
pub enum SubCommand {}

pub fn parse() -> Result<(Config, Option<SubCommand>)> {
    let mut matches = command().get_matches();
    let mut config = matches
        .remove_one::<PathBuf>("config")
        .as_deref()
        .map(config::load)
        .transpose()
        .context("parse config file")?
        .unwrap_or_default();

    patch_config(&mut config, &mut matches);
    apply_defaults(&mut config);
    Ok((config, None))
}

fn command() -> Command {
    Command::new("dragonball-evolved")
        .version(VERSION_LONG)
        .about("Start a Dragonball VMM instance.")
        .long_about(
            "Start a Dragonball VMM instance.\n\nIf boot sources are provided, the virtual machine will boot directly. Alternatively, you can use `--api-sock` to create a Unix domain socket, enabling control of the VM via a RESTful API.",
        )
        .args([
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("PATH")
                .value_parser(value_parser!(PathBuf))
                .help("Specify the path to the config file. Could be json or toml, recognized by extension.")
                .long_help("Specify the path to the config file. Could be json or toml, recognized by extension. The settings can be overwritten by CLI arguments or env vars."),
            Arg::new("id")
                .long("id")
                .env("DRAGONBALL_ID")
                .help("The ID of the dragonball instance, default to a random string."),
            Arg::new("api-sock")
                .long("api-sock")
                .env("API_SOCK")
                .value_name("PATH")
                .value_parser(value_parser!(PathBuf))
                .help("Launch an API server and specify the path to the api socket."),
            Arg::new("kvm-dev")
                .long("kvm-dev")
                .env("KVM_DEV")
                .value_name("PATH")
                .value_parser(value_parser!(PathBuf))
                .help("Specify the path to the KVM device."),
            Arg::new("log-level")
                .long("log-level")
                .env("LOG_LEVEL")
                .value_name("LEVEL")
                .value_parser(value_parser!(logger::LevelFilter))
                .help("The lowest level a record has to reach to be logged: off, error, warn, info, debug or trace. Default: info."),
            Arg::new("log-target")
                .long("log-target")
                .env("LOG_TARGET")
                .value_name("TARGET")
                .value_parser(|value: &str| Target::try_from(value.to_owned()))
                .help("Where log records go: stderr or file=<path>. Default: stderr."),
            Arg::new("log-crash-target")
                .long("log-crash-target")
                .env("LOG_CRASH_TARGET")
                .value_name("TARGET")
                .value_parser(|value: &str| CrashTarget::try_from(value.to_owned()))
                .help("Where crash records go: same_as_log, stderr or file=<path>. Default: same_as_log."),
            Arg::new("log-format")
                .long("log-format")
                .env("LOG_FORMAT")
                .value_name("FORMAT")
                .value_parser(|value: &str| serde_json::from_value::<Format>(value.into()))
                .help("How a log line is laid out: text or json. Default: text."),
            Arg::new("log-show-tid")
                .long("log-show-tid")
                .env("LOG_SHOW_TID")
                .value_name("BOOL")
                .value_parser(value_parser!(bool))
                .help("Whether a log line carries the thread id. Default: true."),
            Arg::new("log-show-thread-name")
                .long("log-show-thread-name")
                .env("LOG_SHOW_THREAD_NAME")
                .value_name("BOOL")
                .value_parser(value_parser!(bool))
                .help("Whether a log line carries the thread name. Default: true."),
            Arg::new("log-show-target")
                .long("log-show-target")
                .env("LOG_SHOW_TARGET")
                .value_name("BOOL")
                .value_parser(value_parser!(bool))
                .help("Whether a log line carries the target, i.e. the module that logged it. Default: true."),
            Arg::new("log-show-file-line")
                .long("log-show-file-line")
                .env("LOG_SHOW_FILE_LINE")
                .value_name("BOOL")
                .value_parser(value_parser!(bool))
                .help("Whether a log line carries the source file and line. Default: true."),
            Arg::new("log-show-id")
                .long("log-show-id")
                .env("LOG_SHOW_ID")
                .value_name("BOOL")
                .value_parser(value_parser!(bool))
                .help("Whether a log line carries the instance id. Default: true."),
        ])
}

fn patch_config(cfg: &mut Config, matches: &mut ArgMatches) {
    let dragonball = &mut cfg.dragonball;
    dragonball.id = matches.remove_one("id").or(dragonball.id.take());
    dragonball.api_sock = matches
        .remove_one("api-sock")
        .or(dragonball.api_sock.take());
    dragonball.kvm_dev = matches.remove_one("kvm-dev").or(dragonball.kvm_dev.take());

    let logger = &mut dragonball.logger;
    logger.level = matches.remove_one("log-level").or(logger.level.take());
    logger.target = matches.remove_one("log-target").or(logger.target.take());
    logger.crash_target = matches
        .remove_one("log-crash-target")
        .or(logger.crash_target.take());
    logger.format = matches.remove_one("log-format").or(logger.format.take());
    logger.show_tid = matches
        .remove_one("log-show-tid")
        .or(logger.show_tid.take());
    logger.show_thread_name = matches
        .remove_one("log-show-thread-name")
        .or(logger.show_thread_name.take());
    logger.show_target = matches
        .remove_one("log-show-target")
        .or(logger.show_target.take());
    logger.show_file_line = matches
        .remove_one("log-show-file-line")
        .or(logger.show_file_line.take());
    logger.show_id = matches.remove_one("log-show-id").or(logger.show_id.take());
}

fn apply_defaults(cfg: &mut Config) {
    cfg.dragonball
        .id
        .get_or_insert_with(|| names::Generator::default().next().unwrap());
    cfg.dragonball.kvm_dev.get_or_insert("/dev/kvm".into());

    let logger = &mut cfg.dragonball.logger;
    logger.level.get_or_insert(logger::LevelFilter::Info);
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

    use api::logger::UpdateLogger;

    use super::*;
    use crate::config::DragonballConfig;

    #[test]
    fn cli_patch_defaults_and_accessors_work_together() {
        let mut config = Config {
            dragonball: DragonballConfig {
                id: Some("from-file".into()),
                api_sock: None,
                kvm_dev: None,
                logger: UpdateLogger {
                    level: Some(logger::LevelFilter::Warn),
                    show_tid: Some(true),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let mut matches = command()
            .mut_args(|arg| arg.env(None::<&str>))
            .try_get_matches_from([
                "dragonball-evolved",
                "--api-sock",
                "/tmp/dragonball.sock",
                "--log-target",
                "stderr",
                "--log-show-tid",
                "false",
            ])
            .unwrap();

        patch_config(&mut config, &mut matches);
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

        let logger = config.dragonball.logger;
        assert_eq!(logger.level, Some(logger::LevelFilter::Warn));
        assert_eq!(logger.target, Some(Target::Stderr));
        assert_eq!(logger.show_tid, Some(false));
        assert_eq!(logger.show_id, Some(true));
    }
}
