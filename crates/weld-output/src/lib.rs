//! Validated output preferences shared by configuration backends and native hosts.

use std::str::FromStr;

use anyhow::{Context, Result, bail};

/// Valid logical scale applied to a physical compositor output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputScale(f64);

/// Direction of an interactive quarter-step scale adjustment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputScaleAdjustment {
    Increase,
    Decrease,
}

impl OutputScale {
    const STEP: f64 = 0.25;

    /// Validate a finite positive factor. Hosts additionally validate against
    /// their output dimensions and protocol representation.
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || value <= 0.0 {
            bail!("output scale must be finite and positive");
        }
        Ok(Self(value))
    }

    /// Return the configured physical-pixels-per-logical-pixel ratio.
    pub const fn value(self) -> f64 {
        self.0
    }

    /// Move to the next quarter step in the requested direction.
    pub fn adjust(self, adjustment: OutputScaleAdjustment) -> Option<Self> {
        let next = match adjustment {
            OutputScaleAdjustment::Increase => ((self.0 / Self::STEP).floor() + 1.0) * Self::STEP,
            OutputScaleAdjustment::Decrease if self.0 <= Self::STEP => return None,
            OutputScaleAdjustment::Decrease => {
                (((self.0 / Self::STEP).ceil() - 1.0) * Self::STEP).max(Self::STEP)
            }
        };
        Self::new(next).ok().filter(|next| *next != self)
    }
}

impl Default for OutputScale {
    fn default() -> Self {
        Self(1.0)
    }
}

impl FromStr for OutputScale {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(
            value
                .parse::<f64>()
                .with_context(|| format!("invalid output scale {value:?}"))?,
        )
    }
}

/// An exact connector/monitor identifier or a default for every output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputSelector {
    All,
    Named(String),
}

impl OutputSelector {
    /// Match connector names or EDID manufacturer/model/serial identifiers.
    pub fn matches(&self, connector: &str, identifier: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Named(name) => name == connector || identifier == Some(name.as_str()),
        }
    }
}

/// One explicit scale preference and the outputs it selects.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputScaleRule {
    pub selector: OutputSelector,
    pub scale: OutputScale,
}

/// Ordered defaults and named overrides. Later rules at the same
/// specificity win; named rules take precedence over wildcard defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputSettings {
    pub scales: Vec<OutputScaleRule>,
}

impl OutputSettings {
    /// Resolve an output's configured override, preserving the host default
    /// when no rule matches.
    pub fn scale_for(&self, connector: &str, identifier: Option<&str>) -> Option<OutputScale> {
        self.scales
            .iter()
            .rev()
            .find_map(|rule| match &rule.selector {
                OutputSelector::Named(_) if rule.selector.matches(connector, identifier) => {
                    Some(rule.scale)
                }
                _ => None,
            })
            .or_else(|| {
                self.scales
                    .iter()
                    .rev()
                    .find_map(|rule| (rule.selector == OutputSelector::All).then_some(rule.scale))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_identifiers_follow_connectors_and_preserve_specific_overrides() {
        let mut settings = OutputSettings {
            scales: vec![
                OutputScaleRule {
                    selector: OutputSelector::Named("Example Panel SERIAL-7".into()),
                    scale: "1.5".parse().expect("scale"),
                },
                OutputScaleRule {
                    selector: OutputSelector::All,
                    scale: "1.25".parse().expect("scale"),
                },
            ],
        };
        for connector in ["DP-1", "DP-2"] {
            assert_eq!(
                settings
                    .scale_for(connector, Some("Example Panel SERIAL-7"))
                    .map(OutputScale::value),
                Some(1.5)
            );
        }
        for identifier in [None, Some("Example Panel SERIAL-8")] {
            assert_eq!(
                settings
                    .scale_for("DP-1", identifier)
                    .map(OutputScale::value),
                Some(1.25)
            );
        }
        settings.scales.push(OutputScaleRule {
            selector: OutputSelector::Named("DP-2".into()),
            scale: "2".parse().expect("scale"),
        });
        assert_eq!(
            settings
                .scale_for("DP-2", Some("Example Panel SERIAL-7"))
                .map(OutputScale::value),
            Some(2.0)
        );
        settings
            .scales
            .retain(|rule| rule.selector != OutputSelector::All);
        assert_eq!(settings.scale_for("DP-1", None), None);
    }

    #[test]
    fn connector_overrides_defaults_and_last_matching_rule_wins() {
        let settings = OutputSettings {
            scales: vec![
                OutputScaleRule {
                    selector: OutputSelector::Named("eDP-1".into()),
                    scale: "1.25".parse().expect("scale"),
                },
                OutputScaleRule {
                    selector: OutputSelector::All,
                    scale: "2".parse().expect("scale"),
                },
                OutputScaleRule {
                    selector: OutputSelector::Named("eDP-1".into()),
                    scale: "1.5".parse().expect("scale"),
                },
            ],
        };
        assert_eq!(
            settings.scale_for("eDP-1", None).map(OutputScale::value),
            Some(1.5)
        );
        assert_eq!(
            settings.scale_for("DP-1", None).map(OutputScale::value),
            Some(2.0)
        );
        assert_eq!(OutputSettings::default().scale_for("eDP-1", None), None);
    }
}
