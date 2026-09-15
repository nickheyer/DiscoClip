//! Records the exchanges resolving links makes into a fixture file, for tests to replay:
//! `cargo run -p discoclip-engine --example record -- [--resolver <id>] <fixture.json>
//! "<notes>" <url>...`. With `--resolver`, only that resolver runs and the players it
//! redirects to are left out of the recording; otherwise the whole registry resolves each
//! link, redirects included. The fixture is named after the resolver that takes the first
//! link; cookies and credentials are redacted as they are recorded. Discord message links are read with the
//! bot tokens in `DISCOCLIP_DISCORD_TOKENS`, comma separated.

use std::path::PathBuf;
use std::sync::Arc;

use discoclip_engine::http::{Fixture, Http, HttpConfig};
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
    let mut args = std::env::args().skip(1).peekable();
    let only = if args.peek().map(String::as_str) == Some("--resolver") {
        args.next();
        args.next()
    } else {
        None
    };
    let (Some(path), Some(notes)) = (args.next(), args.next()) else {
        eprintln!("usage: record [--resolver <id>] <fixture.json> \"<notes>\" <url>...");
        std::process::exit(2);
    };
    let urls: Vec<Url> = args
        .map(|arg| Url::parse(&arg).unwrap_or_else(|error| panic!("{arg}: not a URL: {error}")))
        .collect();
    let Some(first) = urls.first() else {
        eprintln!("usage: record [--resolver <id>] <fixture.json> \"<notes>\" <url>...");
        std::process::exit(2);
    };
    let path = PathBuf::from(path);
    let probe = Http::new(HttpConfig::default());
    let name = only.clone().unwrap_or_else(|| {
        ResolverRegistry::new(standard_resolvers(&probe, bot_tokens()))
            .find(first)
            .map(|r| r.id().to_string())
            .unwrap_or_else(|| "web".to_string())
    });
    drop(probe);
    let mut fixture = Fixture::new(name, Some(first));
    fixture.notes = Some(notes);
    let http = Http::recording(HttpConfig::default(), path.clone(), fixture);
    let list = standard_resolvers(&http, bot_tokens());
    for resolver in &list {
        http.seed_cookies(resolver.id(), resolver.consent_cookies());
    }
    let registry = ResolverRegistry::new(list);
    for url in &urls {
        let outcome = match &only {
            Some(id) => match registry.get(id) {
                Some(resolver) => resolver.resolve(url).await,
                None => {
                    eprintln!("no resolver is called {id}");
                    std::process::exit(2);
                }
            },
            None => registry.resolve(url).await,
        };
        match outcome {
            Ok(Resolution::Media(media)) => println!(
                "{url}: {:?} with {} variant(s)",
                media.title,
                media.variants.len()
            ),
            Ok(Resolution::Playlist(playlist)) => println!(
                "{url}: playlist {:?} with {} entries",
                playlist.title,
                playlist.entries.len()
            ),
            Err(error) => println!("{url}: ERROR {error}"),
        }
    }
    // The recording is written when its last handle goes.
    drop(registry);
    drop(http);
    println!("recorded to {}", path.display());
}
