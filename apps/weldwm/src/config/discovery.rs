//! Distribution config discovery. Explicit choices never fall through on load errors.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

pub(crate) fn discover(explicit: Option<&Path>) -> Result<PathBuf> {
    discover_with(explicit, &env::current_dir()?, |key| env::var_os(key))
}

fn discover_with(
    explicit: Option<&Path>,
    working_directory: &Path,
    variable: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    let nonempty = |key: &str| variable(key).filter(|value| !value.is_empty());
    if let Some(path) = nonempty("WELD_CONFIG") {
        return Ok(path.into());
    }
    let user_directory = nonempty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            nonempty("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        });
    let system_directories = nonempty("XDG_CONFIG_DIRS")
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")]);
    let candidates = user_directory
        .into_iter()
        .chain(
            system_directories
                .into_iter()
                .filter(|path| path.is_absolute()),
        )
        .map(|directory| directory.join("weld/config"))
        .chain(std::iter::once(
            working_directory.join("examples/master.sway.config"),
        ))
        .collect::<Vec<_>>();
    for candidate in &candidates {
        if candidate
            .try_exists()
            .with_context(|| format!("checking {}", candidate.display()))?
        {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "no Weld configuration found; use --config PATH or WELD_CONFIG (searched: {})",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::super::ConfigPlugin;
    use super::*;
    use std::fs;

    #[test]
    fn discovery_orders_explicit_environment_user_system_and_working_directory() {
        let directory = tempfile::tempdir().expect("directory");
        let root = directory.path();
        let user = root.join("user");
        let system = root.join("system");
        let override_file = root.join("override");
        let explicit = root.join("explicit");
        let candidates = [
            user.join("weld/config"),
            system.join("weld/config"),
            root.join("examples/master.sway.config"),
        ];
        for path in &candidates {
            fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
            fs::write(path, "font monospace 14").expect("configuration");
        }
        let environment = |key: &str| match key {
            "XDG_CONFIG_HOME" => Some(user.clone().into_os_string()),
            "XDG_CONFIG_DIRS" => Some(system.clone().into_os_string()),
            _ => None,
        };
        // Missing explicit files are returned and rejected by loading, never replaced.
        assert_eq!(
            discover_with(Some(&explicit), root, environment).expect("explicit"),
            explicit
        );
        let selected = discover_with(None, root, |key| {
            if key == "WELD_CONFIG" {
                Some(override_file.clone().into_os_string())
            } else {
                environment(key)
            }
        })
        .expect("environment override");
        assert_eq!(selected, override_file);
        assert!(ConfigPlugin::load(&selected).is_err());
        for path in &candidates {
            assert_eq!(
                discover_with(None, root, environment).expect("discovered"),
                *path
            );
            fs::remove_file(path).expect("remove candidate");
        }
        assert!(discover_with(None, root, environment).is_err());
    }

    #[test]
    fn relative_xdg_home_uses_home_and_invalid_selected_config_does_not_fall_back() {
        let directory = tempfile::tempdir().expect("directory");
        let home = directory.path();
        let config = home.join(".config/weld/config");
        fs::create_dir_all(config.parent().expect("parent")).expect("parent directory");
        fs::write(&config, "font monospace 14").expect("config");
        let found = discover_with(None, home, |key| match key {
            "HOME" => Some(home.as_os_str().to_owned()),
            "XDG_CONFIG_HOME" => Some("relative".into()),
            "WELD_CONFIG" => Some("".into()),
            "XDG_CONFIG_DIRS" => Some(home.join("system").into_os_string()),
            _ => None,
        })
        .expect("home fallback");
        assert_eq!(found, config);
        ConfigPlugin::load(&found).expect("load discovered config");
        fs::write(&config, "gaps inner invalid").expect("invalid config");
        assert!(ConfigPlugin::load(&found).is_err());
    }
}
