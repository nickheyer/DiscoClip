//! Whether the destination gets the file or a link to the page that plays it.

use crate::policy::{DeliveryMode, DeliveryPolicy, OverLimit, UnderFloor};

/// Whether a page is there to link to, or why not
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAvailability {
    Available,
    Unavailable(String),
}

/// The point in making the output at which the choice has to be made
#[derive(Debug, Clone, PartialEq)]
pub enum Moment {
    /// Nothing tried yet
    Start,
    /// A file over the limit that cannot be reduced
    CannotShrink { size: u64, max_bytes: u64 },
    /// The byte budget cannot hold the floor's bitrate over the clip
    BudgetUnderFloor {
        max_bytes: u64,
        budget_bps: u64,
        secs: f64,
        floor_bps: u64,
    },
    /// The encoder ran out of ladder under the budget
    BudgetUnreachable { max_bytes: u64, duration_secs: u64 },
    /// The encoded output falls under the floor
    BelowFloor { reason: String },
    /// The output is over the limit after all
    OverLimit { size: u64, max_bytes: u64 },
}

impl Moment {
    /// Why the file is not simply uploaded
    pub fn reason(&self) -> String {
        match self {
            Moment::Start => "delivery is set to link".to_string(),
            Moment::CannotShrink { size, max_bytes } | Moment::OverLimit { size, max_bytes } => {
                format!("the file is {size} bytes, over the {max_bytes} the destination takes")
            }
            Moment::BudgetUnderFloor {
                max_bytes,
                budget_bps,
                secs,
                floor_bps,
            } => format!(
                "{max_bytes} bytes hold only {} kb/s of {secs:.0}s, under the {} kb/s floor",
                budget_bps / 1000,
                floor_bps / 1000
            ),
            Moment::BudgetUnreachable {
                max_bytes,
                duration_secs,
            } => format!("{duration_secs}s of video cannot be fit under {max_bytes} bytes"),
            Moment::BelowFloor { reason } => reason.clone(),
        }
    }

    fn size(&self) -> Option<u64> {
        match self {
            Moment::CannotShrink { size, .. } | Moment::OverLimit { size, .. } => Some(*size),
            _ => None,
        }
    }

