//! What Discord knows about an account: the guilds it belongs to, fetched with the
//! account's Discord grant, and which of them it can manage. And what the bots know: the
//! guilds each application has been added to, kept up from the gateway.

use discoclip_engine::StoreError;
use discoclip_engine::reqwest;
use discoclip_engine::rusqlite::{self, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::applications::ApplicationId;
use crate::db::{nanos, timestamp, transact};
use crate::oauth::OAuthError;
use crate::users::UserId;

/// Discord permission bits that let a member manage a guild.
const ADMINISTRATOR: u64 = 1 << 3;
const MANAGE_GUILD: u64 = 1 << 5;

/// A guild as Discord lists it for the user.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RemoteGuild {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub owner: bool,
    /// The member's permission bits, as a decimal string.
    #[serde(default)]
    pub permissions: String,
}

impl RemoteGuild {
    /// Whether the member owns the guild or may manage it.
    pub fn manageable(&self) -> bool {
        let bits: u64 = self.permissions.parse().unwrap_or(0);
        self.owner || bits & (ADMINISTRATOR | MANAGE_GUILD) != 0
    }
}

/// A guild an account belongs to, as last fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub owner: bool,
    pub permissions: String,
    pub manageable: bool,
    pub fetched_at: Timestamp,
}

/// Lists the guilds `access_token` belongs to; needs the `guilds` scope.
pub async fn fetch_guilds(
    http: &reqwest::Client,
    guilds_url: &Url,
    access_token: &str,
) -> Result<Vec<RemoteGuild>, OAuthError> {
    let provider = || "Discord".to_string();
    let response = http
        .get(guilds_url.clone())
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|source| OAuthError::Http {
            provider: provider(),
            source,
        })?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::Refused {
            provider: provider(),
            status,
            body: body.chars().take(300).collect(),
        });
    }
    response.json().await.map_err(|source| OAuthError::Http {
        provider: provider(),
        source,
    })
}

#[derive(Clone)]
pub struct GuildStore {
    db: SqliteStore,
}

