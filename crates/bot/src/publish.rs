//! Posts finished media to Discord as the policy in force where the link was seen says:
//! the file or a link to the page that plays it, as a reply, a plain post, or in place of
//! the message that carried the link, with the lines the policy includes. A live capture
//! is announced when it begins, and that message is edited with the result.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use discoclip_engine::job::{Delivery, Job, Origin, SourceId, Stage};
use discoclip_engine::media::{LocalFile, safe_stem};
use discoclip_engine::policy::{UploadLimit, UploadPolicy};
use discoclip_engine::publish::{Constraints, LinkTarget, PublishError, Published, Publisher};
use jiff::Timestamp;
use tokio::sync::Mutex;
use twilight_http::error::ErrorType;
use twilight_http::response::ResponseFuture;
use twilight_http::{Client, Response};
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::channel::{ChannelType, Message};
use twilight_model::guild::{Permissions, PremiumTier};
use twilight_model::http::attachment::Attachment;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, WebhookMarker};
use url::Url;
use uuid::Uuid;

use crate::config::{DiscordSettings, SharedDiscordSettings};
use crate::directory::{Directories, Directory, MemberInfo};
use crate::link::LinkTargets;
use crate::origin::{DiscordOrigin, SOURCE_ID};
use crate::policy::{
    DiscordPolicy, OriginalEmbeds, PermissionMode, Placement, ReplaceAs, Requester,
};

/// How long a boost tier read over REST stands for a guild the directory has not loaded
const TIER_CACHE: Duration = Duration::from_secs(300);
/// How long a REST lookup may wait behind Discord's rate limiter before it is given up.
const REST_WAIT: Duration = Duration::from_secs(10);
const MIB: u64 = 1024 * 1024;
/// What Discord takes from a bot in an unboosted server or a direct message
const BASE_LIMIT: u64 = 10 * MIB;
/// Every limit Discord grants, largest first, which a refused upload steps down through
const LIMIT_LADDER: [u64; 3] = [100 * MIB, 50 * MIB, BASE_LIMIT];
/// The most characters a message holds
const MESSAGE_MAX: usize = 2000;
const WEBHOOK_NAME: &str = "DiscoClip";
const ISSUES: &str = "https://github.com/nickheyer/DiscoClip/issues";

/// What a server's boost level lets a bot upload
fn limit_for_tier(tier: PremiumTier) -> u64 {
    match tier {
        PremiumTier::Tier2 => 50 * MIB,
        PremiumTier::Tier3 => 100 * MIB,
        _ => BASE_LIMIT,
    }
}

/// The next limit down the ladder, when there is one
fn next_lower(limit: u64) -> Option<u64> {
    LIMIT_LADDER.iter().copied().find(|step| *step < limit)
}

/// The REST clients of one running application. `lookups` answers within seconds or is
/// given up: guild details, link posts, replies. `uploads` carries attachments and is
/// given time in proportion to the file. Both share the token's rate limiter.
#[derive(Clone)]
pub struct DiscordClients {
    pub lookups: Arc<Client>,
    pub uploads: Arc<Client>,
}

/// The REST clients of the running applications, by the server's id for each. Shared with
/// whatever starts and stops the bots.
pub type Clients = Arc<RwLock<HashMap<Uuid, DiscordClients>>>;

/// The limit a guild was last seen to take, forgotten when its boost level changes
#[derive(Debug, Clone, Copy)]
struct KnownLimit {
    bytes: u64,
    tier: PremiumTier,
}

/// A webhook's id and token
type Webhook = (Id<WebhookMarker>, String);
/// The webhooks found or made, by application and channel
type Webhooks = HashMap<(Uuid, Id<ChannelMarker>), Webhook>;

pub struct DiscordPublisher {
    clients: Clients,
    directories: Directories,
    source: SourceId,
    /// Boost tiers read over REST for guilds the directory had not loaded, for a while.
    tiers: Mutex<HashMap<Id<GuildMarker>, (PremiumTier, Instant)>>,
    known: Mutex<HashMap<Id<GuildMarker>, KnownLimit>>,
    /// The webhook each application posts through per channel, once made or found
    webhooks: Mutex<Webhooks>,
    links: Arc<dyn LinkTargets>,
    settings: SharedDiscordSettings,
    /// Display names of the platforms, by resolver id
    platform_names: HashMap<String, String>,
}

/// Everything one post needs, settled before anything is sent
struct Posting {
    origin: DiscordOrigin,
    clients: DiscordClients,
    directory: Option<Arc<Directory>>,
    policy: DiscordPolicy,
    destination: Id<ChannelMarker>,
    /// The channel a thread hangs from, whose webhook posts into it
    thread_parent: Option<Id<ChannelMarker>>,
    placement: Placement,
    replace_as: ReplaceAs,
    suppress: bool,
    notes: Vec<String>,
}

impl Posting {
    fn can_reply(&self) -> bool {
        self.destination == self.origin.channel && self.origin.message.is_some()
    }
}

/// Why one send did not come back with a message
enum SendFailure {
    Http(twilight_http::Error),
    TimedOut,
}

fn is_status(error: &twilight_http::Error, code: u16) -> bool {
    matches!(error.kind(), ErrorType::Response { status, .. } if status.get() == code)
}

