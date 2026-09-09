# Architecture

## Crates

| crate    | kind | role |
|----------|------|------|
| `engine` | lib  | source-agnostic pipeline: resolve, download, transcode, publish, archive |
| `bot`    | lib  | Discord adapter: watches channels, submits requests, publishes results |
| `ui`     | lib  | embedded web UI and JSON/SSE API over the engine |
| `app`    | bin  | composition root: config, logging, wiring, lifecycle |

Dependencies flow one way: `app -> {bot, ui} -> engine`. The engine knows
nothing about Discord or HTTP serving.

## Pipeline

```
Request { origin, url }
  -> resolve    ResolverRegistry picks a Resolver by URL, returns Resolved { variants }
  -> download   a Downloader fetches the chosen Variant into cache_dir
  -> transcode  the Transcoder fits the file to a Target derived from the Publisher's Constraints
  -> publish    the Publisher registered for origin.source uploads the result
  -> archive    an optional Archiver keeps a copy
```

Every stage is a trait. The `Engine` owns one resolver registry, a list of
downloaders, one transcoder, one publisher per `SourceId`, an optional
archiver, and a `JobStore`. Job status and progress go out on a broadcast
channel that `bot` and `ui` subscribe to through `EngineHandle`.

`Origin` is opaque to the engine: a `SourceId` plus a source-specific
reference string. The publisher for that source decodes it.

## Constraints

- No runtime system dependencies. TLS is rustls, transcoding runs in-process,
  storage is embedded. The release binary runs alone.
- UI assets are compiled into the binary from `crates/ui/assets`.
- One TOML config file, `discoclip.toml`, with a section per crate.

## Stack

| concern     | choice |
|-------------|--------|
| runtime     | tokio |
| Discord     | twilight |
| HTTP client | reqwest (rustls) |
| HTTP server | axum |
| assets      | rust-embed |
| config      | serde + toml |
| errors      | thiserror |
| ids         | uuid v7 |
| time        | jiff |
