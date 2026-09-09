use discoclip_engine::EngineHandle;

use crate::config::UiConfig;

pub struct UiServer {
    config: UiConfig,
    engine: EngineHandle,
}