    fn is_floor(&self) -> bool {
        matches!(
            self,
            Moment::BudgetUnderFloor { .. } | Moment::BelowFloor { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Go on with, or accept, the upload
    Upload,
    /// Post a link to the page instead
    Link { reason: String },
    /// Post nothing and fail the job for this reason
    Skip { reason: String },
}

/// What becomes of the output at `moment` under `policy`, given whether a page can be linked
pub fn decide(policy: &DeliveryPolicy, link: &LinkAvailability, moment: Moment) -> Outcome {
    let upload_only = LinkAvailability::Unavailable("delivery here is upload only".to_string());
    let link = if policy.mode == DeliveryMode::Upload {
        &upload_only
    } else {
        link
    };
    let reason = moment.reason();
    let link_or_skip = |reason: String| -> Outcome {
        if let Some(size) = moment.size()
            && size > policy.link_max_bytes
        {
            return Outcome::Skip {
                reason: format!("{reason} and the {} a page takes", policy.link_max_bytes),
            };
        }
        match link {
            LinkAvailability::Available => Outcome::Link { reason },
            LinkAvailability::Unavailable(why) if matches!(moment, Moment::Start) => {
                Outcome::Skip {
                    reason: format!("{reason}, but {why}"),
                }
            }
            LinkAvailability::Unavailable(why) => Outcome::Skip {
                reason: format!("{reason}, and no page is there to link to: {why}"),
            },
        }
    };
    match (&moment, policy.mode) {
        (Moment::Start, DeliveryMode::Link) => link_or_skip(reason),
        (Moment::Start, _) => Outcome::Upload,
        (_, DeliveryMode::Link) => link_or_skip(reason),
        (at, _) if at.is_floor() => match policy.under_floor {
            UnderFloor::Link => link_or_skip(reason),
            UnderFloor::Upload => Outcome::Upload,
            UnderFloor::Skip => Outcome::Skip { reason },
        },
        _ => match policy.over_limit {
            OverLimit::Link => link_or_skip(reason),
            OverLimit::Skip => Outcome::Skip { reason },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::QualityFloor;

    fn policy(
        mode: DeliveryMode,
        under_floor: UnderFloor,
        over_limit: OverLimit,
    ) -> DeliveryPolicy {
        DeliveryPolicy {
            mode,
            under_floor,
            over_limit,
            link_max_bytes: 1000,
            floor: QualityFloor {
                min_height: 720,
                min_bitrate: 1_000_000,
            },
            ..DeliveryPolicy::default()
        }
    }

    fn page() -> LinkAvailability {
        LinkAvailability::Available
    }

    fn no_page() -> LinkAvailability {
        LinkAvailability::Unavailable("the profile names no content view".into())
    }

    fn under_floor() -> Moment {
        Moment::BudgetUnderFloor {
            max_bytes: 100,
            budget_bps: 500_000,
            secs: 20.0,
            floor_bps: 1_000_000,
        }
    }

    #[test]
    fn auto_follows_the_floor_and_limit_actions() {
        let links = policy(DeliveryMode::Auto, UnderFloor::Link, OverLimit::Link);
        assert_eq!(decide(&links, &page(), Moment::Start), Outcome::Upload);
        assert_eq!(
            decide(&links, &page(), under_floor()),
            Outcome::Link {
                reason: "100 bytes hold only 500 kb/s of 20s, under the 1000 kb/s floor".into()
            }
        );
        assert_eq!(
            decide(&links, &no_page(), under_floor()),
            Outcome::Skip {
                reason: "100 bytes hold only 500 kb/s of 20s, under the 1000 kb/s floor, and no \
                         page is there to link to: the profile names no content view"
                    .into()
            }
        );
        let too_big = Moment::CannotShrink {
            size: 900,
            max_bytes: 100,
        };
        assert_eq!(
            decide(&links, &page(), too_big.clone()),
            Outcome::Link {
                reason: "the file is 900 bytes, over the 100 the destination takes".into()
            }
        );
        let huge = Moment::OverLimit {
            size: 5000,
            max_bytes: 100,
        };
        assert_eq!(
            decide(&links, &page(), huge),
            Outcome::Skip {
                reason: "the file is 5000 bytes, over the 100 the destination takes and the 1000 \
                         a page takes"
                    .into()
            }
        );
        let uploads = policy(DeliveryMode::Auto, UnderFloor::Upload, OverLimit::Skip);
        assert_eq!(decide(&uploads, &no_page(), under_floor()), Outcome::Upload);
        assert_eq!(
            decide(&uploads, &page(), too_big),
            Outcome::Skip {
                reason: "the file is 900 bytes, over the 100 the destination takes".into()
            }
        );
        let skips = policy(DeliveryMode::Auto, UnderFloor::Skip, OverLimit::Skip);
        assert_eq!(
            decide(&skips, &page(), under_floor()),
            Outcome::Skip {
                reason: "100 bytes hold only 500 kb/s of 20s, under the 1000 kb/s floor".into()
            }
        );
        assert_eq!(
            decide(
                &skips,
                &page(),
                Moment::BudgetUnreachable {
                    max_bytes: 100,
                    duration_secs: 20
                }
            ),
            Outcome::Skip {
                reason: "20s of video cannot be fit under 100 bytes".into()
            }
        );
    }

    #[test]
    fn link_mode_always_links_and_upload_mode_never_does() {
        let links = policy(DeliveryMode::Link, UnderFloor::Skip, OverLimit::Skip);
        assert_eq!(
            decide(&links, &page(), Moment::Start),
            Outcome::Link {
                reason: "delivery is set to link".into()
            }
        );
        assert_eq!(
            decide(&links, &no_page(), Moment::Start),
            Outcome::Skip {
                reason: "delivery is set to link, but the profile names no content view".into()
            }
        );
        assert!(matches!(
            decide(
                &links,
                &page(),
                Moment::BelowFloor {
                    reason: "low".into()
                }
            ),
            Outcome::Link { .. }
        ));
        let uploads = policy(DeliveryMode::Upload, UnderFloor::Link, OverLimit::Link);
        assert_eq!(decide(&uploads, &page(), Moment::Start), Outcome::Upload);
        assert_eq!(
            decide(&uploads, &page(), under_floor()),
            Outcome::Skip {
                reason: "100 bytes hold only 500 kb/s of 20s, under the 1000 kb/s floor, and no \
                         page is there to link to: delivery here is upload only"
                    .into()
            }
        );
        let lenient = policy(DeliveryMode::Upload, UnderFloor::Upload, OverLimit::Skip);
        assert_eq!(decide(&lenient, &page(), under_floor()), Outcome::Upload);
    }
}