impl GuildStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Replaces what is stored for `user` with `guilds`.
    pub async fn replace(
        &self,
        user: UserId,
        guilds: &[RemoteGuild],
    ) -> Result<Vec<Guild>, StoreError> {
        let guilds = guilds.to_vec();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            tx.execute(
                "DELETE FROM discord_guilds WHERE user_id = ?1",
                params![user.to_string()],
            )?;
            let mut stored = Vec::with_capacity(guilds.len());
            for guild in guilds {
                let manageable = guild.manageable();
                tx.execute(
                    "INSERT INTO discord_guilds (user_id, guild_id, name, icon, owner, permissions, manageable, fetched_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (user_id, guild_id) DO UPDATE SET
                        name = excluded.name, icon = excluded.icon, owner = excluded.owner,
                        permissions = excluded.permissions, manageable = excluded.manageable,
                        fetched_at = excluded.fetched_at",
                    params![
                        user.to_string(),
                        guild.id,
                        guild.name,
                        guild.icon,
                        guild.owner,
                        guild.permissions,
                        manageable,
                        nanos(now),
                    ],
                )?;
                stored.push(Guild {
                    id: guild.id,
                    name: guild.name,
                    icon: guild.icon,
                    owner: guild.owner,
                    permissions: guild.permissions,
                    manageable,
                    fetched_at: now,
                });
            }
            Ok::<_, StoreError>(stored)
        })
        .await
    }

    /// `user`'s guilds, manageable ones first, then by name.
    pub async fn list_for(&self, user: UserId) -> Result<Vec<Guild>, StoreError> {
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(
                "SELECT guild_id, name, icon, owner, permissions, manageable, fetched_at FROM discord_guilds \
                 WHERE user_id = ?1 ORDER BY manageable DESC, name COLLATE NOCASE, guild_id",
            )?;
            let rows = stmt.query_map(params![user.to_string()], row_to_guild)?;
            Ok::<_, StoreError>(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Whether `user` may manage the guild `guild_id`, as last fetched.
    pub async fn can_manage(&self, user: UserId, guild_id: &str) -> Result<bool, StoreError> {
        let guild_id = guild_id.to_string();
        transact(&self.db, move |tx| {
            let manageable: Option<bool> = tx
                .query_row(
                    "SELECT manageable FROM discord_guilds WHERE user_id = ?1 AND guild_id = ?2",
                    params![user.to_string(), guild_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok::<_, StoreError>(manageable.unwrap_or(false))
        })
        .await
    }
}

fn row_to_guild(row: &rusqlite::Row<'_>) -> rusqlite::Result<Guild> {
    Ok(Guild {
        id: row.get(0)?,
        name: row.get(1)?,
        icon: row.get(2)?,
        owner: row.get(3)?,
        permissions: row.get(4)?,
        manageable: row.get(5)?,
        fetched_at: timestamp("discord_guilds.fetched_at", row.get(6)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{Role, UserStore};

    fn guild(id: &str, name: &str, owner: bool, permissions: u64) -> RemoteGuild {
        RemoteGuild {
            id: id.into(),
            name: name.into(),
            icon: None,
            owner,
            permissions: permissions.to_string(),
        }
    }

    #[test]
    fn manageable_means_owner_admin_or_manage_guild() {
        assert!(guild("1", "a", true, 0).manageable());
        assert!(guild("1", "a", false, ADMINISTRATOR).manageable());
        assert!(guild("1", "a", false, MANAGE_GUILD | 1).manageable());
        assert!(!guild("1", "a", false, 1 << 10).manageable());
        assert!(!guild("1", "a", false, 0).manageable());
        let mut bad = guild("1", "a", false, 0);
        bad.permissions = "not a number".into();
        assert!(!bad.manageable());
    }

    #[tokio::test]
    async fn guilds_are_replaced_per_user() {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let users = UserStore::new(db.clone());
        let a = users.set_up("a", "correct horse").await.unwrap().id;
        let b = users.create("b", None, Role::Viewer).await.unwrap().id;
        let store = GuildStore::new(db.clone());

        let stored = store
            .replace(
                a,
                &[
                    guild("2", "zeta", false, 0),
                    guild("1", "Alpha", true, 0),
                    guild("3", "Mid", false, MANAGE_GUILD),
                ],
            )
            .await
            .unwrap();
        assert_eq!(stored.len(), 3);
        store
            .replace(b, &[guild("9", "other", false, 0)])
            .await
            .unwrap();
        let names: Vec<(String, bool)> = store
            .list_for(a)
            .await
            .unwrap()
            .into_iter()
            .map(|g| (g.name, g.manageable))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Alpha".into(), true),
                ("Mid".into(), true),
                ("zeta".into(), false)
            ]
        );
        assert!(store.can_manage(a, "1").await.unwrap());
        assert!(!store.can_manage(a, "2").await.unwrap());
        assert!(!store.can_manage(a, "9").await.unwrap());
        assert!(!store.can_manage(b, "1").await.unwrap());

        let stored = store
            .replace(a, &[guild("1", "Alpha renamed", false, 0)])
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(store.list_for(a).await.unwrap()[0].name, "Alpha renamed");
        assert!(!store.can_manage(a, "1").await.unwrap());
        assert_eq!(store.list_for(b).await.unwrap().len(), 1);

        db.call(move |conn| {
            conn.execute("DELETE FROM users WHERE id = ?1", params![b.to_string()])?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(store.list_for(b).await.unwrap().is_empty());
    }
}

/// A guild an application's bot is, or was, in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotGuild {
    pub application_id: ApplicationId,
    pub guild_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: Option<u64>,
    /// Whether the bot is in the guild now.
    pub present: bool,
    pub joined_at: Timestamp,
    pub left_at: Option<Timestamp>,
    pub updated_at: Timestamp,
}

/// What a bot learned about a guild it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedGuild {
    pub guild_id: String,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: Option<u64>,
}

#[derive(Clone)]
pub struct BotGuildStore {
    db: SqliteStore,
}

const BOT_GUILD_SELECT: &str = "SELECT application_id, guild_id, name, icon, member_count, left_at IS NULL, \
     joined_at, left_at, updated_at";

impl BotGuildStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Records that `application`'s bot is in `guild`: a join when it was not before,
    /// else fresh details.
    pub async fn joined(
        &self,
        application: ApplicationId,
        guild: JoinedGuild,
    ) -> Result<BotGuild, StoreError> {
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            tx.execute(
                "INSERT INTO bot_guilds (application_id, guild_id, name, icon, member_count, joined_at, left_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?6)
                 ON CONFLICT (application_id, guild_id) DO UPDATE SET
                    name = excluded.name, icon = excluded.icon, member_count = excluded.member_count,
                    joined_at = CASE WHEN bot_guilds.left_at IS NULL THEN bot_guilds.joined_at ELSE excluded.joined_at END,
                    left_at = NULL, updated_at = excluded.updated_at",
                params![
                    application.to_string(),
                    guild.guild_id,
                    guild.name,
                    guild.icon,
                    guild.member_count.map(|n| n as i64),
                    nanos(now),
                ],
            )?;
            get_bot_guild(tx, application, &guild.guild_id)?
                .ok_or_else(|| StoreError::Corrupt("bot guild vanished after upsert".into()))
        })
        .await
    }

    /// Records that `application`'s bot is no longer in `guild_id`; whether it was.
    pub async fn left(
        &self,
        application: ApplicationId,
        guild_id: &str,
    ) -> Result<bool, StoreError> {
        let guild_id = guild_id.to_string();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            Ok::<_, StoreError>(
                tx.execute(
                    "UPDATE bot_guilds SET left_at = ?3, updated_at = ?3 \
                     WHERE application_id = ?1 AND guild_id = ?2 AND left_at IS NULL",
                    params![application.to_string(), guild_id, nanos(now)],
                )? > 0,
            )
        })
        .await
    }

    /// At login the gateway lists the guilds the bot is in; any other guild still marked
    /// present was left while the bot was away. Returns the ids marked left.
    pub async fn reconcile(
        &self,
        application: ApplicationId,
        present: Vec<String>,
    ) -> Result<Vec<String>, StoreError> {
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let mut stmt = tx.prepare(
                "SELECT guild_id FROM bot_guilds WHERE application_id = ?1 AND left_at IS NULL",
            )?;
            let stored = stmt
                .query_map(params![application.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut gone = Vec::new();
            for guild_id in stored {
                if !present.contains(&guild_id) {
                    tx.execute(
                        "UPDATE bot_guilds SET left_at = ?3, updated_at = ?3 WHERE application_id = ?1 AND guild_id = ?2",
                        params![application.to_string(), guild_id, nanos(now)],
                    )?;
                    gone.push(guild_id);
                }
            }
            Ok::<_, StoreError>(gone)
        })
        .await
    }

    /// Every guild `application`'s bot has been in, present ones first, then by name.
    pub async fn list_for(&self, application: ApplicationId) -> Result<Vec<BotGuild>, StoreError> {
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(&format!(
                "{BOT_GUILD_SELECT} FROM bot_guilds WHERE application_id = ?1 \
                 ORDER BY left_at IS NOT NULL, name COLLATE NOCASE, guild_id"
            ))?;
            let rows = stmt.query_map(params![application.to_string()], row_to_bot_guild)?;
            Ok::<_, StoreError>(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }
}

