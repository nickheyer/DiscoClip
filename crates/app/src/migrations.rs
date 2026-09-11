//! The application's tables, as a versioned list; the engine keeps its own.

use discoclip_engine::StoreError;
use discoclip_engine::store::migrate::Migration;
use discoclip_engine::store::sqlite::SqliteStore;

pub const SCOPE: &str = "app";

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "settings",
        sql: "
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    source TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
",
    },
    Migration {
        version: 2,
        name: "users",
        sql: "
CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    password_hash TEXT,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX users_username ON users(username COLLATE NOCASE);
",
    },
    Migration {
        version: 3,
        name: "sessions",
        sql: "
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    user_agent TEXT,
    ip TEXT
);
CREATE INDEX sessions_user ON sessions(user_id, created_at DESC);
",
    },
    Migration {
        version: 4,
        name: "oauth_identities",
        sql: "
CREATE TABLE oauth_identities (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    username TEXT,
    display_name TEXT,
    email TEXT,
    scope TEXT,
    access_token TEXT NOT NULL,
    refresh_token TEXT,
    expires_at INTEGER,
    linked_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (provider, subject),
    UNIQUE (user_id, provider)
);
",
    },
    Migration {
        version: 5,
        name: "api_tokens",
        sql: "
CREATE TABLE api_tokens (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scopes TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    expires_at INTEGER
);
CREATE INDEX api_tokens_user ON api_tokens(user_id, created_at DESC);
",
    },
    Migration {
        version: 6,
        name: "discord_guilds",
        sql: "
CREATE TABLE discord_guilds (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    guild_id TEXT NOT NULL,
    name TEXT NOT NULL,
    icon TEXT,
    owner INTEGER NOT NULL,
    permissions TEXT NOT NULL,
    manageable INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, guild_id)
);
",
    },
    Migration {
        version: 7,
        name: "discord_applications",
        sql: "
CREATE TABLE discord_applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    client_id TEXT NOT NULL UNIQUE,
    client_secret TEXT,
    bot_token TEXT NOT NULL,
    login INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
",
    },
    Migration {
        version: 8,
        name: "bot_guilds",
        sql: "
CREATE TABLE bot_guilds (
    application_id TEXT NOT NULL REFERENCES discord_applications(id) ON DELETE CASCADE,
    guild_id TEXT NOT NULL,
    name TEXT NOT NULL,
    icon TEXT,
    member_count INTEGER,
    joined_at INTEGER NOT NULL,
    left_at INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (application_id, guild_id)
);
",
    },
    Migration {
        version: 9,
        name: "watch_rules",
        sql: "
CREATE TABLE watch_rules (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES discord_applications(id) ON DELETE CASCADE,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    post_to TEXT,
    allow_hosts TEXT NOT NULL,
    allow_users TEXT NOT NULL,
    allow_roles TEXT NOT NULL,
    max_source_bytes INTEGER,
    max_duration_secs INTEGER,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (application_id, channel_id)
);
CREATE INDEX watch_rules_guild ON watch_rules(application_id, guild_id);
",
    },
    Migration {
        version: 10,
        name: "application_commands",
        sql: "
ALTER TABLE discord_applications ADD COLUMN commands_mode TEXT NOT NULL DEFAULT 'global';
ALTER TABLE discord_applications ADD COLUMN commands_guilds TEXT NOT NULL DEFAULT '[]';
ALTER TABLE discord_applications ADD COLUMN commands_registered_at INTEGER;
ALTER TABLE discord_applications ADD COLUMN commands_error TEXT;
",
    },
    Migration {
        version: 11,
        name: "application_enabled",
        sql: "
ALTER TABLE discord_applications ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
",
    },
    Migration {
        version: 12,
        name: "watch_rule_max_height",
        sql: "
ALTER TABLE watch_rules ADD COLUMN max_height INTEGER;
",
    },
];

/// Brings the application's tables up to date; returns how many migrations ran.
pub async fn apply(db: &SqliteStore) -> Result<usize, StoreError> {
    db.migrate(SCOPE, MIGRATIONS).await
}
