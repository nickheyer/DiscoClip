//! The policy sections a profile carries and what they add up to. Every leaf of a section
//! is optional, so a profile lays only what it names over the wider scopes, and the
//! built-in Default names every leaf.

use std::collections::BTreeMap;

use discoclip_bot::{
    BotMessages, DiscordPolicy, ErrorsPolicy, InForce, Include, MessagePolicy, OriginalEmbeds,
    OriginalText, PermissionMode, Placement, ReplaceAs, Requester,
};
use discoclip_engine::policy::{
    DedupeMatch, DedupePolicy, DeliveryMode, DeliveryPolicy, Intake, Limits, OverLimit, Playlists,
    Policy, UnderFloor, UploadLimit, UploadPolicy, View,
};
use discoclip_engine::publish::{DestinationTarget, TargetOverride};
use serde::{Deserialize, Deserializer, Serialize};
use twilight_model::id::Id;

use crate::profiles::{Assignment, ProfileError};

/// A key left out inherits, `null` is a value of its own, anything else replaces
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// How big, long and tall media may be. A limit a profile names replaces the wider scope's
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileLimits {
    pub max_source_bytes: Option<u64>,
    /// Left out inherits, `null` lifts the bound, a number sets it
    #[serde(
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_duration_secs: Option<Option<u64>>,
    pub max_height: Option<u32>,
    pub max_capture_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlaylistsOverlay {
    pub enabled: Option<bool>,
    pub max_entries: Option<usize>,
}

/// Which links are taken in at all
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntakeOverlay {
    /// Users whose links count, everyone when this and the roles are both empty
    pub allow_users: Option<Vec<String>>,
    pub allow_roles: Option<Vec<String>>,
    pub bot_messages: Option<BotMessages>,
    pub playlists: PlaylistsOverlay,
    pub live: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadOverlay {
    pub max_bytes: Option<UploadLimit>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FloorOverlay {
    pub min_height: Option<u32>,
    pub min_bitrate: Option<u64>,
}

/// Whether the destination gets the file or a link to the page that plays it
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryOverlay {
    pub mode: Option<DeliveryMode>,
    pub view: Option<View>,
    pub floor: FloorOverlay,
    pub under_floor: Option<UnderFloor>,
    pub over_limit: Option<OverLimit>,
    pub link_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IncludeOverlay {
    pub source_link: Option<bool>,
    pub title: Option<bool>,
    pub platform: Option<bool>,
    pub uploader: Option<bool>,
    pub requester: Option<Requester>,
    pub duration: Option<bool>,
    pub brand: Option<bool>,
    pub earlier_post: Option<bool>,
}

/// How the result is posted and what goes with it
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageOverlay {
    /// Left out inherits, `null` posts where the link was seen, an id names a channel
    #[serde(
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub destination: Option<Option<String>>,
    pub placement: Option<Placement>,
    pub replace_as: Option<ReplaceAs>,
    pub original_text: Option<OriginalText>,
    pub original_embeds: Option<OriginalEmbeds>,
    pub permissions: Option<PermissionMode>,
    pub include: IncludeOverlay,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ErrorsOverlay {
    pub debug: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupeOverlay {
    pub enabled: Option<bool>,
    #[serde(rename = "match")]
    pub matching: Option<DedupeMatch>,
}

/// Every section a profile carries
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sections {
    pub limits: ProfileLimits,
    pub intake: IntakeOverlay,
    pub output: TargetOverride,
    pub upload: UploadOverlay,
    pub delivery: DeliveryOverlay,
    pub message: MessageOverlay,
    pub errors: ErrorsOverlay,
    pub dedupe: DedupeOverlay,
}

/// A few sections replaced whole at a scope, as whoever manages a server on Discord may
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SectionsPatch {
    pub intake: Option<IntakeOverlay>,
    pub message: Option<MessageOverlay>,
    pub output: Option<TargetOverride>,
    pub upload: Option<UploadOverlay>,
}

impl SectionsPatch {
    pub fn is_empty(&self) -> bool {
        self.intake.is_none()
            && self.message.is_none()
            && self.output.is_none()
            && self.upload.is_none()
    }

    /// Whether the patch touches what only the settings permission may set
    pub fn needs_settings_permission(&self) -> bool {
        self.output.is_some() || self.upload.is_some()
    }

    pub fn apply(&self, sections: &mut Sections) {
        if let Some(intake) = &self.intake {
            sections.intake = intake.clone();
        }
        if let Some(message) = &self.message {
            sections.message = message.clone();
        }
        if let Some(output) = &self.output {
            sections.output = output.clone();
        }
        if let Some(upload) = self.upload {
            sections.upload = upload;
        }
    }
}

/// Who the bot listens to and whether it reads other bots, as the sections add up
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveIntake {
    pub allow_users: Vec<String>,
    pub allow_roles: Vec<String>,
    pub bot_messages: BotMessages,
    pub playlists: Playlists,
    pub live: bool,
}

/// How the result is posted, as the sections add up
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveMessage {
    /// The channel results go to, the one the link was seen in when unset
    pub destination: Option<String>,
    #[serde(flatten)]
    pub policy: MessagePolicy,
}

/// What the profiles assigned at a place add up to, every leaf settled
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectivePolicy {
    pub platforms: BTreeMap<String, bool>,
    pub limits: Limits,
    pub audio_language: Option<String>,
    pub intake: EffectiveIntake,
    pub output: DestinationTarget,
    pub upload: UploadPolicy,
    pub delivery: DeliveryPolicy,
    pub message: EffectiveMessage,
    pub errors: ErrorsPolicy,
    pub dedupe: DedupePolicy,
    /// The assignments applied to get here, widest first
    pub applied: Vec<Assignment>,
}

impl EffectivePolicy {
    /// The engine's and the bot's own defaults over `platforms`, before any profile applies
    pub fn base(platforms: BTreeMap<String, bool>) -> Self {
        let policy = Policy::default();
        Self {
            platforms,
            limits: policy.limits,
            audio_language: None,
            intake: EffectiveIntake {
                allow_users: Vec::new(),
                allow_roles: Vec::new(),
                bot_messages: BotMessages::default(),
                playlists: policy.intake.playlists,
                live: policy.intake.live,
            },
            output: policy.output,
            upload: policy.upload,
            delivery: policy.delivery,
            message: EffectiveMessage {
                destination: None,
                policy: MessagePolicy::default(),
            },
            errors: ErrorsPolicy::default(),
            dedupe: policy.dedupe,
            applied: Vec::new(),
        }
    }

    /// The platforms turned off, by resolver id
    pub fn disabled(&self) -> Vec<String> {
        self.platforms
            .iter()
            .filter(|(_, on)| !**on)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// What the engine runs a job under
    pub fn engine_policy(&self) -> Policy {
        Policy {
            limits: self.limits.clone(),
            intake: Intake {
                playlists: self.intake.playlists.clone(),
                live: self.intake.live,
            },
            output: self.output.clone(),
            upload: self.upload,
            delivery: self.delivery.clone(),
            dedupe: self.dedupe,
            publisher: serde_json::Value::Null,
        }
    }

    /// What the Discord publisher reads
    pub fn discord_policy(&self) -> DiscordPolicy {
        DiscordPolicy {
            message: self.message.policy.clone(),
            errors: self.errors,
            original_text_value: None,
        }
    }

    /// The same, as the bots stamp it on requests
    pub fn in_force(&self) -> InForce {
        fn ids<M>(list: &[String]) -> Vec<Id<M>> {
            list.iter()
                .filter_map(|id| id.parse::<u64>().ok())
                .filter_map(Id::new_checked)
                .collect()
        }
        InForce {
            disabled: self.disabled(),
            audio_language: self.audio_language.clone(),
            policy: self.engine_policy(),
            bot_messages: self.intake.bot_messages,
            allow_users: ids(&self.intake.allow_users),
            allow_roles: ids(&self.intake.allow_roles),
            message_destination: self
                .message
                .destination
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok())
                .and_then(Id::new_checked),
            discord: self.discord_policy(),
        }
    }
}

impl Sections {
    /// Lays every leaf this profile names over `eff`
    pub fn apply_to(&self, eff: &mut EffectivePolicy) {
        let limits = &self.limits;
        if let Some(n) = limits.max_source_bytes {
            eff.limits.max_source_bytes = n;
        }
        if let Some(bound) = limits.max_duration_secs {
            eff.limits.max_duration_secs = bound;
        }
        if let Some(n) = limits.max_height {
            eff.limits.max_height = n;
        }
        if let Some(n) = limits.max_capture_secs {
            eff.limits.max_capture_secs = n;
        }
        let intake = &self.intake;
        if let Some(users) = &intake.allow_users {
            eff.intake.allow_users = users.clone();
        }
        if let Some(roles) = &intake.allow_roles {
            eff.intake.allow_roles = roles.clone();
        }
        if let Some(bots) = intake.bot_messages {
            eff.intake.bot_messages = bots;
        }
        if let Some(enabled) = intake.playlists.enabled {
            eff.intake.playlists.enabled = enabled;
        }
        if let Some(n) = intake.playlists.max_entries {
            eff.intake.playlists.max_entries = n;
        }
        if let Some(live) = intake.live {
            eff.intake.live = live;
        }
        eff.output = eff.output.with(&self.output);
        if let Some(limit) = self.upload.max_bytes {
            eff.upload.max_bytes = limit;
        }
        let delivery = &self.delivery;
        if let Some(mode) = delivery.mode {
            eff.delivery.mode = mode;
        }
        if let Some(view) = &delivery.view {
            eff.delivery.view = view.clone();
        }
        if let Some(n) = delivery.floor.min_height {
            eff.delivery.floor.min_height = n;
        }
        if let Some(n) = delivery.floor.min_bitrate {
            eff.delivery.floor.min_bitrate = n;
        }
        if let Some(action) = delivery.under_floor {
            eff.delivery.under_floor = action;
        }
        if let Some(action) = delivery.over_limit {
            eff.delivery.over_limit = action;
        }
        if let Some(n) = delivery.link_max_bytes {
            eff.delivery.link_max_bytes = n;
        }
        let message = &self.message;
        if let Some(destination) = &message.destination {
            eff.message.destination = destination.clone();
        }
        let policy = &mut eff.message.policy;
        if let Some(placement) = message.placement {
            policy.placement = placement;
        }
        if let Some(replace_as) = message.replace_as {
            policy.replace_as = replace_as;
        }
        if let Some(text) = message.original_text {
            policy.original_text = text;
        }
        if let Some(embeds) = message.original_embeds {
            policy.original_embeds = embeds;
        }
        if let Some(permissions) = message.permissions {
            policy.permissions = permissions;
        }
        let include = &message.include;
        if let Some(on) = include.source_link {
            policy.include.source_link = on;
        }
        if let Some(on) = include.title {
            policy.include.title = on;
        }
        if let Some(on) = include.platform {
            policy.include.platform = on;
        }
        if let Some(on) = include.uploader {
            policy.include.uploader = on;
        }
        if let Some(requester) = include.requester {
            policy.include.requester = requester;
        }
        if let Some(on) = include.duration {
            policy.include.duration = on;
        }
        if let Some(on) = include.brand {
            policy.include.brand = on;
        }
        if let Some(on) = include.earlier_post {
            policy.include.earlier_post = on;
        }
        if let Some(debug) = self.errors.debug {
            eff.errors.debug = debug;
        }
        if let Some(enabled) = self.dedupe.enabled {
            eff.dedupe.enabled = enabled;
        }
        if let Some(matching) = self.dedupe.matching {
            eff.dedupe.matching = matching;
        }
    }

    /// Every leaf named, with the engine's and the bot's own defaults
    pub fn defaults() -> Sections {
        let policy = Policy::default();
        let message = MessagePolicy::default();
        let Include {
            source_link,
            title,
            platform,
            uploader,
            requester,
            duration,
            brand,
            earlier_post,
        } = message.include;
        Sections {
            limits: ProfileLimits {
                max_source_bytes: Some(policy.limits.max_source_bytes),
                max_duration_secs: Some(policy.limits.max_duration_secs),
                max_height: Some(policy.limits.max_height),
                max_capture_secs: Some(policy.limits.max_capture_secs),
            },
            intake: IntakeOverlay {
                allow_users: Some(Vec::new()),
                allow_roles: Some(Vec::new()),
                bot_messages: Some(BotMessages::default()),
                playlists: PlaylistsOverlay {
                    enabled: Some(policy.intake.playlists.enabled),
                    max_entries: Some(policy.intake.playlists.max_entries),
                },
                live: Some(policy.intake.live),
            },
            output: TargetOverride {
                container: Some(policy.output.container.clone()),
                video_codec: Some(policy.output.video_codec.clone()),
                audio_codec: Some(policy.output.audio_codec.clone()),
                max_height: policy.output.max_height,
                max_fps: policy.output.max_fps,
                audio_over_still: Some(policy.output.audio_over_still),
                audio_containers: Some(policy.output.audio_containers.clone()),
                image_containers: Some(policy.output.image_containers.clone()),
                files: Some(policy.output.files),
            },
            upload: UploadOverlay {
                max_bytes: Some(policy.upload.max_bytes),
            },
            delivery: DeliveryOverlay {
                mode: Some(policy.delivery.mode),
                view: Some(policy.delivery.view.clone()),
                floor: FloorOverlay {
                    min_height: Some(policy.delivery.floor.min_height),
                    min_bitrate: Some(policy.delivery.floor.min_bitrate),
                },
                under_floor: Some(policy.delivery.under_floor),
                over_limit: Some(policy.delivery.over_limit),
                link_max_bytes: Some(policy.delivery.link_max_bytes),
            },
            message: MessageOverlay {
                destination: Some(None),
                placement: Some(message.placement),
                replace_as: Some(message.replace_as),
                original_text: Some(message.original_text),
                original_embeds: Some(message.original_embeds),
                permissions: Some(message.permissions),
                include: IncludeOverlay {
                    source_link: Some(source_link),
                    title: Some(title),
                    platform: Some(platform),
                    uploader: Some(uploader),
                    requester: Some(requester),
                    duration: Some(duration),
                    brand: Some(brand),
                    earlier_post: Some(earlier_post),
                },
            },
            errors: ErrorsOverlay {
                debug: Some(ErrorsPolicy::default().debug),
            },
            dedupe: DedupeOverlay {
                enabled: Some(policy.dedupe.enabled),
                matching: Some(policy.dedupe.matching),
            },
        }
    }

    /// These sections with every leaf left unnamed taken from `base`
    pub fn filled_from(&self, base: &Sections) -> Sections {
        Sections {
            limits: ProfileLimits {
                max_source_bytes: self
                    .limits
                    .max_source_bytes
                    .or(base.limits.max_source_bytes),
                max_duration_secs: self
                    .limits
                    .max_duration_secs
                    .or(base.limits.max_duration_secs),
                max_height: self.limits.max_height.or(base.limits.max_height),
                max_capture_secs: self
                    .limits
                    .max_capture_secs
                    .or(base.limits.max_capture_secs),
            },
            intake: IntakeOverlay {
                allow_users: self
                    .intake
                    .allow_users
                    .clone()
                    .or_else(|| base.intake.allow_users.clone()),
                allow_roles: self
                    .intake
                    .allow_roles
                    .clone()
                    .or_else(|| base.intake.allow_roles.clone()),
                bot_messages: self.intake.bot_messages.or(base.intake.bot_messages),
                playlists: PlaylistsOverlay {
                    enabled: self
                        .intake
                        .playlists
                        .enabled
                        .or(base.intake.playlists.enabled),
                    max_entries: self
                        .intake
                        .playlists
                        .max_entries
                        .or(base.intake.playlists.max_entries),
                },
                live: self.intake.live.or(base.intake.live),
            },
            output: TargetOverride {
                container: self
                    .output
                    .container
                    .clone()
                    .or_else(|| base.output.container.clone()),
                video_codec: self
                    .output
                    .video_codec
                    .clone()
                    .or_else(|| base.output.video_codec.clone()),
                audio_codec: self
                    .output
                    .audio_codec
                    .clone()
                    .or_else(|| base.output.audio_codec.clone()),
                max_height: self.output.max_height.or(base.output.max_height),
                max_fps: self.output.max_fps.or(base.output.max_fps),
                audio_over_still: self
                    .output
                    .audio_over_still
                    .or(base.output.audio_over_still),
                audio_containers: self
                    .output
                    .audio_containers
                    .clone()
                    .or_else(|| base.output.audio_containers.clone()),
                image_containers: self
                    .output
                    .image_containers
                    .clone()
                    .or_else(|| base.output.image_containers.clone()),
                files: self.output.files.or(base.output.files),
            },
            upload: UploadOverlay {
                max_bytes: self.upload.max_bytes.or(base.upload.max_bytes),
            },
            delivery: DeliveryOverlay {
                mode: self.delivery.mode.or(base.delivery.mode),
                view: self
                    .delivery
                    .view
                    .clone()
                    .or_else(|| base.delivery.view.clone()),
                floor: FloorOverlay {
                    min_height: self
                        .delivery
                        .floor
                        .min_height
                        .or(base.delivery.floor.min_height),
                    min_bitrate: self
                        .delivery
                        .floor
                        .min_bitrate
                        .or(base.delivery.floor.min_bitrate),
                },
                under_floor: self.delivery.under_floor.or(base.delivery.under_floor),
                over_limit: self.delivery.over_limit.or(base.delivery.over_limit),
                link_max_bytes: self
                    .delivery
                    .link_max_bytes
                    .or(base.delivery.link_max_bytes),
            },
            message: MessageOverlay {
                destination: self
                    .message
                    .destination
                    .clone()
                    .or_else(|| base.message.destination.clone()),
                placement: self.message.placement.or(base.message.placement),
                replace_as: self.message.replace_as.or(base.message.replace_as),
                original_text: self.message.original_text.or(base.message.original_text),
                original_embeds: self
                    .message
                    .original_embeds
                    .or(base.message.original_embeds),
                permissions: self.message.permissions.or(base.message.permissions),
                include: IncludeOverlay {
                    source_link: self
                        .message
                        .include
                        .source_link
                        .or(base.message.include.source_link),
                    title: self.message.include.title.or(base.message.include.title),
                    platform: self
                        .message
                        .include
                        .platform
                        .or(base.message.include.platform),
                    uploader: self
                        .message
                        .include
                        .uploader
                        .or(base.message.include.uploader),
                    requester: self
                        .message
                        .include
                        .requester
                        .or(base.message.include.requester),
                    duration: self
                        .message
                        .include
                        .duration
                        .or(base.message.include.duration),
                    brand: self.message.include.brand.or(base.message.include.brand),
                    earlier_post: self
                        .message
                        .include
                        .earlier_post
                        .or(base.message.include.earlier_post),
                },
            },
            errors: ErrorsOverlay {
                debug: self.errors.debug.or(base.errors.debug),
            },
            dedupe: DedupeOverlay {
                enabled: self.dedupe.enabled.or(base.dedupe.enabled),
                matching: self.dedupe.matching.or(base.dedupe.matching),
            },
        }
    }

    /// The leaves left unnamed that a complete profile has to name, as dotted paths
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        let mut need = |named: bool, path: &'static str| {
            if !named {
                missing.push(path);
            }
        };
        need(
            self.limits.max_source_bytes.is_some(),
            "limits.max_source_bytes",
        );
        need(
            self.limits.max_duration_secs.is_some(),
            "limits.max_duration_secs",
        );
        need(self.limits.max_height.is_some(), "limits.max_height");
        need(
            self.limits.max_capture_secs.is_some(),
            "limits.max_capture_secs",
        );
        need(self.intake.allow_users.is_some(), "intake.allow_users");
        need(self.intake.allow_roles.is_some(), "intake.allow_roles");
        need(self.intake.bot_messages.is_some(), "intake.bot_messages");
        need(
            self.intake.playlists.enabled.is_some(),
            "intake.playlists.enabled",
        );
        need(
            self.intake.playlists.max_entries.is_some(),
            "intake.playlists.max_entries",
        );
        need(self.intake.live.is_some(), "intake.live");
        need(self.output.container.is_some(), "output.container");
        need(self.output.video_codec.is_some(), "output.video_codec");
        need(self.output.audio_codec.is_some(), "output.audio_codec");
        need(
            self.output.audio_over_still.is_some(),
            "output.audio_over_still",
        );
        need(
            self.output.audio_containers.is_some(),
            "output.audio_containers",
        );
        need(
            self.output.image_containers.is_some(),
            "output.image_containers",
        );
        need(self.output.files.is_some(), "output.files");
        need(self.upload.max_bytes.is_some(), "upload.max_bytes");
        need(self.delivery.mode.is_some(), "delivery.mode");
        need(self.delivery.view.is_some(), "delivery.view");
        need(
            self.delivery.floor.min_height.is_some(),
            "delivery.floor.min_height",
        );
        need(
            self.delivery.floor.min_bitrate.is_some(),
            "delivery.floor.min_bitrate",
        );
        need(self.delivery.under_floor.is_some(), "delivery.under_floor");
        need(self.delivery.over_limit.is_some(), "delivery.over_limit");
        need(
            self.delivery.link_max_bytes.is_some(),
            "delivery.link_max_bytes",
        );
        need(self.message.destination.is_some(), "message.destination");
        need(self.message.placement.is_some(), "message.placement");
        need(self.message.replace_as.is_some(), "message.replace_as");
        need(
            self.message.original_text.is_some(),
            "message.original_text",
        );
        need(
            self.message.original_embeds.is_some(),
            "message.original_embeds",
        );
        need(self.message.permissions.is_some(), "message.permissions");
        need(
            self.message.include.source_link.is_some(),
            "message.include.source_link",
        );
        need(
            self.message.include.title.is_some(),
            "message.include.title",
        );
        need(
            self.message.include.platform.is_some(),
            "message.include.platform",
        );
        need(
            self.message.include.uploader.is_some(),
            "message.include.uploader",
        );
        need(
            self.message.include.requester.is_some(),
            "message.include.requester",
        );
        need(
            self.message.include.duration.is_some(),
            "message.include.duration",
        );
        need(
            self.message.include.brand.is_some(),
            "message.include.brand",
        );
        need(
            self.message.include.earlier_post.is_some(),
            "message.include.earlier_post",
        );
        need(self.errors.debug.is_some(), "errors.debug");
        need(self.dedupe.enabled.is_some(), "dedupe.enabled");
        need(self.dedupe.matching.is_some(), "dedupe.match");
        missing
    }

    /// Whether any delivery choice named here posts links
    pub fn can_link(&self) -> bool {
        self.delivery.mode == Some(DeliveryMode::Link)
            || self.delivery.under_floor == Some(UnderFloor::Link)
            || self.delivery.over_limit == Some(OverLimit::Link)
    }

    /// The sections that name anything, for a summary
    pub fn named(&self) -> Vec<&'static str> {
        let defaults = Sections::default();
        let mut named = Vec::new();
        if self.limits != defaults.limits {
            named.push("limits");
        }
        if self.intake != defaults.intake {
            named.push("intake");
        }
        if self.output != defaults.output {
            named.push("output");
        }
        if self.upload != defaults.upload {
            named.push("upload");
        }
        if self.delivery != defaults.delivery {
            named.push("delivery");
        }
        if self.message != defaults.message {
            named.push("message");
        }
        if self.errors != defaults.errors {
            named.push("errors");
        }
        if self.dedupe != defaults.dedupe {
            named.push("dedupe");
        }
        named
    }
}

/// What the sections are checked against as the server stands
#[derive(Debug, Clone, Default)]
pub struct Known {
    /// Every content view by id, and whether it is on
    pub views: BTreeMap<String, bool>,
    /// Whether the server has a public address to build links from
    pub public_url: bool,
    /// The output the whole server runs with, which an overlay's output is laid over
    pub base_output: DestinationTarget,
}

fn snowflake(what: &str, value: &str) -> Result<(), ProfileError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(|_| ())
        .ok_or_else(|| ProfileError::Invalid(format!("{what} {value:?} is not a Discord id")))
}

/// Why the sections cannot be used, the built-in profile having to name every leaf
pub fn check_sections(
    sections: &Sections,
    known: &Known,
    builtin: bool,
) -> Result<(), ProfileError> {
    let invalid = |message: &str| ProfileError::Invalid(message.to_string());
    if sections.limits.max_source_bytes == Some(0) {
        return Err(invalid("max_source_bytes must be above zero"));
    }
    if sections.limits.max_height == Some(0) {
        return Err(invalid("max_height must be above zero"));
    }
    if sections.limits.max_capture_secs == Some(0) {
        return Err(invalid("max_capture_secs must be above zero"));
    }
    for user in sections.intake.allow_users.iter().flatten() {
        snowflake("user", user)?;
    }
    for role in sections.intake.allow_roles.iter().flatten() {
        snowflake("role", role)?;
    }
    if sections.intake.playlists.max_entries == Some(0) {
        return Err(invalid("playlists.max_entries must be above zero"));
    }
    known
        .base_output
        .with(&sections.output)
        .check()
        .map_err(|message| ProfileError::Invalid(format!("output: {message}")))?;
    if sections.upload.max_bytes == Some(UploadLimit::Bytes(0)) {
        return Err(invalid("upload.max_bytes must be above zero or auto"));
    }
    if let Some(View::Id(id)) = &sections.delivery.view {
        match known.views.get(id) {
            None => return Err(ProfileError::UnknownView(id.clone())),
            Some(false) => return Err(ProfileError::ViewDisabled(id.clone())),
            Some(true) => {}
        }
    }
    if sections.delivery.floor.min_height == Some(0)
        || sections.delivery.floor.min_bitrate == Some(0)
    {
        return Err(invalid(
            "the quality floor's height and bitrate must be above zero",
        ));
    }
    if sections.delivery.link_max_bytes == Some(0) {
        return Err(invalid("link_max_bytes must be above zero"));
    }
    if sections.can_link() && !known.public_url {
        return Err(ProfileError::NoPublicUrl);
    }
    if let Some(Some(channel)) = &sections.message.destination {
        snowflake("destination channel", channel)?;
    }
    if builtin {
        let missing = sections.missing();
        if !missing.is_empty() {
            return Err(ProfileError::Incomplete(missing.join(", ")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_fold_over_the_base_and_null_lifts_the_duration() {
        let mut eff = EffectivePolicy::base(BTreeMap::new());
        assert_eq!(eff.limits.max_duration_secs, Some(10800));
        let overlay: Sections = serde_json::from_value(serde_json::json!({
            "limits": {"max_duration_secs": null, "max_height": 720},
            "intake": {"allow_users": ["9"], "live": false},
            "output": {"max_height": 480},
            "upload": {"max_bytes": 25000000},
            "delivery": {"mode": "link", "floor": {"min_height": 480}},
            "message": {"destination": "50", "placement": "replace", "include": {"brand": false}},
            "errors": {"debug": true},
            "dedupe": {"match": "url"}
        }))
        .unwrap();
        overlay.apply_to(&mut eff);
        assert_eq!(eff.limits.max_duration_secs, None);
        assert_eq!(eff.limits.max_height, 720);
        assert_eq!(eff.limits.max_source_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(eff.intake.allow_users, vec!["9".to_string()]);
        assert!(!eff.intake.live);
        assert!(eff.intake.playlists.enabled);
        assert_eq!(eff.output.max_height, Some(480));
        assert_eq!(eff.upload.max_bytes, UploadLimit::Bytes(25_000_000));
        assert_eq!(eff.delivery.mode, DeliveryMode::Link);
        assert_eq!(eff.delivery.floor.min_height, 480);
        assert_eq!(eff.delivery.floor.min_bitrate, 1_500_000);
        assert_eq!(eff.message.destination.as_deref(), Some("50"));
        assert_eq!(eff.message.policy.placement, Placement::Replace);
        assert!(!eff.message.policy.include.brand);
        assert!(eff.message.policy.include.title);
        assert!(eff.errors.debug);
        assert_eq!(eff.dedupe.matching, DedupeMatch::Url);
        let back_home: Sections =
            serde_json::from_value(serde_json::json!({"message": {"destination": null}})).unwrap();
        back_home.apply_to(&mut eff);
        assert_eq!(eff.message.destination, None);
        let in_force = eff.in_force();
        assert_eq!(in_force.allow_users, vec![Id::new(9)]);
        assert_eq!(in_force.message_destination, None);
        assert!(in_force.discord.errors.debug);
        assert_eq!(in_force.policy.delivery.mode, DeliveryMode::Link);
        let json = serde_json::to_value(&overlay).unwrap();
        assert_eq!(json["limits"]["max_duration_secs"], serde_json::Value::Null);
        assert!(json["limits"].get("max_source_bytes").is_some());
        let untouched: Sections = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(
            serde_json::to_value(&untouched).unwrap()["limits"]
                .get("max_duration_secs")
                .is_none()
        );
        assert_eq!(overlay.named().len(), 8);
        assert!(untouched.named().is_empty());
    }

    #[test]
    fn the_defaults_are_complete_and_fill_what_is_left_unnamed() {
        let defaults = Sections::defaults();
        assert!(defaults.missing().is_empty());
        let partial: Sections = serde_json::from_value(serde_json::json!({
            "limits": {"max_height": 480},
            "delivery": {"under_floor": "link"}
        }))
        .unwrap();
        assert!(partial.missing().contains(&"limits.max_source_bytes"));
        assert!(partial.missing().contains(&"delivery.mode"));
        assert!(!partial.missing().contains(&"delivery.under_floor"));
        let filled = partial.filled_from(&defaults);
        assert!(filled.missing().is_empty());
        assert_eq!(filled.limits.max_height, Some(480));
        assert_eq!(filled.delivery.under_floor, Some(UnderFloor::Link));
        assert_eq!(filled.delivery.over_limit, Some(OverLimit::Skip));
        let mut eff = EffectivePolicy::base(BTreeMap::new());
        let before = eff.clone();
        defaults.apply_to(&mut eff);
        assert_eq!(eff, before);
        assert!(check_sections(&defaults, &Known::default(), true).is_ok());
        assert!(matches!(
            check_sections(
                &partial,
                &Known {
                    public_url: true,
                    ..Known::default()
                },
                true
            ),
            Err(ProfileError::Incomplete(_))
        ));
    }

    #[test]
    fn sections_are_checked_against_what_the_server_has() {
        let known = Known {
            views: BTreeMap::from([("v1".to_string(), true), ("v2".to_string(), false)]),
            public_url: true,
            base_output: DestinationTarget::default(),
        };
        let mut sections = Sections::default();
        assert!(check_sections(&sections, &known, false).is_ok());
        sections.delivery.view = Some(View::Id("v1".into()));
        assert!(check_sections(&sections, &known, false).is_ok());
        sections.delivery.view = Some(View::Id("v2".into()));
        assert!(matches!(
            check_sections(&sections, &known, false),
            Err(ProfileError::ViewDisabled(_))
        ));
        sections.delivery.view = Some(View::Id("nope".into()));
        assert!(matches!(
            check_sections(&sections, &known, false),
            Err(ProfileError::UnknownView(_))
        ));
        sections.delivery.view = None;
        sections.delivery.under_floor = Some(UnderFloor::Link);
        assert!(check_sections(&sections, &known, false).is_ok());
        let no_address = Known {
            public_url: false,
            ..known.clone()
        };
        assert!(matches!(
            check_sections(&sections, &no_address, false),
            Err(ProfileError::NoPublicUrl)
        ));
        sections.delivery.under_floor = None;
        sections.message.destination = Some(Some("x".into()));
        assert!(matches!(
            check_sections(&sections, &known, false),
            Err(ProfileError::Invalid(_))
        ));
        sections.message.destination = Some(None);
        sections.upload.max_bytes = Some(UploadLimit::Bytes(0));
        assert!(check_sections(&sections, &known, false).is_err());
        sections.upload.max_bytes = Some(UploadLimit::AUTO);
        sections.output.video_codec = Some(discoclip_engine::media::VideoCodec::Vp9);
        assert!(matches!(
            check_sections(&sections, &known, false),
            Err(ProfileError::Invalid(message)) if message.starts_with("output:")
        ));
        let patch: SectionsPatch = serde_json::from_value(serde_json::json!({
            "intake": {"allow_roles": ["500"]},
            "message": {"destination": "50"}
        }))
        .unwrap();
        assert!(!patch.needs_settings_permission());
        let mut target = Sections::defaults();
        patch.apply(&mut target);
        assert_eq!(target.intake.allow_roles, Some(vec!["500".to_string()]));
        assert_eq!(target.intake.allow_users, None);
        assert_eq!(target.message.destination, Some(Some("50".to_string())));
        assert_eq!(target.message.placement, None);
    }
}
