//! Resolves links from the command line against the live network, printing what each
//! resolver found: `cargo run -p discoclip-engine --example resolve -- <url>...`. Variant
//! links are shortened unless `RESOLVE_FULL_URLS` is set.
//! Discord message links are read with the bot tokens in `DISCOCLIP_DISCORD_TOKENS`,
//! comma separated.

use std::sync::Arc;

use discoclip_engine::http::{Http, HttpConfig};
use discoclip_engine::resolve::{Resolution, ResolverRegistry, standard_resolvers};
use url::Url;


fn bot_tokens() -> Arc<dyn discoclip_engine::resolve::discord::BotTokens> {
    let tokens: Vec<String> = std::env::var("DISCOCLIP_DISCORD_TOKENS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();
    Arc::new(tokens)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let http = Http::new(HttpConfig::default());
    let list = standard_resolvers(&http, bot_tokens());
    for resolver in &list {
        http.seed_cookies(resolver.id(), resolver.consent_cookies());
    }
    let registry = Arc::new(ResolverRegistry::new(list));
    for arg in std::env::args().skip(1) {
        let url = match Url::parse(&arg) {
            Ok(url) => url,
            Err(error) => {
                println!("{arg}: not a URL: {error}");
                continue;
            }
        };
        let resolver = registry.find(&url).map(|r| r.id()).unwrap_or("none");
        let started = std::time::Instant::now();
        match registry.resolve(&url).await {
            Ok(Resolution::Media(media)) => {
                println!(
                    "{arg}\n  resolver {resolver} in {:.1}s: {:?} by {:?}, {:?}, live {}, {} variant(s), {} subtitle(s)",
                    started.elapsed().as_secs_f64(),
                    media.title,
                    media.uploader,
                    media.duration,
                    media.live,
                    media.variants.len(),
                    media.subtitles.len()
                );
                let full = std::env::var_os("RESOLVE_FULL_URLS").is_some();
                for v in media.variants.iter().take(if full { usize::MAX } else { 12 }) {
                    let shown = if full {
                        v.url.as_str().len()
                    } else {
                        v.url.as_str().len().min(100)
                    };
                    println!(
                        "    {} {:?} {:?}x{:?} {:?} {:?} {}",
                        v.kind.as_str(),
                        v.label,
                        v.width,
                        v.height,
                        v.video,
                        v.bitrate,
                        &v.url.as_str()[..shown]
                    );
                }
            }
            Ok(Resolution::Playlist(playlist)) => {
                println!(
                    "{arg}\n  resolver {resolver} in {:.1}s: playlist {:?} with {} entries (total {:?})",
                    started.elapsed().as_secs_f64(),
                    playlist.title,
                    playlist.entries.len(),
                    playlist.total
                );
                for entry in playlist.entries.iter().take(5) {
                    println!("    {:?} {}", entry.title, entry.url);
                }
            }
            Err(error) => println!(
                "{arg}\n  resolver {resolver} in {:.1}s: ERROR {error}",
                started.elapsed().as_secs_f64()
            ),
        }
    }
}
