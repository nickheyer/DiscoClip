# TODOS

REMOVE EACH ITEM AS YOU FINISH IT. DO NOT PROGRESS UNTIL YOUVE COMPLETED THE CURRENT ITEM IN ITS ENTIRETY. MUST DO IN ORDER.

9. Download: DASH SegmentBase, SegmentList, SegmentTemplate, SegmentTimeline, multi-period, live; CENC reported as DRM.
10. Download: Smooth Streaming, RTMP, RTSP, RTP, WebRTC WHEP.
11. Download: MSE-only players through a headless browser.
12. Transcode: every container and codec ffmpeg decodes as input; HDR tone-mapping; interlacing; VFR; 360°; audio-only over a still; subtitle burn-in.
13. Transcode: per-destination targets, Discord tier and Nitro limits, hardware encoders, per-guild overrides.
14. Ops: job and cache retention backups.
15. Ops: Docker image, compose file, systemd unit, Prometheus metrics, health endpoint.
16. Graceful shutdown across engine, web and bot.

## Resolvers to build

Ranked by traffic in Similarweb's August 2026 global, streaming, adult and music
categories, by how often the platform's links turn up in Discord chats.
Each is done when videos, audio, images or files of every link shape resolve with fixtures,
media kinds declared, sessions where the site needs them, and the platforms page passing
in full.

1. SoundCloud — soundcloud.com, on.soundcloud.com, api.soundcloud.com: tracks, sets and
   playlists, user pages, likes, private share links; audio as progressive MP3 and HLS
   AAC/Opus through the client id the web app carries. Music category #3.
2. Pornhub — pornhub.com, pornhub.org, pornhubpremium.com (session), modelhub links:
   videos with MP4 by height and HLS, playlists, channels and models, age gate and
   region cookies. Global #25, adult #1.
3. xHamster — xhamster.com and its .desi/.one/.xxx mirrors, xhamsterlive out: videos
   with MP4 by height, HLS, galleries as image sets, users and channels. Global #27.
4. XVideos — xvideos.com and its country mirrors, xvideos.es: videos with MP4 and HLS,
   channels and profiles. Global #34.
5. XNXX — xnxx.com and mirrors: the same player as XVideos with its own pages.
6. Rutube — rutube.ru: videos, shorts, playlists and channels, live; HLS from the
   options API. Streaming #7.
7. OK.ru (Odnoklassniki) — ok.ru, odnoklassniki.ru, ok.ru/live: videos, live, groups,
   embeds; MP4 by height and HLS from the player data. Largest Russian social video
   after VK.
8. Naver TV — tv.naver.com, naver.me short links: clips, live, channels, playlists
   through the play API. Naver is global #21.
9. Kakao TV — tv.kakao.com: clips, live, channels through the katz API.
10. SOOP (AfreecaTV) — sooplive.co.kr, afreecatv.com, vod.sooplive.co.kr: recordings,
    clips, live, stations; HLS with the login the site wants for adult-flagged streams.
11. Medal.tv — medal.tv: game clips, users and categories; MP4 sources from the page's
    hydration data. The clip site Discord gaming communities share most.
12. Steam — store.steampowered.com, steamcommunity.com: store trailers and community
    videos and screenshots; MP4 and WebM sources, screenshots as images.
13. Patreon — patreon.com: posts with video, audio and images and attached files,
    creator pages and collections; public posts without a session, paid ones with it.
14. Spotify podcasts — open.spotify.com/episode, /show: episodes as MP4 audio and shows
    as playlists through the anonymous web token; music links are refused as DRM.
15. Mixcloud — mixcloud.com: shows, users and playlists; audio decrypted as the player
    does. Long-form DJ sets.
16. Audiomack — audiomack.com: songs, albums and playlists through the site's API.
17. PeerTube — any instance, detected by its API: videos, live, channels and accounts,
    playlists; WebTorrent MP4 and HLS files. The fediverse's video, alongside Mastodon.
18. Eporner — eporner.com: videos with MP4 by height, categories and profiles. Adult #6.
19. SpankBang — spankbang.com, spankbang.party: videos with MP4 by height and HLS,
    playlists and profiles. Adult #24.
20. Erome — erome.com: albums of videos and images, profiles. Adult #11.
21. Stripchat — stripchat.com: live rooms as HLS, offline rooms reported, model pages.
    Adult #4; the cam site not yet covered beside Chaturbate and BongaCams.
22. Dzen — dzen.ru: videos, channels and articles with video; MP4 and HLS from the
    player. Global #29.
23. MediaFire — mediafire.com: files and folders of any kind through the download page
    and folder API.
24. Pixeldrain — pixeldrain.com: files and lists of any kind through its API.
25. Gofile — gofile.io: files and folders of any kind through the guest account the
    site hands out.
26. TED — ted.com: talks with MP4 by height, subtitles in every language, playlists.
27. ZDF — zdf.de, zdfheute.de, zdftivi.de: videos, live channels and series; HLS and
    MP4 from the ptmd API. Germany's most shared broadcaster after ARD.
28. France Télévisions — francetvinfo.fr, france.tv, francetv links in articles:
    videos and live; HLS and DASH from the player API.
29. RaiPlay — raiplay.it, rai.it, rainews.it: videos, live channels, series and
    programmes; HLS and MP4 from the relinker, geo-gated ones reported.
30. TVer — tver.jp: episodes and series through the platform API with the Streaks
    playback tokens it issues, geo-gated ones reported. Streaming #37.