impl DiscordPublisher {
    pub fn new(
        clients: Clients,
        directories: Directories,
        links: Arc<dyn LinkTargets>,
        settings: SharedDiscordSettings,
        platform_names: HashMap<String, String>,
    ) -> Self {
        Self {
            clients,
            directories,
            source: SourceId::new(SOURCE_ID),
            tiers: Mutex::new(HashMap::new()),
            known: Mutex::new(HashMap::new()),
            webhooks: Mutex::new(HashMap::new()),
            links,
            settings,
            platform_names,
        }
    }

    fn settings(&self) -> DiscordSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn clients_for(&self, application: Uuid) -> Result<DiscordClients, PublishError> {
        self.clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&application)
            .cloned()
            .ok_or_else(|| {
                PublishError::Rejected(format!(
                    "discord application {application} is no longer configured"
                ))
            })
    }

    fn directory_for(&self, application: Uuid) -> Option<Arc<Directory>> {
        self.directories
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&application)
            .cloned()
    }

    /// The guild's boost tier: from what the gateway told the bot about the guild, or,
    /// for a guild the bot has not loaded, from Discord, remembered for a while.
    async fn guild_tier(
        &self,
        application: Uuid,
        http: &Client,
        guild: Id<GuildMarker>,
    ) -> Result<PremiumTier, PublishError> {
        let known = self
            .directory_for(application)
            .and_then(|directory| directory.guild(guild));
        if let Some(info) = known {
            return Ok(info.premium_tier);
        }
        if let Some((tier, at)) = self.tiers.lock().await.get(&guild)
            && at.elapsed() < TIER_CACHE
        {
            return Ok(*tier);
        }
        let model = tokio::time::timeout(REST_WAIT, http.guild(guild))
            .await
            .map_err(|_| {
                PublishError::Rejected(format!(
                    "Discord server lookup timed out after {}s. The API may be rate limited.",
                    REST_WAIT.as_secs()
                ))
            })?
            .map_err(|e| PublishError::Rejected(format!("guild lookup: {e}")))?
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("guild decode: {e}")))?;
        self.tiers
            .lock()
            .await
            .insert(guild, (model.premium_tier, Instant::now()));
        Ok(model.premium_tier)
    }

    /// The upload limit where `origin` posts, the policy's own or the one Discord was seen to take
    async fn upload_limit(
        &self,
        origin: &DiscordOrigin,
        http: &Client,
        policy: UploadPolicy,
    ) -> Result<u64, PublishError> {
        if let UploadLimit::Bytes(bytes) = policy.max_bytes {
            return Ok(bytes);
        }
        let Some(guild) = origin.guild else {
            return Ok(BASE_LIMIT);
        };
        let tier = self.guild_tier(origin.application, http, guild).await?;
        let mut known = self.known.lock().await;
        if let Some(limit) = known.get(&guild)
            && limit.tier == tier
        {
            return Ok(limit.bytes);
        }
        let bytes = limit_for_tier(tier);
        known.insert(guild, KnownLimit { bytes, tier });
        Ok(bytes)
    }

    /// The next limit down the ladder for `guild`, remembered, when there is one
    async fn lower_limit(&self, guild: Option<Id<GuildMarker>>, limit: u64) -> Option<u64> {
        let lower = next_lower(limit)?;
        if let Some(guild) = guild
            && let Some(known) = self.known.lock().await.get_mut(&guild)
        {
            known.bytes = lower;
        }
        Some(lower)
    }

    fn parse(origin: &Origin) -> Result<DiscordOrigin, PublishError> {
        DiscordOrigin::parse(origin)
            .ok_or_else(|| PublishError::InvalidOrigin(origin.reference.clone()))
    }

    /// Where a job's messages go, in Discord's terms a channel id: the channel the profile
    /// sends results to, else the channel the link was seen in.
    fn destination(job: &Job, origin: &DiscordOrigin) -> Id<ChannelMarker> {
        job.request
            .destination
            .as_deref()
            .and_then(|channel| channel.parse::<u64>().ok())
            .and_then(Id::new_checked)
            .unwrap_or(origin.channel)
    }

    /// The message that announced the job's capture, to be edited with the result.
    fn announced(job: &Job) -> Option<Id<MessageMarker>> {
        job.artifacts
            .announced
            .as_ref()
            .and_then(|published| published.reference.parse::<u64>().ok())
            .and_then(Id::new_checked)
    }

    fn channel_label(directory: Option<&Directory>, channel: Id<ChannelMarker>) -> String {
        directory
            .and_then(|d| d.channel(channel))
            .filter(|c| !c.name.is_empty())
            .map(|c| format!("#{}", c.name))
            .unwrap_or_else(|| format!("channel {channel}"))
    }

    /// Settles where and how a post goes, under the policy and the bot's permissions
    async fn posting(&self, job: &Job, uploading: bool) -> Result<Posting, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let clients = self.clients_for(origin.application)?;
        let policy = DiscordPolicy::of(&job.request).map_err(PublishError::Rejected)?;
        let destination = Self::destination(job, &origin);
        let directory = self.directory_for(origin.application);
        let channel = directory.as_ref().and_then(|d| d.channel(destination));
        let is_thread = channel.as_ref().is_some_and(|c| {
            matches!(
                c.kind,
                ChannelType::PublicThread
                    | ChannelType::PrivateThread
                    | ChannelType::AnnouncementThread
            )
        });
        let thread_parent = channel
            .as_ref()
            .filter(|_| is_thread)
            .and_then(|c| c.parent_id);
        let mut posting = Posting {
            origin,
            clients,
            directory,
            destination,
            thread_parent,
            placement: policy.message.placement,
            replace_as: policy.message.replace_as,
            suppress: policy.message.original_embeds == OriginalEmbeds::Suppress,
            notes: Vec::new(),
            policy,
        };
        if origin.message.is_none() {
            posting.placement = Placement::Post;
            posting.suppress = false;
        } else if posting.placement == Placement::Reply && !posting.can_reply() {
            posting.placement = Placement::Post;
        }
        if posting.policy.message.permissions == PermissionMode::Check
            && let Some(guild) = origin.guild
        {
            let dest_label = Self::channel_label(posting.directory.as_deref(), destination);
            let origin_label = Self::channel_label(posting.directory.as_deref(), origin.channel);
            let computed = posting.directory.as_ref().and_then(|d| {
                let me = d.current_user()?;
                let at_destination = d.permissions_in(me, destination);
                let at_origin = if origin.channel == destination {
                    at_destination.clone()
                } else {
                    d.permissions_in(me, origin.channel)
                };
                Some(at_destination.and_then(|dest| at_origin.map(|orig| (dest, orig))))
            });
            match computed {
                Some(Ok((at_destination, at_origin))) => apply_permissions(
                    &mut posting,
                    at_destination,
                    at_origin,
                    is_thread,
                    uploading,
                    &dest_label,
                    &origin_label,
                )?,
                Some(Err(why)) => posting.notes.push(format!(
                    "Permissions in {dest_label} could not be computed ({why}). Acting as \
                     configured."
                )),
                None => posting.notes.push(format!(
                    "Permissions in {dest_label} could not be computed (the bot's view of \
                     server {guild} has not loaded). Acting as configured."
                )),
            }
        }
        Ok(posting)
    }

    /// The name the requester goes by in the guild, when the bot has seen them
    fn requester_name(posting: &Posting) -> Option<String> {
        let author = posting.origin.author?;
        let guild = posting.origin.guild?;
        let member = posting.directory.as_ref()?.member(guild, author)?;
        Some(display_name(&member))
    }

    /// The name and avatar a replacement posts under, when the author is known here
    fn author_identity(posting: &Posting) -> Option<(String, String)> {
        let author = posting.origin.author?;
        let guild = posting.origin.guild?;
        let member = posting.directory.as_ref()?.member(guild, author)?;
        Some((
            webhook_name(&display_name(&member)),
            avatar_url(guild, &member),
        ))
    }

    /// The text posted with the media, line by line as the policy includes them
    fn compose(
        &self,
        job: &Job,
        posting: &Posting,
        page: Option<&Url>,
        requester: Option<&str>,
    ) -> (String, AllowedMentions) {
        let include = &posting.policy.message.include;
        let resolved = job.artifacts.resolved.as_ref();
        let mut lines: Vec<String> = Vec::new();
        let mut original_index = None;
        if posting.placement == Placement::Replace
            && let Some(text) = &posting.policy.original_text_value
            && !text.trim().is_empty()
        {
            original_index = Some(lines.len());
            lines.push(text.clone());
        }
        if include.title
            && let Some(title) = job.title()
        {
            lines.push(format!("**{}**", escape_markdown(title)));
        }
        let mut about: Vec<String> = Vec::new();
        if include.platform
            && let Some(resolver) = job.resolver()
        {
            about.push(
                self.platform_names
                    .get(resolver)
                    .cloned()
                    .unwrap_or_else(|| resolver.to_string()),
            );
        }
        if include.uploader
            && let Some(uploader) = resolved.and_then(|r| r.uploader.as_deref())
        {
            about.push(uploader.to_string());
        }
        if include.duration
            && let Some(duration) = job
                .artifacts
                .output
                .as_ref()
                .and_then(|o| o.info.as_ref())
                .and_then(|i| i.duration)
                .or_else(|| resolved.and_then(|r| r.duration))
        {
            about.push(clock(duration));
        }
        if !about.is_empty() {
            lines.push(about.join(" · "));
        }
        let mut allowed = AllowedMentions::default();
        match include.requester {
            Requester::None => {}
            Requester::Name => {
                if let Some(name) = requester {
                    lines.push(format!("Requested by {}", escape_markdown(name)));
                }
            }
            Requester::Mention => {
                if let Some(author) = posting.origin.author {
                    lines.push(format!("Requested by <@{author}>"));
                    allowed.users = vec![author];
                }
            }
        }
        if include.source_link {
            let source = resolved
                .and_then(|r| r.webpage_url.clone())
                .unwrap_or_else(|| job.request.url.clone());
            lines.push(format!("<{source}>"));
        }
        if include.earlier_post
            && let Some(earlier) = &job.artifacts.earlier_post
        {
            lines.push(format!("Posted before: <{earlier}>"));
        }
        if let Some(page) = page {
            lines.push(page.to_string());
        }
        if include.brand {
            lines.push("-# via DiscoClip".to_string());
        }
        (fit_lines(lines, original_index), allowed)
    }

    /// The webhook the application posts through in the posting's channel, found or made
    async fn webhook_for(&self, posting: &Posting) -> Result<Webhook, PublishError> {
        let channel = posting.thread_parent.unwrap_or(posting.destination);
        let key = (posting.origin.application, channel);
        if let Some(found) = self.webhooks.lock().await.get(&key).cloned() {
            return Ok(found);
        }
        let http = &posting.clients.lookups;
        let application = posting.directory.as_ref().and_then(|d| d.application_id());
        let existing = tokio::time::timeout(REST_WAIT, http.channel_webhooks(channel))
            .await
            .map_err(|_| PublishError::Rejected("webhook lookup timed out".into()))?
            .map_err(|e| PublishError::Rejected(format!("webhook lookup: {e}")))?
            .models()
            .await
            .map_err(|e| PublishError::Rejected(format!("webhook decode: {e}")))?;
        let mine = existing.into_iter().find(|webhook| {
            webhook.token.is_some()
                && (application.is_none() || webhook.application_id == application)
        });
        let (id, token) = match mine.and_then(|w| w.token.map(|token| (w.id, token))) {
            Some(found) => found,
            None => {
                let created =
                    tokio::time::timeout(REST_WAIT, http.create_webhook(channel, WEBHOOK_NAME))
                        .await
                        .map_err(|_| PublishError::Rejected("webhook creation timed out".into()))?
                        .map_err(|e| {
                            PublishError::Rejected(format!("could not create a webhook: {e}"))
                        })?
                        .model()
                        .await
                        .map_err(|e| PublishError::Rejected(format!("webhook decode: {e}")))?;
                let token = created.token.ok_or_else(|| {
                    PublishError::Rejected("Discord returned a webhook without its token".into())
                })?;
                (created.id, token)
            }
        };
        if let Some(directory) = &posting.directory {
            directory.add_own_poster(Id::new(id.get()));
        }
        self.webhooks.lock().await.insert(key, (id, token.clone()));
        Ok((id, token))
    }

    /// Sends one post as the posting says, within `budget` when there is one
    #[allow(clippy::too_many_arguments)]
    async fn send_once(
        &self,
        posting: &Posting,
        announced: Option<Id<MessageMarker>>,
        webhook: Option<&Webhook>,
        identity: Option<&(String, String)>,
        text: &str,
        allowed: &AllowedMentions,
        attachments: &[Attachment],
        budget: Option<Duration>,
    ) -> Result<Response<Message>, SendFailure> {
        let client = if attachments.is_empty() {
            &posting.clients.lookups
        } else {
            &posting.clients.uploads
        };
        let future: ResponseFuture<Message> = match (announced, webhook) {
            (Some(message), _) => {
                let mut request = client
                    .update_message(posting.destination, message)
                    .content(Some(text))
                    .allowed_mentions(Some(allowed));
                if !attachments.is_empty() {
                    request = request.attachments(attachments);
                }
                request.into_future()
            }
            (None, Some((id, token))) => {
                let mut request = client
                    .execute_webhook(*id, token)
                    .allowed_mentions(Some(allowed));
                if let Some((name, avatar)) = identity {
                    request = request.username(name).avatar_url(avatar);
                }
                if !text.is_empty() {
                    request = request.content(text);
                }
                if !attachments.is_empty() {
                    request = request.attachments(attachments);
                }
                if posting.thread_parent.is_some() {
                    request = request.thread_id(posting.destination);
                }
                request.wait().into_future()
            }
            (None, None) => {
                let mut request = client
                    .create_message(posting.destination)
                    .allowed_mentions(Some(allowed));
                if !text.is_empty() {
                    request = request.content(text);
                }
                if !attachments.is_empty() {
                    request = request.attachments(attachments);
                }
                if posting.placement == Placement::Reply
                    && let Some(message) = posting.origin.message
                {
                    request = request.reply(message).fail_if_not_exists(false);
                }
                request.into_future()
            }
        };
        let outcome = match budget {
            Some(budget) => tokio::time::timeout(budget, future)
                .await
                .map_err(|_| SendFailure::TimedOut)?,
            None => future.await,
        };
        outcome.map_err(SendFailure::Http)
    }

    /// Removes the message that carried the link and hides its embeds, as the posting says
    async fn tend_original(&self, posting: &mut Posting) -> Result<(), PublishError> {
        let Some(message) = posting.origin.message else {
            return Ok(());
        };
        let http = &posting.clients.lookups;
        if posting.placement == Placement::Replace {
            match http.delete_message(posting.origin.channel, message).await {
                Ok(_) => posting
                    .notes
                    .push("Removed the message that carried the link.".into()),
                Err(error) if is_status(&error, 404) => posting
                    .notes
                    .push("The message that carried the link was already gone.".into()),
                Err(error) => {
                    return Err(PublishError::Rejected(format!(
                        "the result was posted, but the message that carried the link could \
                         not be removed: {error}"
                    )));
                }
            }
            return Ok(());
        }
        if posting.suppress {
            match http
                .update_message(posting.origin.channel, message)
                .flags(MessageFlags::SUPPRESS_EMBEDS)
                .await
            {
                Ok(_) => posting
                    .notes
                    .push("Hid the embeds of the message that carried the link.".into()),
                Err(error) => {
                    posting.notes.push(format!(
                        "The original message's embeds were not hidden: {error}"
                    ));
                    if posting.policy.errors.debug {
                        self.report(
                            &posting.clients,
                            &posting.origin,
                            posting.destination,
                            format!(
                                "DiscoClip could not hide the embeds of the original message: \
                                 {error}\nReport: <{ISSUES}>"
                            ),
                            None,
                        )
                        .await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Posts `text` where the job's posts go, else where the link was seen, and never fails
    async fn report(
        &self,
        clients: &DiscordClients,
        origin: &DiscordOrigin,
        destination: Id<ChannelMarker>,
        text: String,
        attachment: Option<Attachment>,
    ) {
        let text: String = text.chars().take(MESSAGE_MAX).collect();
        let allowed = AllowedMentions::default();
        let attachments: Vec<Attachment> = attachment.into_iter().collect();
        let mut channels = vec![destination];
        if origin.channel != destination {
            channels.push(origin.channel);
        }
        for channel in channels {
            let mut request = clients
                .lookups
                .create_message(channel)
                .content(&text)
                .allowed_mentions(Some(&allowed));
            if !attachments.is_empty() {
                request = request.attachments(&attachments);
            }
            if channel == origin.channel
                && let Some(message) = origin.message
            {
                request = request.reply(message).fail_if_not_exists(false);
            }
            match request.await {
                Ok(_) => return,
                Err(error) => {
                    tracing::warn!(channel = %channel, "could not report the failure: {error}")
                }
            }
        }
    }
}

/// Settles the posting against what the bot may do, degrading in a fixed order and noting each step
fn apply_permissions(
    posting: &mut Posting,
    at_destination: Permissions,
    at_origin: Permissions,
    is_thread: bool,
    uploading: bool,
    dest_label: &str,
    origin_label: &str,
) -> Result<(), PublishError> {
    if posting.placement == Placement::Replace
        && posting.replace_as == ReplaceAs::Author
        && !at_destination.contains(Permissions::MANAGE_WEBHOOKS)
    {
        posting.replace_as = ReplaceAs::Bot;
        posting.notes.push(format!(
            "Replacing as the bot: the bot lacks Manage Webhooks in {dest_label}."
        ));
    }
    if posting.placement == Placement::Replace && !at_origin.contains(Permissions::MANAGE_MESSAGES)
    {
        posting.placement = if posting.can_reply() {
            Placement::Reply
        } else {
            Placement::Post
        };
        posting.notes.push(format!(
            "{} instead of replacing: the bot lacks Manage Messages in {origin_label}.",
            if posting.placement == Placement::Reply {
                "Replying"
            } else {
                "Posting"
            }
        ));
    }
    if posting.placement == Placement::Reply
        && !at_destination.contains(Permissions::READ_MESSAGE_HISTORY)
    {
        posting.placement = Placement::Post;
        posting.notes.push(format!(
            "Posting instead of replying: the bot lacks Read Message History in {dest_label}."
        ));
    }
    if posting.suppress && !at_origin.contains(Permissions::MANAGE_MESSAGES) {
        posting.suppress = false;
        posting.notes.push(format!(
            "Leaving the original embeds: the bot lacks Manage Messages in {origin_label}."
        ));
    }
    let forbidden = |missing: &str| PublishError::Forbidden {
        channel: dest_label.to_string(),
        missing: missing.to_string(),
    };
    if !at_destination.contains(Permissions::VIEW_CHANNEL) {
        return Err(forbidden("View Channel"));
    }
    if is_thread {
        if !at_destination.contains(Permissions::SEND_MESSAGES_IN_THREADS) {
            return Err(forbidden("Send Messages in Threads"));
        }
    } else if !at_destination.contains(Permissions::SEND_MESSAGES) {
        return Err(forbidden("Send Messages"));
    }
    if uploading && !at_destination.contains(Permissions::ATTACH_FILES) {
        return Err(forbidden("Attach Files"));
    }
    if !uploading && !at_destination.contains(Permissions::EMBED_LINKS) {
        posting.notes.push(format!(
            "The page will not unfurl: the bot lacks Embed Links in {dest_label}."
        ));
    }
    Ok(())
}

/// The publisher's record of a message it posted or edited.
fn published(origin: &DiscordOrigin, message: &Message, notes: Vec<String>) -> Published {
    let posted = DiscordOrigin {
        application: origin.application,
        guild: message.guild_id.or(origin.guild),
        channel: message.channel_id,
        message: Some(message.id),
        author: Some(message.author.id),
    };
    Published {
        reference: message.id.to_string(),
        url: posted.jump_url(),
        at: Timestamp::now(),
        notes,
    }
}

/// Whether Discord's side failed in a way another attempt may get past: the connection
/// broke before an answer, or the answer was a server error.
fn transient(error: &twilight_http::Error) -> bool {
    match error.kind() {
        ErrorType::RequestError => true,
        ErrorType::Response { status, .. } => status.get() >= 500,
        _ => false,
    }
}

/// How long to wait before attempt `next`, counted from one: two seconds, then four, then
/// eight.
fn backoff(next: u32) -> Duration {
    Duration::from_secs(1 << next.min(4))
}

/// A duration as people read it in an error: `2m 8s`.
fn wait_text(wait: Duration) -> String {
    let secs = wait.as_secs();
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

/// A playing time as a clock reads, `1:02:03` or `4:05`
fn clock(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Text with Discord's markdown characters escaped, so a title reads as written
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The lines joined within a message, the repeated original text giving way first
fn fit_lines(mut lines: Vec<String>, original_index: Option<usize>) -> String {
    let total = |lines: &[String]| {
        lines.iter().map(|l| l.chars().count()).sum::<usize>() + lines.len().saturating_sub(1)
    };
    if total(&lines) > MESSAGE_MAX
        && let Some(index) = original_index
    {
        let others = total(&lines) - lines[index].chars().count();
        let room = MESSAGE_MAX.saturating_sub(others).saturating_sub(1);
        if room == 0 {
            lines.remove(index);
        } else {
            let kept: String = lines[index].chars().take(room).collect();
            lines[index] = format!("{kept}\u{2026}");
        }
    }
    let text = lines.join("\n");
    if text.chars().count() > MESSAGE_MAX {
        text.chars().take(MESSAGE_MAX).collect()
    } else {
        text
    }
}

fn display_name(member: &MemberInfo) -> String {
    member
        .nick
        .clone()
        .or_else(|| member.display_name.clone())
        .unwrap_or_else(|| member.username.clone())
}

/// A name a webhook may post under, which Discord refuses when it names itself
fn webhook_name(name: &str) -> String {
    let trimmed: String = name.trim().chars().take(80).collect();
    let lower = trimmed.to_lowercase();
    if trimmed.is_empty() || lower.contains("clyde") || lower.contains("discord") {
        "someone".to_string()
    } else {
        trimmed
    }
}

/// The member's avatar on Discord's CDN, the one set for the guild first, else the default
fn avatar_url(guild: Id<GuildMarker>, member: &MemberInfo) -> String {
    if let Some(hash) = &member.guild_avatar {
        return format!(
            "https://cdn.discordapp.com/guilds/{guild}/users/{}/avatars/{hash}.png",
            member.id
        );
    }
    if let Some(hash) = &member.avatar {
        return format!(
            "https://cdn.discordapp.com/avatars/{}/{hash}.png",
            member.id
        );
    }
    format!(
        "https://cdn.discordapp.com/embed/avatars/{}.png",
        (member.id.get() >> 22) % 6
    )
}

#[async_trait]
impl Publisher for DiscordPublisher {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError> {
        let origin = Self::parse(&job.request.origin)?;
        let clients = self.clients_for(origin.application)?;
        let limit = self
            .upload_limit(&origin, &clients.lookups, job.request.policy.upload)
            .await?;
        Ok(job.request.policy.output.constraints(limit))
    }

    async fn link_target(&self, job: &Job) -> Result<LinkTarget, PublishError> {
        self.links
            .link_for(job, &job.request.policy.delivery.view)
            .map(|link| LinkTarget {
                page: link.page,
                view: link.view,
            })
            .map_err(|e| PublishError::NoLink(e.to_string()))
    }

    async fn announce(
        &self,
        job: &Job,
        _recording: &LocalFile,
    ) -> Result<Option<Published>, PublishError> {
        let page = match self.link_target(job).await {
            Ok(target) => target.page,
            Err(PublishError::NoLink(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut posting = self.posting(job, false).await?;
        let requester = Self::requester_name(&posting);
        let (text, allowed) = self.compose(job, &posting, Some(&page), requester.as_deref());
        let webhook =
            if posting.placement == Placement::Replace && posting.replace_as == ReplaceAs::Author {
                Some(self.webhook_for(&posting).await?)
            } else {
                None
            };
        let identity = webhook
            .as_ref()
            .and_then(|_| Self::author_identity(&posting));
        let message = self
            .send_once(
                &posting,
                None,
                webhook.as_ref(),
                identity.as_ref(),
                &text,
                &allowed,
                &[],
                None,
            )
            .await
            .map_err(|failure| match failure {
                SendFailure::Http(error) => PublishError::Rejected(error.to_string()),
                SendFailure::TimedOut => {
                    PublishError::Rejected("the announcement timed out".into())
                }
            })?
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("message decode: {e}")))?;
        self.tend_original(&mut posting).await?;
        Ok(Some(published(&posting.origin, &message, posting.notes)))
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        let uploading = job.artifacts.delivery == Delivery::Upload;
        let mut posting = self.posting(job, uploading).await?;
        let announced = Self::announced(job);
        let settings = self.settings();
        let page = if uploading {
            None
        } else {
            match job.artifacts.link.as_ref() {
                Some(link) => Some(link.page.clone()),
                None => Some(self.link_target(job).await?.page),
            }
        };
        let requester = Self::requester_name(&posting);
        let (text, allowed) = self.compose(job, &posting, page.as_ref(), requester.as_deref());
        let mut attachments = Vec::new();
        let mut limit = 0;
        if uploading {
            limit = self
                .upload_limit(
                    &posting.origin,
                    &posting.clients.lookups,
                    job.request.policy.upload,
                )
                .await?;
            if file.size > limit {
                return Err(if job.request.policy.upload.max_bytes.is_auto() {
                    PublishError::LimitLowered {
                        size: file.size,
                        max: limit,
                    }
                } else {
                    PublishError::TooLarge {
                        size: file.size,
                        max: limit,
                    }
                });
            }
            let bytes = tokio::fs::read(&file.path).await?;
            let ext = file
                .path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");
            let id = job.id.to_string();
            let filename = format!(
                "{}.{ext}",
                safe_stem(job.title(), &format!("{}-{}", job.media(), &id[..8]))
            );
            attachments.push(Attachment::from_bytes(filename, bytes, 1));
        }
        // A capture that was announced gets its result in the announcement, which was
        // placed and tended when it was posted.
        let as_webhook = announced.is_none()
            && posting.placement == Placement::Replace
            && posting.replace_as == ReplaceAs::Author;
        let mut webhook = if as_webhook {
            Some(self.webhook_for(&posting).await?)
        } else {
            None
        };
        let identity = webhook
            .as_ref()
            .and_then(|_| Self::author_identity(&posting));
        if as_webhook && identity.is_none() {
            webhook = None;
            posting.replace_as = ReplaceAs::Bot;
            posting
                .notes
                .push("Replacing as the bot: the author is no longer known here.".into());
        }
        // Each upload attempt gets the whole budget: a grace for Discord to answer, plus
        // the file at the slowest rate the settings give an upload link credit for. An
        // upload that runs out of time is not sent again, since Discord may have posted
        // it while the answer was on its way.
        let budget = uploading.then(|| settings.upload.timeout(file.size));
        let attempts = if uploading {
            settings.upload.attempts.max(1)
        } else {
            1
        };
        let mut attempt = 1;
        let mut webhook_remade = false;
        let response = loop {
            let sent = self
                .send_once(
                    &posting,
                    announced,
                    webhook.as_ref(),
                    identity.as_ref(),
                    &text,
                    &allowed,
                    &attachments,
                    budget,
                )
                .await;
            match sent {
                Ok(response) => break response,
                Err(SendFailure::Http(error)) => {
                    if is_status(&error, 413) {
                        return Err(match self.lower_limit(posting.origin.guild, limit).await {
                            Some(max) if job.request.policy.upload.max_bytes.is_auto() => {
                                PublishError::LimitLowered {
                                    size: file.size,
                                    max,
                                }
                            }
                            _ => PublishError::TooLarge {
                                size: file.size,
                                max: limit,
                            },
                        });
                    }
                    if webhook.is_some() && !webhook_remade && is_status(&error, 404) {
                        let channel = posting.thread_parent.unwrap_or(posting.destination);
                        self.webhooks
                            .lock()
                            .await
                            .remove(&(posting.origin.application, channel));
                        webhook = Some(self.webhook_for(&posting).await?);
                        webhook_remade = true;
                        continue;
                    }
                    if attempt < attempts && transient(&error) {
                        let wait = backoff(attempt);
                        tracing::warn!(
                            job = %job.id,
                            attempt,
                            of = attempts,
                            "discord post failed, sending again in {}: {error}",
                            wait_text(wait)
                        );
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(PublishError::Rejected(if attempt > 1 {
                        format!("{error} (after {attempt} attempts)")
                    } else {
                        error.to_string()
                    }));
                }
                Err(SendFailure::TimedOut) => {
                    return Err(PublishError::Rejected(format!(
                        "the upload of {} bytes did not finish within {}: the connection to \
                         Discord is too slow or has stalled. discord.upload in Settings sets \
                         the time an upload gets.",
                        file.size,
                        wait_text(budget.unwrap_or_default())
                    )));
                }
            }
        };
        let message = response
            .model()
            .await
            .map_err(|e| PublishError::Rejected(format!("message decode: {e}")))?;
        if announced.is_none() {
            self.tend_original(&mut posting).await?;
        }
        Ok(published(&posting.origin, &message, posting.notes))
    }

    async fn report_failure(&self, job: &Job, stage: Stage, message: &str) {
        let policy = match DiscordPolicy::of(&job.request) {
            Ok(policy) => policy,
            Err(why) => {
                tracing::warn!(job = %job.id, "{why}");
                return;
            }
        };
        if !policy.errors.debug {
            return;
        }
        let Ok(origin) = Self::parse(&job.request.origin) else {
            return;
        };
        let clients = match self.clients_for(origin.application) {
            Ok(clients) => clients,
            Err(error) => {
                tracing::warn!(job = %job.id, "could not report the failure: {error}");
                return;
            }
        };
        let text = format!(
            "DiscoClip could not post {}\n{stage} failed: {message}\nReport: <{ISSUES}>",
            job.request.url
        );
        let id = job.id.to_string();
        let log = Attachment::from_bytes(
            format!("discoclip-{}.txt", &id[..8]),
            job.render_log().into_bytes(),
            1,
        );
        self.report(
            &clients,
            &origin,
            Self::destination(job, &origin),
            text,
            Some(log),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_and_waits_read_plainly() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(9), Duration::from_secs(16));
        assert_eq!(wait_text(Duration::from_secs(45)), "45s");
        assert_eq!(wait_text(Duration::from_secs(120)), "2m");
        assert_eq!(wait_text(Duration::from_secs(128)), "2m 8s");
    }

    #[test]
    fn limits_step_down_the_ladder() {
        assert_eq!(limit_for_tier(PremiumTier::None), 10 * MIB);
        assert_eq!(limit_for_tier(PremiumTier::Tier1), 10 * MIB);
        assert_eq!(limit_for_tier(PremiumTier::Tier2), 50 * MIB);
        assert_eq!(limit_for_tier(PremiumTier::Tier3), 100 * MIB);
        assert_eq!(next_lower(100 * MIB), Some(50 * MIB));
        assert_eq!(next_lower(50 * MIB), Some(10 * MIB));
        assert_eq!(next_lower(10 * MIB), None);
        assert_eq!(next_lower(25 * MIB), Some(10 * MIB));
    }

    #[test]
    fn text_helpers_read_as_people_expect() {
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(escape_markdown("a_b*c|d"), "a\\_b\\*c\\|d");
        assert_eq!(webhook_name("  Nick  "), "Nick");
        assert_eq!(webhook_name("Discord Mod"), "someone");
        assert_eq!(webhook_name("clyde"), "someone");
        assert_eq!(webhook_name(""), "someone");
        assert_eq!(webhook_name(&"x".repeat(100)).chars().count(), 80);
        let member = MemberInfo {
            id: Id::new(1 << 22),
            username: "nick".into(),
            display_name: Some("Nick".into()),
            nick: None,
            avatar: None,
            guild_avatar: None,
            bot: false,
        };
        assert_eq!(display_name(&member), "Nick");
        assert_eq!(
            avatar_url(Id::new(5), &member),
            "https://cdn.discordapp.com/embed/avatars/1.png"
        );
        let with_avatar = MemberInfo {
            avatar: Some("abc".into()),
            nick: Some("N".into()),
            ..member.clone()
        };
        assert_eq!(display_name(&with_avatar), "N");
        assert_eq!(
            avatar_url(Id::new(5), &with_avatar),
            format!("https://cdn.discordapp.com/avatars/{}/abc.png", 1u64 << 22)
        );
        let with_guild_avatar = MemberInfo {
            guild_avatar: Some("ghi".into()),
            ..with_avatar
        };
        assert_eq!(
            avatar_url(Id::new(5), &with_guild_avatar),
            format!(
                "https://cdn.discordapp.com/guilds/5/users/{}/avatars/ghi.png",
                1u64 << 22
            )
        );
    }

    #[test]
    fn the_original_text_gives_way_first_when_a_message_overflows() {
        let long = "x".repeat(2500);
        let text = fit_lines(
            vec![long, "**title**".into(), "<https://a.test>".into()],
            Some(0),
        );
        assert!(text.chars().count() <= MESSAGE_MAX);
        assert!(text.ends_with("**title**\n<https://a.test>"));
        assert!(text.starts_with("xxx"));
        assert!(text.contains('\u{2026}'));
        let short = fit_lines(vec!["a".into(), "b".into()], None);
        assert_eq!(short, "a\nb");
        let huge_rest = fit_lines(vec!["orig".into(), "y".repeat(2100)], Some(0));
        assert_eq!(huge_rest.chars().count(), MESSAGE_MAX);
        assert!(huge_rest.starts_with('y'));
    }

    #[tokio::test]
    async fn permissions_degrade_in_a_fixed_order() {
        fn posting(placement: Placement, replace_as: ReplaceAs, suppress: bool) -> Posting {
            let clients = DiscordClients {
                lookups: Arc::new(Client::new("t".into())),
                uploads: Arc::new(Client::new("t".into())),
            };
            Posting {
                origin: DiscordOrigin {
                    application: Uuid::from_u128(1),
                    guild: Some(Id::new(5)),
                    channel: Id::new(10),
                    message: Some(Id::new(77)),
                    author: Some(Id::new(9)),
                },
                clients,
                directory: None,
                policy: DiscordPolicy::default(),
                destination: Id::new(10),
                thread_parent: None,
                placement,
                replace_as,
                suppress,
                notes: Vec::new(),
            }
        }
        let all = Permissions::all();
        let base = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::ATTACH_FILES
            | Permissions::EMBED_LINKS;

        let mut p = posting(Placement::Replace, ReplaceAs::Author, true);
        apply_permissions(&mut p, all, all, false, true, "#a", "#a").unwrap();
        assert_eq!(p.placement, Placement::Replace);
        assert_eq!(p.replace_as, ReplaceAs::Author);
        assert!(p.suppress && p.notes.is_empty());

        let mut p = posting(Placement::Replace, ReplaceAs::Author, true);
        let no_hooks = all - Permissions::MANAGE_WEBHOOKS;
        apply_permissions(&mut p, no_hooks, no_hooks, false, true, "#a", "#a").unwrap();
        assert_eq!(p.replace_as, ReplaceAs::Bot);
        assert_eq!(p.placement, Placement::Replace);
        assert!(p.notes[0].contains("Manage Webhooks"));

        let mut p = posting(Placement::Replace, ReplaceAs::Bot, true);
        let no_manage = base | Permissions::READ_MESSAGE_HISTORY;
        apply_permissions(&mut p, no_manage, no_manage, false, true, "#a", "#a").unwrap();
        assert_eq!(p.placement, Placement::Reply);
        assert!(!p.suppress);
        assert!(p.notes[0].starts_with("Replying instead of replacing"));
        assert!(p.notes[1].starts_with("Leaving the original embeds"));

        let mut p = posting(Placement::Reply, ReplaceAs::Bot, false);
        apply_permissions(&mut p, base, base, false, true, "#a", "#a").unwrap();
        assert_eq!(p.placement, Placement::Post);
        assert!(p.notes[0].contains("Read Message History"));

        let mut p = posting(Placement::Post, ReplaceAs::Bot, false);
        let no_send = base - Permissions::SEND_MESSAGES;
        assert!(matches!(
            apply_permissions(&mut p, no_send, no_send, false, true, "#a", "#a"),
            Err(PublishError::Forbidden { missing, .. }) if missing == "Send Messages"
        ));
        let mut p = posting(Placement::Post, ReplaceAs::Bot, false);
        let threads = no_send | Permissions::SEND_MESSAGES_IN_THREADS;
        assert!(apply_permissions(&mut p, threads, threads, true, true, "#a", "#a").is_ok());
        let mut p = posting(Placement::Post, ReplaceAs::Bot, false);
        let no_files = base - Permissions::ATTACH_FILES;
        assert!(matches!(
            apply_permissions(&mut p, no_files, no_files, false, true, "#a", "#a"),
            Err(PublishError::Forbidden { missing, .. }) if missing == "Attach Files"
        ));
        let mut p = posting(Placement::Post, ReplaceAs::Bot, false);
        assert!(apply_permissions(&mut p, no_files, no_files, false, false, "#a", "#a").is_ok());
        let mut p = posting(Placement::Post, ReplaceAs::Bot, false);
        let no_embeds = base - Permissions::EMBED_LINKS;
        apply_permissions(&mut p, no_embeds, no_embeds, false, false, "#a", "#a").unwrap();
        assert!(p.notes[0].contains("Embed Links"));
    }
}
