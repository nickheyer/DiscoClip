# TODOS

Progress: 31%

1. `crates/app/src/web.rs`: `WebApp` serving the embedded SVELTE frontend with sessions, optional TLS, reverse-proxy awareness.
2. Web: dashboard with live SSE job feed, queue depth, worker utilisation, bot state.
3. Web: jobs list, filters, detail with stage log and artifacts, download, retry, cancel, delete, bulk actions.
4. Web: settings pages for every setting.
5. Web: users, roles, sessions, API tokens.
6. Web: Discord applications, guilds, channels, rules.
7. Web: platform coverage page with formats and last fixture pass date.
8. Web: health, metrics, log viewer.
9. Resolver machinery: per-platform cookie jars and logged-in sessions managed in the app, proxies, rate limiting, retries, JS interpreter for signature ciphers, embed following, short-link unwrapping, age and consent gates.
10. Resolver: YouTube (videos, shorts, live, premieres, age-gated, playlists, clips).
11. Resolvers: TikTok, Instagram, Facebook, Twitter/X native.
12. Resolvers: Twitch, Kick, Vimeo, Dailymotion, Streamable, Imgur, Redgifs.
13. Resolvers: Bilibili, Niconico, Douyin, Kuaishou, Weibo, Xiaohongshu, VK.
14. Resolvers: Rumble, Odysee, Bluesky, Mastodon, Threads, Tumblr, Pinterest, LinkedIn, Snapchat, Loom, Telegram.
15. Resolvers: Discord attachments and CDN, 9GAG, iFunny, Newgrounds, Archive.org, Wikimedia Commons, Coub, Giphy, Tenor, Catbox, Google Drive, Dropbox, OneDrive, Mega.
16. Resolvers: embedded players: JW Player, Brightcove, Wistia, Kaltura, Vidyard, Cloudflare Stream, Mux, Bunny, YouTube/Vimeo/Twitch/Streamable embeds.
17. Resolvers: every remaining yt-dlp extractor, alphabetically, each with fixtures.
18. Download: progressive HTTP with ranges, resume, parallel chunks.
19. Download: HLS SAMPLE-AES, live and event playlists with refresh, discontinuities, subtitle renditions, live-to-VOD capture.
20. Download: DASH SegmentBase, SegmentList, SegmentTemplate, SegmentTimeline, multi-period, live; CENC reported as DRM.
21. Download: Smooth Streaming, RTMP, RTSP, RTP, WebRTC WHEP.
22. Download: MSE-only players through a headless browser.
23. Transcode: every container and codec ffmpeg decodes as input; HDR tone-mapping; interlacing; VFR; 360°; audio-only over a still; subtitle burn-in.
24. Transcode: per-destination targets, Discord tier and Nitro limits, hardware encoders, per-guild overrides.
25. Ops: job and cache retention, archive browsing, backups.
26. Ops: Docker image, compose file, systemd unit, Prometheus metrics, health endpoint.
27. Graceful shutdown across engine, web and bot.
