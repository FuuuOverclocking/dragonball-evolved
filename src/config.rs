use std::path::PathBuf;

pub struct Config {
    pub dragonball: DragonballConfig,
    pub machine: MachineConfig,
}

pub struct DragonballConfig {
    id: Option<String>,
    api_sock: Option<PathBuf>,
    kvm_dev: Option<PathBuf>,
    logger: logger_backend::Config,
}

/// 暂时放这儿.
pub struct MachineConfig {}
