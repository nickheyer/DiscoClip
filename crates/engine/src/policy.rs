//! The policy a request runs under, as the app's profile sets it. Stamped on a request
//! when it is submitted, so a job runs under the policy it was seen under.

use serde::{Deserialize, Serialize};

use crate::publish::{DestinationTarget, QualityFloor};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub limits: Limits,
    pub intake: Intake,
    pub output: DestinationTarget,
    pub upload: UploadPolicy,
    pub delivery: DeliveryPolicy,
    pub dedupe: DedupePolicy,
    /// What the origin's publisher reads beyond the engine, kept as it gave it
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub publisher: serde_json::Value,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            intake: Intake::default(),
            output: DestinationTarget::default(),
            upload: UploadPolicy::default(),
            delivery: DeliveryPolicy::default(),
            dedupe: DedupePolicy::default(),
            publisher: serde_json::Value::Null,
        }
    }
}

impl Policy {
    /// Why the policy cannot be used, naming the leaf at fault
    pub fn check(&self) -> Result<(), String> {
        if self.limits.max_source_bytes == 0 {
            return Err("limits.max_source_bytes must be at least 1".into());
        }
        if self.limits.max_height == 0 {
            return Err("limits.max_height must be at least 1".into());
        }
        if self.limits.max_capture_secs == 0 {
            return Err("limits.max_capture_secs must be at least 1".into());
        }
        if self.intake.playlists.max_entries == 0 {
            return Err("intake.playlists.max_entries must be at least 1".into());
        }
        self.output.check().map_err(|e| format!("output: {e}"))?;
        if self.upload.max_bytes == UploadLimit::Bytes(0) {
            return Err("upload.max_bytes must be at least 1".into());
        }
        if self.delivery.floor.min_height == 0 {
            return Err("delivery.floor.min_height must be at least 1".into());
        }
        if self.delivery.floor.min_bitrate == 0 {
            return Err("delivery.floor.min_bitrate must be at least 1".into());
        }
        if self.delivery.link_max_bytes == 0 {
            return Err("delivery.link_max_bytes must be at least 1".into());
        }
        Ok(())
    }
}

/// The ceiling on what a job may fetch and how long a live stream is captured
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_source_bytes: u64,
    /// `None` leaves the length unbounded
    pub max_duration_secs: Option<u64>,
    pub max_height: u32,
    pub max_capture_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 2 * 1024 * 1024 * 1024,
            max_duration_secs: Some(3 * 60 * 60),
            max_height: 1080,
            max_capture_secs: 3 * 60 * 60,
        }
    }
}

/// What the engine takes in at all
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Intake {
    pub playlists: Playlists,
    /// Whether live streams are captured
    pub live: bool,
}

impl Default for Intake {
    fn default() -> Self {
        Self {
            playlists: Playlists::default(),
            live: true,
        }
    }
}

/// How links to playlists and channels are handled, each entry becoming a job of its own
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Playlists {
    pub enabled: bool,
    pub max_entries: usize,
}

