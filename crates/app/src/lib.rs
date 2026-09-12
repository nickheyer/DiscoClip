//! The DiscoClip server: provisioning, settings, accounts, the web app, and the composition
//! of them with the engine and the Discord bot. The binary in `main.rs` only parses arguments.

pub mod applications;
pub mod args;
pub mod audit;
pub mod bots;
pub mod commands;
pub mod compose;
pub mod config;
pub mod db;
pub mod discord;
pub mod live;
pub mod local;
pub mod migrations;
pub mod oauth;
pub mod ratelimit;
pub mod rules;
pub mod secrets;
pub mod sessions;
pub mod settings;
pub mod telemetry;
pub mod tokens;
pub mod users;
pub mod web;
