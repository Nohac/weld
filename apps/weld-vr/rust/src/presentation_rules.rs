//! Explicit local presentation preferences, not peer identity or permissions.
//! The test launcher supplies bounded rules; no emulator names live here.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
    sync::Arc,
};
use weld_client::{Extent, PresentationGroupId, PresentationRole, SurfaceBitratePreference};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowRule {
    pub app_id: String,
    pub title_suffix: String,
    pub stereo: bool,
    pub size: Extent,
    pub slot: u32,
    pub bitrate: Option<SurfaceBitratePreference>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WindowRules {
    pub entries: Vec<Arc<WindowRule>>,
}

impl WindowRules {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 4096, "presentation rules exceed 4096 bytes");
        let mut lines = text.lines();
        ensure!(
            lines.next() == Some("weld-window-rules-v1"),
            "invalid presentation rules header"
        );
        let mut entries = Vec::new();
        for line in lines {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(
                fields.len() == 8 && entries.len() < 8,
                "invalid presentation rule"
            );
            ensure!(
                !fields[0].is_empty() && fields[0].len() <= 1024 && fields[1].len() <= 1024,
                "invalid window selector"
            );
            let stereo = match fields[2] {
                "mono" => false,
                "sbs" => true,
                _ => anyhow::bail!("unknown window view layout"),
            };
            let width = fields[3].parse().context("invalid rule width")?;
            let height = fields[4].parse().context("invalid rule height")?;
            ensure!(
                crate::presentation::supported_extent(width, height),
                "rule exceeds receive pixel budget"
            );
            ensure!(
                !stereo || width % 2 == 0,
                "packed stereo width must be even"
            );
            let slot = fields[5].parse().context("invalid panel slot")?;
            ensure!(slot < 8, "panel slot exceeds window budget");
            let bitrate = if fields[6] == "-" && fields[7] == "-" {
                None
            } else {
                let group = PresentationGroupId::try_from(
                    fields[6].parse::<u32>().context("invalid bitrate group")?,
                )
                .map_err(anyhow::Error::msg)?;
                let role = match fields[7] {
                    "primary" => PresentationRole::Primary,
                    "companion" => PresentationRole::Companion,
                    "utility" => PresentationRole::Utility,
                    _ => anyhow::bail!("unknown bitrate role"),
                };
                Some(SurfaceBitratePreference { group, role })
            };
            entries.push(Arc::new(WindowRule {
                app_id: fields[0].into(),
                title_suffix: fields[1].into(),
                stereo,
                size: Extent::new(width, height),
                slot,
                bitrate,
            }));
        }
        ensure!(
            !entries.is_empty(),
            "presentation rules must select at least one window"
        );
        Ok(Self { entries })
    }

    /// A fresh launch without a file restores ordinary presentation behavior.
    pub fn consume(directory: &Path) -> Result<Self> {
        let path = directory.join("presentation.rules");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).context("open presentation rules"),
        };
        let mut text = String::new();
        file.take(4097)
            .read_to_string(&mut text)
            .context("read presentation rules")?;
        let rules = Self::parse(&text)?;
        std::fs::remove_file(path).context("consume presentation rules")?;
        Ok(rules)
    }

    pub fn select(&self, app_id: &str, title: &str) -> Option<&Arc<WindowRule>> {
        self.entries
            .iter()
            .find(|rule| rule.app_id == app_id && title.ends_with(&rule.title_suffix))
    }

    pub fn filtered(&self) -> bool {
        !self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quality_rules_are_explicit_bounded_and_optional() {
        let rules = WindowRules::parse("weld-window-rules-v1\napp\tPrimary\tsbs\t1600\t480\t0\t1\tprimary\napp\tSecondary\tmono\t640\t480\t1\t1\tcompanion\napp\t\tmono\t800\t600\t2\t-\t-\n").expect("rules");
        let primary = rules
            .select("app", "Game Primary")
            .expect("primary")
            .bitrate
            .expect("hint");
        let companion = rules
            .select("app", "Game Secondary")
            .expect("secondary")
            .bitrate
            .expect("hint");
        assert_eq!(primary.group, companion.group);
        assert_eq!(primary.role, PresentationRole::Primary);
        assert_eq!(companion.role, PresentationRole::Companion);
        assert!(
            rules
                .select("app", "Manager")
                .expect("manager")
                .bitrate
                .is_none()
        );
        for suffix in [
            "0\tprimary",
            "65536\tprimary",
            "1\tunknown",
            "-\tprimary",
            "1\t-",
            "1",
            "1\tprimary\textra",
        ] {
            assert!(
                WindowRules::parse(&format!(
                    "weld-window-rules-v1\napp\t\tmono\t640\t480\t0\t{suffix}\n"
                ))
                .is_err()
            );
        }
        assert!(WindowRules::parse("weld-window-rules-v1\napp\t\tmono\t640\t480\t0\n").is_err());
        assert!(
            WindowRules::parse("weld-window-rules-v2\napp\t\tmono\t640\t480\t0\t1\tprimary\n")
                .is_err()
        );
    }
    #[test]
    fn explicit_selectors_do_not_depend_on_window_order_or_game_title() {
        let rules = WindowRules::parse("weld-window-rules-v1\norg.example.App\t | Primary Window\tsbs\t1600\t480\t0\t-\t-\norg.example.App\t | Secondary Window\tmono\t640\t480\t1\t-\t-\n").unwrap();
        assert!(
            rules
                .select("org.example.App", "Game A | Primary Window")
                .unwrap()
                .stereo
        );
        assert!(
            rules
                .select("org.example.App", "Game B | Primary Window")
                .unwrap()
                .stereo
        );
        assert!(
            !rules
                .select("org.example.App", "Game B | Secondary Window")
                .unwrap()
                .stereo
        );
        assert!(rules.select("other", "Game B | Primary Window").is_none());
        assert!(rules.select("org.example.App", "Library").is_none());
    }
    #[test]
    fn malformed_and_oversized_rules_are_rejected() {
        for row in [
            "app\tname\tsbs\t1601\t480\t0\t-\t-",
            "app\tname\tmono\t4096\t480\t0\t-\t-",
            "app\tname\tmono\t640\t480\t8\t-\t-",
            "app\tname\tbad\t640\t480\t0\t-\t-",
            "bad",
        ] {
            assert!(WindowRules::parse(&format!("weld-window-rules-v1\n{row}\n")).is_err());
        }
        assert!(WindowRules::parse("weld-window-rules-v1\n").is_err());
        assert!(WindowRules::parse(&"x".repeat(4097)).is_err());
    }
}
