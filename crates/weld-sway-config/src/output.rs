//! Sway output directives translated into native output preferences.

use crate::{
    Statement,
    evaluation::{ConfigWarning, UnsupportedPolicy, line_of, unsupported},
};
use anyhow::{Context, Result, ensure};
use weld_output::{OutputScaleRule, OutputSelector, OutputSettings};

/// Apply one output directive from an already variable-expanded configuration.
/// Unsupported properties retain source-located diagnostics while supported
/// properties on the same line continue to apply under the warning policy.
pub fn apply(
    settings: &mut OutputSettings,
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<()> {
    let mut arguments = statement.arguments().iter();
    let selector =
        crate::value::parse(arguments.next().context("missing output selector")?.text())?;
    ensure!(!selector.is_empty(), "output selector must not be empty");
    let selector = if selector == "*" {
        OutputSelector::All
    } else {
        OutputSelector::Named(selector.to_owned())
    };
    if let Some(block) = statement.block() {
        ensure!(
            arguments.next().is_none(),
            "unexpected output block arguments"
        );
        for property in block.statements() {
            if property.block().is_some() {
                policy.handle(
                    unsupported("nested output blocks are not supported"),
                    name,
                    line_of(source, property)?,
                    warnings,
                )?;
                continue;
            }
            let words = std::iter::once(property.name().text())
                .chain(property.arguments().iter().map(|word| word.text()));
            apply_properties(
                settings,
                &selector,
                words,
                (name, line_of(source, property)?),
                policy,
                warnings,
            )?;
        }
    } else {
        ensure!(arguments.len() > 0, "output requires a property or block");
        apply_properties(
            settings,
            &selector,
            arguments.map(|word| word.text()),
            (name, line_of(source, statement)?),
            policy,
            warnings,
        )?;
    }
    Ok(())
}

fn apply_properties<'a>(
    settings: &mut OutputSettings,
    selector: &OutputSelector,
    words: impl Iterator<Item = &'a str>,
    location: (&str, usize),
    policy: UnsupportedPolicy,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<()> {
    let mut words = words.peekable();
    while let Some(property) = words.next() {
        if property == "scale" {
            let value =
                crate::value::parse(words.next().context("output scale requires a factor")?)?;
            settings.scales.push(OutputScaleRule {
                selector: selector.clone(),
                scale: value.parse()?,
            });
            continue;
        }
        policy.handle(
            unsupported(format!("unsupported output property {property}")),
            location.0,
            location.1,
            warnings,
        )?;
        // Consume known Sway property arities so a later scale retains its own
        // meaning. An unfamiliar property leaves its remaining tail uninterpreted.
        let count = match property {
            "enable" | "disable" => 0,
            "adaptive_sync" | "transform" | "scale_filter" | "power" | "dpms"
            | "max_render_time" | "subpixel" => 1,
            "mode" | "resolution" => {
                if words.peek() == Some(&"--custom") {
                    words.next();
                }
                1
            }
            "pos" | "position" => {
                let first = words
                    .next()
                    .context("output position requires coordinates")?;
                usize::from(!first.contains(','))
            }
            _ => break,
        };
        for _ in 0..count {
            words
                .next()
                .with_context(|| format!("missing value for output property {property}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(source: &str) -> Result<(OutputSettings, Vec<ConfigWarning>)> {
        let parsed = crate::parse("outputs", source)?;
        let mut settings = OutputSettings::default();
        let mut warnings = Vec::new();
        for statement in parsed.statements() {
            apply(
                &mut settings,
                statement,
                "outputs",
                source,
                UnsupportedPolicy::Warn,
                &mut warnings,
            )?;
        }
        Ok((settings, warnings))
    }

    #[test]
    fn scale_survives_known_unsupported_properties_and_block_forms() -> Result<()> {
        let (settings, warnings) = compile(
            "output * pos 0 0 scale 1.25 adaptive_sync on\noutput 'eDP-1' {\n scale 1.5\n}\n",
        )?;
        assert_eq!(
            settings.scale_for("DP-1", None).map(|scale| scale.value()),
            Some(1.25)
        );
        assert_eq!(
            settings.scale_for("eDP-1", None).map(|scale| scale.value()),
            Some(1.5)
        );
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| warning.line == 1));
        for invalid in [
            "output * scale",
            "output * scale 0",
            "output * scale -1",
            "output * scale NaN",
            "output * scale inf",
        ] {
            assert!(compile(invalid).is_err(), "{invalid}");
        }
        Ok(())
    }

    #[test]
    fn quoted_monitor_identifier_selects_only_the_matching_monitor() -> Result<()> {
        let (settings, warnings) =
            compile("output 'Example Panel SERIAL-7' pos 0 0 scale 1.5 adaptive_sync on")?;
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            settings
                .scale_for("eDP-1", Some("Example Panel SERIAL-7"))
                .map(|scale| scale.value()),
            Some(1.5)
        );
        assert_eq!(
            settings.scale_for("HDMI-A-1", Some("Another Panel SERIAL-8")),
            None
        );
        assert_eq!(settings.scale_for("eDP-1", None), None);
        Ok(())
    }
}
