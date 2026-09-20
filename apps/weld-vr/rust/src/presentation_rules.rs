//! Explicit local presentation preferences, not peer identity or permissions.
//! The test launcher supplies bounded rules; no emulator names live here.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
    sync::Arc,
};
use weld_client::{Extent, PresentationGroupId, PresentationRole, SurfaceBitratePreference};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RuleInput")]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDocument {
    rules: Vec<WindowRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleInput {
    app_id: String,
    title_suffix: String,
    stereo: bool,
    width: u32,
    height: u32,
    slot: u32,
    #[serde(default)]
    bitrate: Option<RuleBitrate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleBitrate {
    group: PresentationGroupId,
    role: RuleRole,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuleRole {
    Primary,
    Companion,
    Utility,
}

impl TryFrom<RuleInput> for WindowRule {
    type Error = anyhow::Error;

    fn try_from(rule: RuleInput) -> Result<Self> {
        ensure!(
            !rule.app_id.is_empty() && rule.app_id.len() <= 1024 && rule.title_suffix.len() <= 1024,
            "invalid window selector"
        );
        ensure!(
            crate::presentation::supported_extent(rule.width, rule.height),
            "rule exceeds receive pixel budget"
        );
        ensure!(
            !rule.stereo || rule.width.is_multiple_of(2),
            "packed stereo width must be even"
        );
        ensure!(rule.slot < 8, "panel slot exceeds window budget");
        Ok(Self {
            app_id: rule.app_id,
            title_suffix: rule.title_suffix,
            stereo: rule.stereo,
            size: Extent::new(rule.width, rule.height),
            slot: rule.slot,
            bitrate: rule.bitrate.map(|bitrate| SurfaceBitratePreference {
                group: bitrate.group,
                role: match bitrate.role {
                    RuleRole::Primary => PresentationRole::Primary,
                    RuleRole::Companion => PresentationRole::Companion,
                    RuleRole::Utility => PresentationRole::Utility,
                },
            }),
        })
    }
}

impl WindowRules {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 4096, "presentation rules exceed 4096 bytes");
        let document: RuleDocument =
            serde_json::from_str(text).context("parse presentation rules JSON")?;
        ensure!(
            (1..=8).contains(&document.rules.len()),
            "presentation rules must contain between one and eight entries"
        );
        Ok(Self {
            entries: document.rules.into_iter().map(Arc::new).collect(),
        })
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
    use std::process::Command;

    fn rule() -> RuleInput {
        RuleInput {
            app_id: "app".into(),
            title_suffix: "Primary".into(),
            stereo: true,
            width: 1600,
            height: 480,
            slot: 0,
            bitrate: None,
        }
    }

    #[test]
    fn actual_launcher_json_parses_with_matching_roles_and_catchall_last() {
        let launcher = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/run-azahar-xr");
        for manager in [false, true] {
            let output = Command::new("python3").args(["-I", "-c",
                "import runpy,sys; print(runpy.run_path(sys.argv[1])['rules'](sys.argv[2] == 'true'))"])
                .arg(&launcher).arg(manager.to_string()).output().expect("launcher rules");
            assert!(output.status.success());
            let text = String::from_utf8(output.stdout).expect("JSON");
            let rules = WindowRules::parse(&text).expect("parse actual Python output");
            assert_eq!(rules.entries.len(), if manager { 3 } else { 2 });
            for (suffix, role, stereo, width) in [
                (" | Primary Window", PresentationRole::Primary, true, 1600),
                (
                    " | Secondary Window",
                    PresentationRole::Companion,
                    false,
                    640,
                ),
            ] {
                let selected = rules
                    .select("org.azahar_emu.Azahar", &format!("Any game{suffix}"))
                    .expect("selected");
                assert_eq!(selected.stereo, stereo);
                assert_eq!(selected.size, Extent::new(width, 480));
                assert_eq!(
                    selected.bitrate,
                    Some(SurfaceBitratePreference {
                        group: PresentationGroupId::try_from(1).expect("group"),
                        role,
                    })
                );
            }
            assert_eq!(
                rules.select("org.azahar_emu.Azahar", "Manager").is_some(),
                manager
            );
        }
    }

    #[test]
    fn selectors_match_app_and_suffix_independently_of_game_title() {
        let rules = WindowRules {
            entries: vec![Arc::new(WindowRule::try_from(rule()).expect("valid rule"))],
        };
        assert!(rules.select("app", "Game A Primary").is_some());
        assert!(rules.select("app", "Game B Primary").is_some());
        assert!(rules.select("other", "Game A Primary").is_none());
        assert!(rules.select("app", "Library").is_none());
    }

    #[test]
    fn rule_validation_enforces_weld_label_pixel_and_slot_budgets() {
        fn rejects(change: impl FnOnce(&mut RuleInput)) {
            let mut input = rule();
            change(&mut input);
            assert!(WindowRule::try_from(input).is_err());
        }
        rejects(|input| input.app_id.clear());
        rejects(|input| input.app_id = "x".repeat(1025));
        rejects(|input| input.title_suffix = "ø".repeat(513));
        rejects(|input| input.width = 1601);
        rejects(|input| input.width = 4096);
        rejects(|input| input.width = 0);
        rejects(|input| input.slot = 8);
        let mut mono = rule();
        mono.stereo = false;
        mono.width = 1601;
        assert!(
            WindowRule::try_from(mono).is_ok(),
            "even width is a stereo constraint"
        );
    }

    #[test]
    fn document_limits_bound_rule_count_and_input_bytes() {
        let entry = r#"{"app_id":"app","title_suffix":"Primary","stereo":true,"width":1600,"height":480,"slot":0}"#;
        for (count, accepted) in [(0, false), (8, true), (9, false)] {
            let text = format!(r#"{{"rules":[{}]}}"#, vec![entry; count].join(","));
            assert_eq!(WindowRules::parse(&text).is_ok(), accepted);
        }
        let mut text = format!(r#"{{"rules":[{entry}]}}"#);
        text.push_str(&" ".repeat(4096 - text.len()));
        assert!(WindowRules::parse(&text).is_ok());
        text.push(' ');
        assert!(WindowRules::parse(&text).is_err());
    }
}
