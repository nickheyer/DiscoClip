//! A guild as the application's bot sees it: its channels with the rule watching each,
//! its roles, and its members by name, for choosing what a rule names.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;

use super::AppState;
use super::auth::{Auth, Identity, parse_id};
use super::error::ApiError;
use super::rules::may_edit;
use crate::applications::ApplicationId;
use crate::rules::RuleId;

/// What a channel is for, as Discord kinds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Announcement,
    Voice,
    Stage,
    Category,
    Forum,
    Media,
    Thread,
    Other,
}

impl From<ChannelType> for ChannelKind {
    fn from(kind: ChannelType) -> Self {
        match kind {
            ChannelType::GuildText => ChannelKind::Text,
            ChannelType::GuildAnnouncement => ChannelKind::Announcement,
            ChannelType::GuildVoice => ChannelKind::Voice,
            ChannelType::GuildStageVoice => ChannelKind::Stage,
            ChannelType::GuildCategory => ChannelKind::Category,
            ChannelType::GuildForum => ChannelKind::Forum,
            ChannelType::GuildMedia => ChannelKind::Media,
            ChannelType::AnnouncementThread
            | ChannelType::PublicThread
            | ChannelType::PrivateThread => ChannelKind::Thread,
            _ => ChannelKind::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildChannel {
    pub id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub parent_id: Option<String>,
    pub position: i32,
    /// The rule watching the channel, when one does.
    pub rule: Option<RuleId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildRole {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub position: i64,
    /// Held by an integration rather than given by hand.
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildMember {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub nick: Option<String>,
    pub avatar: Option<String>,
    pub bot: bool,
}

/// The bot of `id`, and `guild` as an id, once `identity` may see the guild's rules.
async fn bot_for(
    state: &AppState,
    identity: &Identity,
    id: &str,
    guild: &str,
) -> Result<(std::sync::Arc<Client>, Id<GuildMarker>), ApiError> {
    let id: ApplicationId = parse_id(id)?;
    may_edit(state, identity, guild).await?;
    state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let guild_id = guild
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or_else(|| ApiError::BadRequest(format!("guild {guild:?} is not a Discord id")))?;
    let http = state
        .bots
        .client(id)
        .ok_or_else(|| ApiError::Conflict("the application's bot is not running".into()))?;
    Ok((http, guild_id))
}

/// A Discord answer the bot could not get: the guild is out of reach, or Discord failed.
fn discord_error(what: &str, guild: &str, error: twilight_http::Error) -> ApiError {
    match error.kind() {
        twilight_http::error::ErrorType::Response { status, .. }
            if matches!(status.get(), 403 | 404) =>
        {
            ApiError::BadRequest(format!(
                "the bot cannot see {what} of guild {guild}: {error}"
            ))
        }
        _ => ApiError::BadGateway(format!("Discord could not list {what}: {error}")),
    }
}

/// The guild's channels in Discord's order, with the rule watching each.
pub async fn list_channels(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, guild)): Path<(String, String)>,
) -> Result<Json<Vec<GuildChannel>>, ApiError> {
    let (http, guild_id) = bot_for(&state, &identity, &id, &guild).await?;
    let application: ApplicationId = parse_id(&id)?;
    let rules = state.rules.list_for_guild(application, &guild).await?;
    let channels = http
        .guild_channels(guild_id)
        .await
        .map_err(|e| discord_error("the channels", &guild, e))?
        .models()
        .await
        .map_err(|e| ApiError::BadGateway(format!("Discord answered unexpectedly: {e}")))?;
    let mut listed: Vec<GuildChannel> = channels
        .into_iter()
        .map(|channel| {
            let id = channel.id.to_string();
            GuildChannel {
                rule: rules
                    .iter()
                    .find(|rule| rule.input.channel_id == id)
                    .map(|rule| rule.id),
                id,
                name: channel.name.unwrap_or_default(),
                kind: channel.kind.into(),
                parent_id: channel.parent_id.map(|p| p.to_string()),
                position: channel.position.unwrap_or(0),
            }
        })
        .collect();
    listed.sort_by(|a, b| {
        a.parent_id
            .cmp(&b.parent_id)
            .then(a.position.cmp(&b.position))
            .then(a.name.cmp(&b.name))
    });
    Ok(Json(listed))
}

/// The guild's roles, highest first.
pub async fn list_roles(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, guild)): Path<(String, String)>,
) -> Result<Json<Vec<GuildRole>>, ApiError> {
    let (http, guild_id) = bot_for(&state, &identity, &id, &guild).await?;
    let roles = http
        .roles(guild_id)
        .await
        .map_err(|e| discord_error("the roles", &guild, e))?
        .models()
        .await
        .map_err(|e| ApiError::BadGateway(format!("Discord answered unexpectedly: {e}")))?;
    let mut listed: Vec<GuildRole> = roles
        .into_iter()
        .map(|role| GuildRole {
            id: role.id.to_string(),
            name: role.name,
            color: role.colors.primary_color,
            position: role.position,
            managed: role.managed,
        })
        .collect();
    listed.sort_by(|a, b| b.position.cmp(&a.position).then(a.name.cmp(&b.name)));
    Ok(Json(listed))
}

#[derive(Debug, Deserialize)]
pub struct MemberQuery {
    pub q: String,
    #[serde(default = "twenty")]
    pub limit: u16,
}

fn twenty() -> u16 {
    20
}

/// The guild's members whose name or nickname starts with `q`.
pub async fn search_members(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, guild)): Path<(String, String)>,
    Query(query): Query<MemberQuery>,
) -> Result<Json<Vec<GuildMember>>, ApiError> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    if query.limit == 0 || query.limit > 100 {
        return Err(ApiError::BadRequest("limit is 1 to 100".into()));
    }
    let (http, guild_id) = bot_for(&state, &identity, &id, &guild).await?;
    let members = http
        .search_guild_members(guild_id, q)
        .limit(query.limit)
        .await
        .map_err(|e| discord_error("the members", &guild, e))?
        .models()
        .await
        .map_err(|e| ApiError::BadGateway(format!("Discord answered unexpectedly: {e}")))?;
    Ok(Json(
        members
            .into_iter()
            .map(|member| GuildMember {
                id: member.user.id.to_string(),
                username: member.user.name,
                display_name: member.user.global_name,
                nick: member.nick,
                avatar: member
                    .avatar
                    .or(member.user.avatar)
                    .map(|hash| hash.to_string()),
                bot: member.user.bot,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::oauth::Registry;
    use crate::settings::WebConfig;
    use crate::users::Role;
    use crate::web::testing::{Client, FakeDiscord, app_with_discord, wait_for};

    #[tokio::test]
    async fn channels_roles_and_members_come_through_the_bot() {
        let discord = FakeDiscord::start().await;
        discord.add_bot("bot-token-1", "1001", "Clipper", "secret-1");
        discord.add_channel_of("5", "100", "General", 4, None, 0);
        discord.add_channel_of("10", "100", "general", 0, Some("5"), 0);
        discord.add_channel_of("11", "100", "clips", 0, Some("5"), 1);
        discord.add_channel_of("12", "100", "news", 5, None, 2);
        discord.add_channel_of("13", "100", "lounge", 2, None, 3);
        discord.add_channel_of("14", "100", "help", 15, None, 4);
        discord.add_channel_of("15", "100", "a thread", 11, Some("10"), 0);
        discord.add_channel("20", "200", "elsewhere");
        discord.add_role("100", "500", "Clippers", 0xff0000, 5, false);
        discord.add_role("100", "501", "Bot", 0, 9, true);
        discord.add_role("100", "100", "@everyone", 0, 0, false);
        discord.add_member("100", "9", "nick", Some("Nick"), Some("nicky"), false);
        discord.add_member("100", "8", "nina", None, None, false);
        discord.add_member("100", "7", "noodle", None, None, true);
        let app = app_with_discord(
            WebConfig::default(),
            Registry::default(),
            false,
            discord.endpoints(),
        )
        .await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        wait_for("the bot to connect", || {
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            let path = format!("/api/discord/applications/{id}");
            async move {
                let (_, body) = client.get(&path).await;
                (body["bot"]["state"] == "connected").then_some(())
            }
        })
        .await;
        let base = format!("/api/discord/applications/{id}/guilds/100");
        admin
            .post(&format!("{base}/rules"), json!({"channel_id": "11"}))
            .await;

        let (status, body) = admin.get(&format!("{base}/channels")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let channels = body.as_array().unwrap();
        assert_eq!(channels.len(), 7);
        let by_id = |cid: &str| channels.iter().find(|c| c["id"] == cid).unwrap().clone();
        assert_eq!(by_id("5")["kind"], "category");
        assert_eq!(by_id("10")["kind"], "text");
        assert_eq!(by_id("10")["parent_id"], "5");
        assert_eq!(by_id("12")["kind"], "announcement");
        assert_eq!(by_id("13")["kind"], "voice");
        assert_eq!(by_id("14")["kind"], "forum");
        assert_eq!(by_id("15")["kind"], "thread");
        assert!(by_id("11")["rule"].is_string());
        assert!(by_id("10")["rule"].is_null());

        let (status, body) = admin.get(&format!("{base}/roles")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let roles = body.as_array().unwrap();
        assert_eq!(roles.len(), 3);
        assert_eq!(roles[0]["name"], "Bot");
        assert_eq!(roles[0]["managed"], true);
        assert_eq!(roles[1]["name"], "Clippers");
        assert_eq!(roles[1]["color"], 0xff0000);
        assert_eq!(roles[2]["name"], "@everyone");

        let (status, body) = admin.get(&format!("{base}/members?q=ni")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let members = body.as_array().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0]["username"], "nick");
        assert_eq!(members[0]["display_name"], "Nick");
        assert_eq!(members[0]["nick"], "nicky");
        assert_eq!(members[1]["username"], "nina");
        let (status, body) = admin.get(&format!("{base}/members?q=noo&limit=1")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body[0]["bot"], true);
        assert_eq!(
            admin.get(&format!("{base}/members?q=")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            admin.get(&format!("{base}/members?q=a&limit=0")).await.0,
            StatusCode::BAD_REQUEST
        );

        // A guild the bot is not in, a bad guild id, and an unknown application.
        let (status, body) = admin
            .get(&format!(
                "/api/discord/applications/{id}/guilds/999/channels"
            ))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("cannot see"));
        assert_eq!(
            admin
                .get(&format!("/api/discord/applications/{id}/guilds/x/roles"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            admin
                .get("/api/discord/applications/00000000-0000-0000-0000-000000000000/guilds/100/channels")
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // Who may look: operators, and viewers managing the guild on Discord.
        for (name, role) in [("op", Role::Operator), ("viewer", Role::Viewer)] {
            app.state
                .users
                .create(name, Some("battery staple"), role)
                .await
                .unwrap();
        }
        let mut op = Client::new(&app);
        op.login("op", "battery staple").await;
        assert_eq!(op.get(&format!("{base}/channels")).await.0, StatusCode::OK);
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        assert_eq!(
            viewer.get(&format!("{base}/channels")).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.get(&format!("{base}/roles")).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.get(&format!("{base}/members?q=n")).await.0,
            StatusCode::FORBIDDEN
        );

        // A stopped bot cannot look.
        admin
            .send(
                axum::http::Method::POST,
                &format!("/api/discord/applications/{id}/bot/stop"),
                None,
            )
            .await;
        let (status, body) = admin.get(&format!("{base}/channels")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        app.state.bots.retire(id.parse().unwrap()).await;
        let (status, _) = admin.get(&format!("{base}/channels")).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