fn get_bot_guild(
    conn: &rusqlite::Connection,
    application: ApplicationId,
    guild_id: &str,
) -> Result<Option<BotGuild>, StoreError> {
    Ok(conn
        .query_row(
            &format!(
                "{BOT_GUILD_SELECT} FROM bot_guilds WHERE application_id = ?1 AND guild_id = ?2"
            ),
            params![application.to_string(), guild_id],
            row_to_bot_guild,
        )
        .optional()?)
}

fn row_to_bot_guild(row: &rusqlite::Row<'_>) -> rusqlite::Result<BotGuild> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let application_id: String = row.get(0)?;
    let member_count: Option<i64> = row.get(4)?;
    let left_at: Option<i64> = row.get(7)?;
    Ok(BotGuild {
        application_id: application_id
            .parse()
            .map_err(|e| corrupt(format!("bot guild application {application_id}: {e}")))?,
        guild_id: row.get(1)?,
        name: row.get(2)?,
        icon: row.get(3)?,
        member_count: member_count.map(|n| n.max(0) as u64),
        present: row.get(5)?,
        joined_at: timestamp("bot_guilds.joined_at", row.get(6)?)
            .map_err(|e| corrupt(e.to_string()))?,
        left_at: left_at
            .map(|at| timestamp("bot_guilds.left_at", at))
            .transpose()
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("bot_guilds.updated_at", row.get(8)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod bot_guild_tests {
    use super::*;
    use crate::applications::{ApplicationStore, Credentials};
    use crate::secrets::Keyring;

    fn joined(id: &str, name: &str) -> JoinedGuild {
        JoinedGuild {
            guild_id: id.into(),
            name: name.into(),
            icon: None,
            member_count: Some(3),
        }
    }

    #[tokio::test]
    async fn joins_leaves_and_reconciliation_are_tracked_per_application() {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let applications = ApplicationStore::new(db.clone(), Keyring::from_key([9; 32]));
        let credentials = || Credentials {
            bot_token: "t".into(),
            client_secret: None,
        };
        let a = applications
            .create("A", "1", credentials())
            .await
            .unwrap()
            .id;
        let b = applications
            .create("B", "2", credentials())
            .await
            .unwrap()
            .id;
        let store = BotGuildStore::new(db.clone());

        let first = store.joined(a, joined("100", "Clips")).await.unwrap();
        assert!(first.present);
        assert_eq!(first.member_count, Some(3));
        store.joined(a, joined("200", "Zeta")).await.unwrap();
        store.joined(b, joined("100", "Clips")).await.unwrap();
        assert!(store.left(a, "200").await.unwrap());
        assert!(!store.left(a, "200").await.unwrap());
        assert!(!store.left(a, "999").await.unwrap());
        let listed = store.list_for(a).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].guild_id, "100");
        assert!(listed[0].present);
        assert_eq!(listed[1].guild_id, "200");
        assert!(!listed[1].present);
        assert!(listed[1].left_at.is_some());
        assert_eq!(store.list_for(b).await.unwrap().len(), 1);

        // Rejoining keeps the row and starts a new stay.
        let again = store
            .joined(a, joined("200", "Zeta renamed"))
            .await
            .unwrap();
        assert!(again.present);
        assert!(again.left_at.is_none());
        assert!(again.joined_at > listed[1].joined_at);
        assert_eq!(again.name, "Zeta renamed");
        let same = store.joined(a, joined("100", "Clips")).await.unwrap();
        assert_eq!(same.joined_at, first.joined_at);

        // Login lists only 100: 200 was left meanwhile.
        let gone = store.reconcile(a, vec!["100".into()]).await.unwrap();
        assert_eq!(gone, vec!["200"]);
        let listed = store.list_for(a).await.unwrap();
        assert!(listed.iter().any(|g| g.guild_id == "200" && !g.present));
        assert!(store.list_for(b).await.unwrap()[0].present);

        applications.delete(b).await.unwrap();
        assert!(store.list_for(b).await.unwrap().is_empty());
    }
}
