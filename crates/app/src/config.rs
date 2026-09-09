use discoclip_bot::BotConfig;
use discoclip_engine::EngineConfig;
use discoclip_ui::UiConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub log: LogConfig,
    pub engine: EngineConfig,
    pub bot: BotConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    pub filter: String,
}
