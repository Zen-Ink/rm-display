use rm_display_protocol::{ContentClass, FrameIntent, Rect};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Waveform {
    Fastest = 0,
    Fast = 1,
    Quality = 3,
    FullQuality = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshProfile {
    Realtime,
    Animate,
    #[default]
    Balanced,
    Reading,
    Quality,
    Custom,
}

impl RefreshProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Animate => "animate",
            Self::Balanced => "balanced",
            Self::Reading => "reading",
            Self::Quality => "quality",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshDecision {
    pub waveform: Waveform,
    pub complete_refresh: bool,
    pub full_refresh_reason: FullRefreshReason,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum FullRefreshReason {
    #[default]
    None,
    PartialDisabled,
    Forced,
    FirstFrame,
    Periodic,
    LargeDamage,
    StaticFastDebt,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshDebt {
    pub has_presented: bool,
    pub physical_partial_updates_since_cleanup: u32,
}

impl RefreshDebt {
    pub fn validate(self) -> Result<(), RefreshConfigError> {
        if !self.has_presented && self.physical_partial_updates_since_cleanup != 0 {
            return Err(RefreshConfigError::DebtWithoutPresentation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicyConfig {
    pub profile: RefreshProfile,
    pub latest_text_waveform: Waveform,
    pub latest_photo_waveform: Waveform,
    pub latest_video_waveform: Waveform,
    pub settled_waveform: Waveform,
    pub partial_refresh_enabled: bool,
    pub cleanup_after_updates: u32,
    pub clean_first_frame: bool,
    /// Percentage of the panel covered by one update that triggers a complete
    /// refresh. Zero disables the area trigger.
    pub large_update_threshold_percent: u8,
    /// Complete once at the next SETTLED barrier after this many fast
    /// waveform submissions. Zero disables static cleanup.
    pub static_cleanup_after_fast_updates: u32,
    pub damage_tile: u32,
}

impl RefreshPolicyConfig {
    pub const fn for_profile(profile: RefreshProfile) -> Self {
        match profile {
            RefreshProfile::Realtime => Self {
                profile,
                latest_text_waveform: Waveform::Fastest,
                latest_photo_waveform: Waveform::Fastest,
                latest_video_waveform: Waveform::Fastest,
                settled_waveform: Waveform::Fastest,
                partial_refresh_enabled: true,
                cleanup_after_updates: 360,
                clean_first_frame: true,
                large_update_threshold_percent: 0,
                static_cleanup_after_fast_updates: 12,
                damage_tile: 64,
            },
            RefreshProfile::Animate => Self {
                profile,
                latest_text_waveform: Waveform::Fastest,
                latest_photo_waveform: Waveform::Quality,
                latest_video_waveform: Waveform::Fastest,
                settled_waveform: Waveform::Fast,
                partial_refresh_enabled: true,
                cleanup_after_updates: 180,
                clean_first_frame: true,
                large_update_threshold_percent: 0,
                static_cleanup_after_fast_updates: 8,
                damage_tile: 64,
            },
            RefreshProfile::Balanced => Self {
                profile,
                latest_text_waveform: Waveform::Fast,
                latest_photo_waveform: Waveform::Quality,
                latest_video_waveform: Waveform::Fastest,
                settled_waveform: Waveform::Quality,
                partial_refresh_enabled: true,
                cleanup_after_updates: 90,
                clean_first_frame: true,
                large_update_threshold_percent: 0,
                static_cleanup_after_fast_updates: 6,
                damage_tile: 64,
            },
            RefreshProfile::Reading => Self {
                profile,
                latest_text_waveform: Waveform::Quality,
                latest_photo_waveform: Waveform::Quality,
                latest_video_waveform: Waveform::Fast,
                settled_waveform: Waveform::Quality,
                partial_refresh_enabled: true,
                cleanup_after_updates: 45,
                clean_first_frame: true,
                large_update_threshold_percent: 50,
                static_cleanup_after_fast_updates: 3,
                damage_tile: 64,
            },
            RefreshProfile::Quality => Self {
                profile,
                latest_text_waveform: Waveform::Quality,
                latest_photo_waveform: Waveform::Quality,
                latest_video_waveform: Waveform::Quality,
                settled_waveform: Waveform::Quality,
                partial_refresh_enabled: true,
                cleanup_after_updates: 20,
                clean_first_frame: true,
                large_update_threshold_percent: 33,
                static_cleanup_after_fast_updates: 0,
                damage_tile: 64,
            },
            // CUSTOM is only a neutral construction baseline. Protocol SET
            // replaces every field atomically with producer-supplied values.
            RefreshProfile::Custom => Self {
                profile,
                latest_text_waveform: Waveform::Fast,
                latest_photo_waveform: Waveform::Quality,
                latest_video_waveform: Waveform::Fastest,
                settled_waveform: Waveform::Quality,
                partial_refresh_enabled: true,
                cleanup_after_updates: 90,
                clean_first_frame: true,
                large_update_threshold_percent: 0,
                static_cleanup_after_fast_updates: 6,
                damage_tile: 64,
            },
        }
    }

    pub fn validate(self) -> Result<(), RefreshConfigError> {
        if self.damage_tile == 0 {
            return Err(RefreshConfigError::ZeroDamageTile);
        }
        if self.large_update_threshold_percent > 100 {
            return Err(RefreshConfigError::InvalidAreaThreshold);
        }
        if self.latest_text_waveform == Waveform::FullQuality
            || self.latest_photo_waveform == Waveform::FullQuality
            || self.latest_video_waveform == Waveform::FullQuality
            || self.settled_waveform == Waveform::FullQuality
        {
            return Err(RefreshConfigError::ConfiguredFullQuality);
        }
        Ok(())
    }

    /// Select another named preset while retaining explicit operator
    /// overrides and surface-local settings. Values that still match the old
    /// preset move to the new preset; customized values remain authoritative.
    pub fn switched_to(self, profile: RefreshProfile) -> Self {
        let old_preset = Self::for_profile(self.profile);
        let mut next = Self::for_profile(profile);
        if self.cleanup_after_updates != old_preset.cleanup_after_updates {
            next.cleanup_after_updates = self.cleanup_after_updates;
        }
        if self.large_update_threshold_percent != old_preset.large_update_threshold_percent {
            next.large_update_threshold_percent = self.large_update_threshold_percent;
        }
        if self.static_cleanup_after_fast_updates != old_preset.static_cleanup_after_fast_updates {
            next.static_cleanup_after_fast_updates = self.static_cleanup_after_fast_updates;
        }
        // Waveform selections define a named preset and therefore switch with
        // it. Exact custom waveforms are installed directly, not via this API.
        next.partial_refresh_enabled = self.partial_refresh_enabled;
        next.clean_first_frame = self.clean_first_frame;
        next.damage_tile = self.damage_tile;
        next
    }
}

impl Default for RefreshPolicyConfig {
    fn default() -> Self {
        Self::for_profile(RefreshProfile::Balanced)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RefreshConfigError {
    #[error("damage tile must be nonzero")]
    ZeroDamageTile,
    #[error("large-update threshold must be between 0 and 100 percent")]
    InvalidAreaThreshold,
    #[error("FULL_QUALITY is reserved for receiver-selected complete refreshes")]
    ConfiguredFullQuality,
    #[error("partial-update debt requires a previously presented panel state")]
    DebtWithoutPresentation,
}

#[derive(Debug, Clone)]
pub struct RefreshPolicy {
    config: RefreshPolicyConfig,
    presented_since_cleanup: u32,
    has_presented: bool,
}

impl RefreshPolicy {
    pub fn new(config: RefreshPolicyConfig) -> Result<Self, RefreshConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            presented_since_cleanup: 0,
            has_presented: false,
        })
    }

    pub fn decide(
        &self,
        intent: FrameIntent,
        content_class: ContentClass,
        damage_pixels: u64,
        panel_pixels: u64,
        force_cleanup: bool,
    ) -> RefreshDecision {
        let area_cleanup_due = self.config.large_update_threshold_percent > 0
            && damage_pixels.saturating_mul(100)
                >= panel_pixels
                    .saturating_mul(u64::from(self.config.large_update_threshold_percent));
        let full_refresh_reason = if !self.config.partial_refresh_enabled {
            FullRefreshReason::PartialDisabled
        } else if force_cleanup {
            FullRefreshReason::Forced
        } else if !self.has_presented && self.config.clean_first_frame {
            FullRefreshReason::FirstFrame
        } else if self.config.cleanup_after_updates > 0
            && self.presented_since_cleanup >= self.config.cleanup_after_updates
        {
            FullRefreshReason::Periodic
        } else if area_cleanup_due {
            FullRefreshReason::LargeDamage
        } else {
            FullRefreshReason::None
        };
        if full_refresh_reason != FullRefreshReason::None {
            return RefreshDecision {
                waveform: match self.config.profile {
                    RefreshProfile::Quality => Waveform::FullQuality,
                    RefreshProfile::Realtime
                    | RefreshProfile::Animate
                    | RefreshProfile::Balanced
                    | RefreshProfile::Reading => Waveform::Quality,
                    RefreshProfile::Custom => Waveform::Quality,
                },
                complete_refresh: true,
                full_refresh_reason,
            };
        }
        let waveform = if intent == FrameIntent::Settled {
            self.config.settled_waveform
        } else {
            match content_class {
                ContentClass::Photo => self.config.latest_photo_waveform,
                ContentClass::Video => self.config.latest_video_waveform,
                ContentClass::TextUi | ContentClass::Mixed | ContentClass::Unspecified => {
                    self.config.latest_text_waveform
                }
            }
        };
        RefreshDecision {
            waveform,
            complete_refresh: false,
            full_refresh_reason: FullRefreshReason::None,
        }
    }

    /// Record one successful physical panel submission. Zero-damage logical
    /// presentations do not accrue cleanup debt because no ink was updated.
    pub fn presented(&mut self, decision: RefreshDecision) {
        self.has_presented = true;
        self.presented_since_cleanup = if decision.complete_refresh {
            0
        } else {
            self.presented_since_cleanup.saturating_add(1)
        };
    }

    pub const fn presented_since_cleanup(&self) -> u32 {
        self.presented_since_cleanup
    }

    pub const fn debt(&self) -> RefreshDebt {
        RefreshDebt {
            has_presented: self.has_presented,
            physical_partial_updates_since_cleanup: self.presented_since_cleanup,
        }
    }

    pub fn restore_debt(&mut self, debt: RefreshDebt) -> Result<(), RefreshConfigError> {
        debt.validate()?;
        self.has_presented = debt.has_presented;
        self.presented_since_cleanup = debt.physical_partial_updates_since_cleanup;
        Ok(())
    }

    pub const fn config(&self) -> RefreshPolicyConfig {
        self.config
    }

    /// Change only policy configuration. Cleanup counters remain intact until
    /// the caller has successfully submitted the required complete refresh.
    pub fn reconfigure(&mut self, config: RefreshPolicyConfig) -> Result<(), RefreshConfigError> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    pub fn damage_for_decision(
        &self,
        decision: RefreshDecision,
        width: u32,
        height: u32,
        mut damage: Vec<Rect>,
    ) -> Vec<Rect> {
        if decision.complete_refresh {
            vec![Rect {
                x: 0,
                y: 0,
                width,
                height,
            }]
        } else {
            damage.shrink_to_fit();
            damage
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_presets_match_epaper_tradeoffs() {
        let realtime = RefreshPolicyConfig::for_profile(RefreshProfile::Realtime);
        let animate = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        let balanced = RefreshPolicyConfig::for_profile(RefreshProfile::Balanced);
        let reading = RefreshPolicyConfig::for_profile(RefreshProfile::Reading);
        let quality = RefreshPolicyConfig::for_profile(RefreshProfile::Quality);
        assert_eq!(realtime.cleanup_after_updates, 360);
        assert_eq!(animate.cleanup_after_updates, 180);
        assert_eq!(balanced.cleanup_after_updates, 90);
        assert_eq!(reading.cleanup_after_updates, 45);
        assert_eq!(quality.cleanup_after_updates, 20);
        assert_eq!(reading.large_update_threshold_percent, 50);
        assert_eq!(quality.large_update_threshold_percent, 33);
    }

    #[test]
    fn five_profiles_have_the_documented_latest_and_settled_waveform_matrix() {
        let cases = [
            (
                RefreshProfile::Realtime,
                [Waveform::Fastest, Waveform::Fastest, Waveform::Fastest],
                Waveform::Fastest,
            ),
            (
                RefreshProfile::Animate,
                [Waveform::Fastest, Waveform::Quality, Waveform::Fastest],
                Waveform::Fast,
            ),
            (
                RefreshProfile::Balanced,
                [Waveform::Fast, Waveform::Quality, Waveform::Fastest],
                Waveform::Quality,
            ),
            (
                RefreshProfile::Reading,
                [Waveform::Quality, Waveform::Quality, Waveform::Fast],
                Waveform::Quality,
            ),
            (
                RefreshProfile::Quality,
                [Waveform::Quality, Waveform::Quality, Waveform::Quality],
                Waveform::Quality,
            ),
        ];
        for (profile, expected, settled_waveform) in cases {
            let mut config = RefreshPolicyConfig::for_profile(profile);
            config.clean_first_frame = false;
            let policy = RefreshPolicy::new(config).unwrap();
            for (content_class, waveform) in [
                ContentClass::TextUi,
                ContentClass::Photo,
                ContentClass::Video,
            ]
            .into_iter()
            .zip(expected)
            {
                let decision = policy.decide(FrameIntent::Latest, content_class, 1, 10_000, false);
                assert_eq!(
                    (profile, content_class, decision.waveform),
                    (profile, content_class, waveform)
                );
                assert!(!decision.complete_refresh);
            }
            let settled =
                policy.decide(FrameIntent::Settled, ContentClass::TextUi, 1, 10_000, false);
            assert_eq!(settled.waveform, settled_waveform);
            assert!(!settled.complete_refresh);
        }
    }

    #[test]
    fn cleanup_uses_full_quality_only_for_quality_profile() {
        for profile in [
            RefreshProfile::Realtime,
            RefreshProfile::Animate,
            RefreshProfile::Balanced,
            RefreshProfile::Reading,
            RefreshProfile::Quality,
        ] {
            let policy = RefreshPolicy::new(RefreshPolicyConfig::for_profile(profile)).unwrap();
            let decision =
                policy.decide(FrameIntent::Latest, ContentClass::TextUi, 1, 10_000, true);
            assert!(decision.complete_refresh);
            assert_eq!(
                decision.waveform,
                if profile == RefreshProfile::Quality {
                    Waveform::FullQuality
                } else {
                    Waveform::Quality
                }
            );
        }
    }

    #[test]
    fn profile_switch_preserves_operator_overrides() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Balanced);
        config.cleanup_after_updates = 7;
        config.damage_tile = 32;
        let switched = config.switched_to(RefreshProfile::Quality);
        assert_eq!(switched.profile, RefreshProfile::Quality);
        assert_eq!(switched.cleanup_after_updates, 7);
        assert_eq!(switched.large_update_threshold_percent, 33);
        assert_eq!(switched.damage_tile, 32);

        let preset = RefreshPolicyConfig::for_profile(RefreshProfile::Balanced)
            .switched_to(RefreshProfile::Reading);
        assert_eq!(preset.cleanup_after_updates, 45);
        assert_eq!(preset.large_update_threshold_percent, 50);
        let realtime = preset.switched_to(RefreshProfile::Realtime);
        assert_eq!(realtime.cleanup_after_updates, 360);
        assert_eq!(realtime.large_update_threshold_percent, 0);
    }

    #[test]
    fn quality_profile_cleans_large_updates() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Quality);
        config.clean_first_frame = false;
        let policy = RefreshPolicy::new(config).unwrap();
        let decision = policy.decide(
            FrameIntent::Latest,
            ContentClass::TextUi,
            3_300,
            10_000,
            false,
        );
        assert!(decision.complete_refresh);
        assert_eq!(decision.waveform, Waveform::FullQuality);
        assert_eq!(decision.full_refresh_reason, FullRefreshReason::LargeDamage);
    }

    #[test]
    fn animate_settled_uses_fast_waveform() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        let policy = RefreshPolicy::new(config).unwrap();
        let decision = policy.decide(FrameIntent::Settled, ContentClass::Video, 1, 100, false);
        assert_eq!(decision.waveform, Waveform::Fast);
        assert!(!decision.complete_refresh);
    }

    #[test]
    fn custom_profile_uses_exact_content_and_settled_waveforms() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Custom);
        config.clean_first_frame = false;
        config.latest_text_waveform = Waveform::Fastest;
        config.latest_photo_waveform = Waveform::Fast;
        config.latest_video_waveform = Waveform::Quality;
        config.settled_waveform = Waveform::Fast;
        let policy = RefreshPolicy::new(config).unwrap();
        assert_eq!(
            policy
                .decide(FrameIntent::Latest, ContentClass::TextUi, 1, 100, false)
                .waveform,
            Waveform::Fastest
        );
        assert_eq!(
            policy
                .decide(FrameIntent::Latest, ContentClass::Photo, 1, 100, false)
                .waveform,
            Waveform::Fast
        );
        assert_eq!(
            policy
                .decide(FrameIntent::Latest, ContentClass::Video, 1, 100, false)
                .waveform,
            Waveform::Quality
        );
        assert_eq!(
            policy
                .decide(FrameIntent::Settled, ContentClass::Photo, 1, 100, false)
                .waveform,
            Waveform::Fast
        );
    }

    #[test]
    fn custom_profile_cannot_configure_full_quality_waveform() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Custom);
        config.settled_waveform = Waveform::FullQuality;
        assert_eq!(
            RefreshPolicy::new(config).unwrap_err(),
            RefreshConfigError::ConfiguredFullQuality
        );
    }

    #[test]
    fn disabling_partial_refresh_forces_receiver_selected_complete_updates() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Animate);
        config.clean_first_frame = false;
        config.partial_refresh_enabled = false;
        let policy = RefreshPolicy::new(config).unwrap();
        let decision = policy.decide(FrameIntent::Latest, ContentClass::Video, 1, 10_000, false);
        assert!(decision.complete_refresh);
        assert_eq!(decision.waveform, Waveform::Quality);
        assert_eq!(
            decision.full_refresh_reason,
            FullRefreshReason::PartialDisabled
        );
    }

    #[test]
    fn zero_thresholds_disable_all_configurable_automatic_cleanup_triggers() {
        let mut config = RefreshPolicyConfig::for_profile(RefreshProfile::Reading);
        config.clean_first_frame = false;
        config.cleanup_after_updates = 0;
        config.large_update_threshold_percent = 0;
        config.static_cleanup_after_fast_updates = 0;
        let mut policy = RefreshPolicy::new(config).unwrap();
        for _ in 0..500 {
            let decision = policy.decide(
                FrameIntent::Settled,
                ContentClass::TextUi,
                10_000,
                10_000,
                false,
            );
            assert!(!decision.complete_refresh);
            assert_eq!(decision.full_refresh_reason, FullRefreshReason::None);
            policy.presented(decision);
        }
    }

    #[test]
    fn refresh_debt_restore_rejects_count_without_presented_state() {
        let mut policy = RefreshPolicy::new(RefreshPolicyConfig::default()).unwrap();
        let error = policy
            .restore_debt(RefreshDebt {
                has_presented: false,
                physical_partial_updates_since_cleanup: 1,
            })
            .unwrap_err();
        assert_eq!(error, RefreshConfigError::DebtWithoutPresentation);
        assert_eq!(policy.debt(), RefreshDebt::default());
    }

    #[test]
    fn invalid_damage_tile_is_rejected() {
        let config = RefreshPolicyConfig {
            damage_tile: 0,
            ..RefreshPolicyConfig::default()
        };
        assert_eq!(
            RefreshPolicy::new(config).unwrap_err(),
            RefreshConfigError::ZeroDamageTile
        );
    }
}
