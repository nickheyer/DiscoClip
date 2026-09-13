//! Resolves links from the command line against the live network, printing what each
//! resolver found: `cargo run -p discoclip-engine --example resolve -- <url>...`.

use std::sync::Arc;

use discoclip_engine::http::{Http, HttpConfig};
use discoclip_engine::resolve::{Resolution, Resolver, ResolverRegistry};
use url::Url;

fn resolvers(http: &Http) -> Vec<Box<dyn Resolver>> {
    use discoclip_engine::resolve::*;
    vec![
        Box::new(youtube::YoutubeResolver::new(http.clone())),
        Box::new(x::XResolver::new(http.clone())),
        Box::new(tiktok::TiktokResolver::new(http.clone())),
        Box::new(instagram::InstagramResolver::new(http.clone())),
        Box::new(facebook::FacebookResolver::new(http.clone())),
        Box::new(reddit::RedditResolver::new(http.clone())),
        Box::new(twitch::TwitchResolver::new(http.clone())),
        Box::new(kick::KickResolver::new(http.clone())),
        Box::new(vimeo::VimeoResolver::new(http.clone())),
        Box::new(dailymotion::DailymotionResolver::new(http.clone())),
        Box::new(streamable::StreamableResolver::new(http.clone())),
        Box::new(imgur::ImgurResolver::new(http.clone())),
        Box::new(redgifs::RedgifsResolver::new(http.clone())),
        Box::new(bilibili::BilibiliResolver::new(http.clone())),
        Box::new(niconico::NiconicoResolver::new(http.clone())),
        Box::new(douyin::DouyinResolver::new(http.clone())),
        Box::new(kuaishou::KuaishouResolver::new(http.clone())),
        Box::new(weibo::WeiboResolver::new(http.clone())),
        Box::new(xiaohongshu::XiaohongshuResolver::new(http.clone())),
        Box::new(vk::VkResolver::new(http.clone())),
        Box::new(rumble::RumbleResolver::new(http.clone())),
        Box::new(odysee::OdyseeResolver::new(http.clone())),
        Box::new(bluesky::BlueskyResolver::new(http.clone())),
        Box::new(threads::ThreadsResolver::new(http.clone())),
        Box::new(tumblr::TumblrResolver::new(http.clone())),
        Box::new(pinterest::PinterestResolver::new(http.clone())),
        Box::new(linkedin::LinkedinResolver::new(http.clone())),
        Box::new(snapchat::SnapchatResolver::new(http.clone())),
        Box::new(loom::LoomResolver::new(http.clone())),
        Box::new(telegram::TelegramResolver::new(http.clone())),
        Box::new(mastodon::MastodonResolver::new(http.clone())),
        Box::new(web::WebResolver::new(http.clone())),
    ]
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
    let list = resolvers(&http);
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
                for v in media.variants.iter().take(12) {
                    println!(
                        "    {} {:?} {:?}x{:?} {:?} {:?} {}",
                        v.kind.as_str(),
                        v.label,
                        v.width,
                        v.height,
                        v.video,
                        v.bitrate,
                        &v.url.as_str()[..v.url.as_str().len().min(100)]
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