impl Default for Playlists {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadPolicy {
    pub max_bytes: UploadLimit,
}

impl Default for UploadPolicy {
    fn default() -> Self {
        Self {
            max_bytes: UploadLimit::AUTO,
        }
    }
}

/// The size a destination takes, discovered by its publisher or set by hand
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UploadLimit {
    Bytes(u64),
    Auto(AutoWord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoWord {
    Auto,
}

impl UploadLimit {
    pub const AUTO: UploadLimit = UploadLimit::Auto(AutoWord::Auto);

    pub fn is_auto(self) -> bool {
        matches!(self, UploadLimit::Auto(_))
    }

    /// The cap a publisher that discovers nothing applies
    pub fn cap(self) -> u64 {
        match self {
            UploadLimit::Bytes(n) => n,
            UploadLimit::Auto(_) => u64::MAX,
        }
    }
}

/// Whether the destination gets the file or a link to the page that plays it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryPolicy {
    pub mode: DeliveryMode,
    pub view: View,
    pub floor: QualityFloor,
    pub under_floor: UnderFloor,
    pub over_limit: OverLimit,
    /// The size bound of the output made for the page
    pub link_max_bytes: u64,
}

impl Default for DeliveryPolicy {
    fn default() -> Self {
        Self {
            mode: DeliveryMode::Auto,
            view: View::None,
            floor: QualityFloor {
                min_height: 720,
                min_bitrate: 1_500_000,
            },
            under_floor: UnderFloor::Skip,
            over_limit: OverLimit::Skip,
            link_max_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// The file, unless it would be too large or too reduced
    #[default]
    Auto,
    Upload,
    Link,
}

/// The content view the media is published on and a link points at, or none
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum View {
    #[default]
    None,
    Id(String),
}

impl View {
    pub fn id(&self) -> Option<&str> {
        match self {
            View::None => None,
            View::Id(id) => Some(id),
        }
    }
}

impl From<String> for View {
    /// Reads `none`, and `auto` as older records spelled the unchosen view
    fn from(value: String) -> Self {
        if value.is_empty() || value == "auto" || value == "none" {
            View::None
        } else {
            View::Id(value)
        }
    }
}

impl From<View> for String {
    fn from(value: View) -> Self {
        match value {
            View::None => "none".to_string(),
            View::Id(id) => id,
        }
    }
}

/// What happens to a video the upload budget would reduce under the floor
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderFloor {
    Link,
    Upload,
    #[default]
    Skip,
}

/// What happens to media the destination cannot take at any size
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverLimit {
    Link,
    #[default]
    Skip,
}

/// Whether media already fetched for an earlier job is published again from the archive
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupePolicy {
    pub enabled: bool,
    #[serde(rename = "match")]
    pub matching: DedupeMatch,
}

impl Default for DedupePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            matching: DedupeMatch::Either,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupeMatch {
    Url,
    Content,
    #[default]
    Either,
}

impl DedupeMatch {
    pub fn by_url(self) -> bool {
        matches!(self, DedupeMatch::Url | DedupeMatch::Either)
    }

    pub fn by_content(self) -> bool {
        matches!(self, DedupeMatch::Content | DedupeMatch::Either)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_limits_read_auto_or_a_number() {
        let auto: UploadPolicy = serde_json::from_str(r#"{"max_bytes":"auto"}"#).unwrap();
        assert!(auto.max_bytes.is_auto());
        assert_eq!(auto.max_bytes.cap(), u64::MAX);
        let fixed: UploadPolicy = serde_json::from_str(r#"{"max_bytes":1000}"#).unwrap();
        assert_eq!(fixed.max_bytes, UploadLimit::Bytes(1000));
        assert_eq!(
            serde_json::to_string(&auto).unwrap(),
            r#"{"max_bytes":"auto"}"#
        );
        assert_eq!(
            serde_json::to_string(&fixed).unwrap(),
            r#"{"max_bytes":1000}"#
        );
        assert!(serde_json::from_str::<UploadPolicy>(r#"{"max_bytes":"big"}"#).is_err());
    }

    #[test]
    fn views_read_none_or_an_id() {
        let policy: DeliveryPolicy = serde_json::from_str(r#"{"view":"none"}"#).unwrap();
        assert_eq!(policy.view, View::None);
        let older: DeliveryPolicy = serde_json::from_str(r#"{"view":"auto"}"#).unwrap();
        assert_eq!(older.view, View::None);
        let named: DeliveryPolicy = serde_json::from_str(r#"{"view":"abc"}"#).unwrap();
        assert_eq!(named.view, View::Id("abc".into()));
        assert_eq!(named.view.id(), Some("abc"));
        assert_eq!(View::None.id(), None);
        assert_eq!(
            serde_json::to_value(&named).unwrap()["view"],
            serde_json::json!("abc")
        );
        assert_eq!(
            serde_json::to_value(DeliveryPolicy::default()).unwrap()["view"],
            serde_json::json!("none")
        );
    }

    #[test]
    fn defaults_pass_the_check_and_zeros_fail_it() {
        let policy = Policy::default();
        assert!(policy.check().is_ok());
        assert_eq!(policy.delivery.under_floor, UnderFloor::Skip);
        assert_eq!(policy.delivery.over_limit, OverLimit::Skip);
        assert!(policy.dedupe.enabled);
        assert!(policy.dedupe.matching.by_url() && policy.dedupe.matching.by_content());
        let json = serde_json::to_value(&policy).unwrap();
        assert!(json.get("publisher").is_none());
        assert_eq!(json["dedupe"]["match"], "either");
        let back: Policy = serde_json::from_value(json).unwrap();
        assert_eq!(back, policy);
        let mut bad = Policy::default();
        bad.limits.max_height = 0;
        assert_eq!(
            bad.check().unwrap_err(),
            "limits.max_height must be at least 1"
        );
        let mut bad = Policy::default();
        bad.upload.max_bytes = UploadLimit::Bytes(0);
        assert!(bad.check().is_err());
        let mut bad = Policy::default();
        bad.delivery.link_max_bytes = 0;
        assert!(bad.check().is_err());
    }
}
