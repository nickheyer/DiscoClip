use std::sync::Arc;

use discoclip_engine::EngineHandle;
use twilight_gateway::Shard;
use twilight_http::Client;

use crate::config::BotConfig;
use crate::watch::Watcher;

pub struct Bot {
    config: BotConfig,
    engine: EngineHandle,
    http: Arc<Client>,
    shard: Shard,
    watcher: Watcher,
}
