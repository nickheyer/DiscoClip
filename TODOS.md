# TODOS

REMOVE EACH ITEM AS YOU FINISH IT. DO NOT PROGRESS UNTIL YOUVE COMPLETED THE CURRENT ITEM IN ITS ENTIRETY. MUST DO IN ORDER.

1. We currently "have" 102 "resolvers" though its totally unclear if/what they resolve. Though we are going to support some resolvers that might resolve file data of a non video format. Discoclip is basically just going to become a discord interface ytdlp + file scraper (not just like arbitrary files, but intended file scraping).
2. A profile will be the configuration container that is generically applicable at various levels. Each platform needs a toggle, and this goes in a global profile (default settings), but profiles can be set per server, per channel, or per user even.
3. DiscoClip can host any number of "public" front ends, these also take a profile. A front end's primary use will be as a content browser that contains all the videos/files that it's profile scopes it to whether that be per platform, per discord server (default), per discord channel, or just everything. Oauth methods we already have can make this an authenticated front end, and discord oauth can even make only users apart of the scoped server/channel/user-list/whatever have access. There are more auth methods too, set a pin/token/password, and if it's easy enough for you - then you could add a little user/pass store too. Frontend's profile can enable/disable the ability to download videos as well. Should also be able to watch recorded streams here. Finally (and this is the most useful part I think) - when a file is too big, or its transcoded resolution/bitrate is below a user defined (default reasonable) threshold, instead of posting the video in discord, you post the public link to either the source downloaded content or a frontend acceptable transcoding (look into what we need to set in our page's data in order for discord's own link viewer so that it actually plays the video in its embedded player with only just the link). 
4. I'd like you to determine the top 20 or 30 yt-dlp (or even beyond yt-dlp if needed) platforms that we need a resolver for, put them into a list below these todos. We will implement each of these completely without excuses or caveats.
5. Another setting to add to profile, will be closer to a preset filter. Things like basic, sfw, nsfw, news, etc, This will white list platforms with one or more toggles (they are additive).
6. You need to implement caching or something for this discord interface you are aggregating info through, it frequently blocks the webserver and locks up the browser with a loading screen, ie: resp{endpoint=Endpoint { method: Get, path: "guilds/271722542262255626/channels" }}: exhausted reset_after=31.751809858s
7. Download: progressive HTTP with ranges, resume, parallel chunks.
8. Download: HLS SAMPLE-AES, live and event playlists with refresh, discontinuities, subtitle renditions, live-to-VOD capture.
9. Download: DASH SegmentBase, SegmentList, SegmentTemplate, SegmentTimeline, multi-period, live; CENC reported as DRM.
10. Download: Smooth Streaming, RTMP, RTSP, RTP, WebRTC WHEP.
11. Download: MSE-only players through a headless browser.
12. Transcode: every container and codec ffmpeg decodes as input; HDR tone-mapping; interlacing; VFR; 360°; audio-only over a still; subtitle burn-in.
13. Transcode: per-destination targets, Discord tier and Nitro limits, hardware encoders, per-guild overrides.
14. Ops: job and cache retention backups.
15. Ops: Docker image, compose file, systemd unit, Prometheus metrics, health endpoint.
16. Graceful shutdown across engine, web and bot.
