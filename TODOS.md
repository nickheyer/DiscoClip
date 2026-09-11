# TODOS

Progress: 28%

1. Audit log of settings changes and Discord management actions.
2. `crates/app/src/web.rs`: `WebApp` serving the embedded SVELTE frontend with sessions, optional TLS, reverse-proxy awareness.
3. Web: dashboard with live SSE job feed, queue depth, worker utilisation, bot state.
4. Web: jobs list, filters, detail with stage log and artifacts, download, retry, cancel, delete, bulk actions.
5. Web: settings pages for every setting.
6. Web: users, roles, sessions, API tokens.
7. Web: Discord applications, guilds, channels, rules.
8. Web: platform coverage page with formats and last fixture pass date.
9. Web: health, metrics, log viewer.
10. Resolver machinery: per-platform cookie jars and logged-in sessions managed in the app, proxies, rate limiting, retries, JS interpreter for signature ciphers, embed following, short-link unwrapping, age and consent gates.
11. Resolver: YouTube (videos, shorts, live, premieres, age-gated, playlists, clips).
12. Resolvers: TikTok, Instagram, Facebook, Twitter/X native.
13. Resolvers: Twitch, Kick, Vimeo, Dailymotion, Streamable, Imgur, Redgifs.
14. Resolvers: Bilibili, Niconico, Douyin, Kuaishou, Weibo, Xiaohongshu, VK.
15. Resolvers: Rumble, Odysee, Bluesky, Mastodon, Threads, Tumblr, Pinterest, LinkedIn, Snapchat, Loom, Telegram.
16. Resolvers: Discord attachments and CDN, 9GAG, iFunny, Newgrounds, Archive.org, Wikimedia Commons, Coub, Giphy, Tenor, Catbox, Google Drive, Dropbox, OneDrive, Mega.
17. Resolvers: embedded players: JW Player, Brightcove, Wistia, Kaltura, Vidyard, Cloudflare Stream, Mux, Bunny, YouTube/Vimeo/Twitch/Streamable embeds.
18. Resolvers: every remaining yt-dlp extractor, alphabetically, each with fixtures.
19. Download: progressive HTTP with ranges, resume, parallel chunks.
20. Download: HLS SAMPLE-AES, live and event playlists with refresh, discontinuities, subtitle renditions, live-to-VOD capture.
21. Download: DASH SegmentBase, SegmentList, SegmentTemplate, SegmentTimeline, multi-period, live; CENC reported as DRM.
22. Download: Smooth Streaming, RTMP, RTSP, RTP, WebRTC WHEP.
23. Download: MSE-only players through a headless browser.
24. Transcode: every container and codec ffmpeg decodes as input; HDR tone-mapping; interlacing; VFR; 360°; audio-only over a still; subtitle burn-in.
25. Transcode: per-destination targets, Discord tier and Nitro limits, hardware encoders, per-guild overrides.
26. Ops: job and cache retention, archive browsing, backups.
27. Ops: Docker image, compose file, systemd unit, Prometheus metrics, health endpoint.
28. Graceful shutdown across engine, web and bot.
