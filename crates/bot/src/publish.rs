use std::sync::Arc;

use twilight_http::Client;

use crate::config::WatchRule;

pub struct DiscordPublisher {
    http: Arc<Client>,
    rules: Vec<WatchRule>,
}
