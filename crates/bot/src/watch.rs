use discoclip_engine::EngineHandle;

use crate::config::WatchRule;

pub struct Watcher {
    rules: Vec<WatchRule>,
    engine: EngineHandle,
}
