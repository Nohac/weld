//! Alternative views of the host's application inventory preserve window identity.
use std::collections::BTreeMap;
use weld_hoist_iroh::pairing::ApplicationInfo;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Grouping {
    #[default]
    Windows,
    Application,
}

pub struct Group<'a> {
    pub label: &'a str,
    pub windows: Vec<&'a ApplicationInfo>,
}

pub fn groups(applications: &[ApplicationInfo], grouping: Grouping) -> Vec<Group<'_>> {
    match grouping {
        Grouping::Windows => vec![Group {
            label: "Running windows",
            windows: applications.iter().collect(),
        }],
        Grouping::Application => {
            let mut groups = BTreeMap::<&str, Vec<&ApplicationInfo>>::new();
            for application in applications {
                let label = if application.app_id.is_empty() {
                    "Other applications"
                } else {
                    &application.app_id
                };
                groups.entry(label).or_default().push(application);
            }
            groups
                .into_iter()
                .map(|(label, windows)| Group { label, windows })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
    #[test]
    fn regrouping_preserves_independent_window_targets_and_availability() {
        let application = |window, app_id: &str, available| ApplicationInfo {
            window,
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), window), window),
            title: "Same title".into(),
            app_id: app_id.into(),
            available,
            hoisted_here: false,
        };
        let apps = [
            application(1, "kitty", true),
            application(2, "foot", true),
            application(3, "kitty", false),
        ];
        let grouped = groups(&apps, Grouping::Application);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[1].label, "kitty");
        assert_eq!(
            grouped[1]
                .windows
                .iter()
                .map(|app| app.window)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(!grouped[1].windows[1].available);
        assert_eq!(groups(&apps, Grouping::Windows)[0].windows.len(), 3);
    }
}
