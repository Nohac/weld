//! Exposes selected cursor artwork through user data directories visible to app runtimes.
//!
//! Public links point into Weld's managed directory. The recorded source keeps
//! its package/profile path so subsequent sessions can refresh a resolved target
//! after a package upgrade. Shared file locks retain links while another Weld
//! session uses the same theme; the last session releases the managed exposure.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::ErrorKind,
    os::unix::fs::symlink,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tracing::warn;
use xcursor::CursorTheme;
use xdg::BaseDirectories;

use super::CursorIcon;

pub(crate) struct ThemeLink {
    public_link: PathBuf,
    public_target: PathBuf,
    managed_root: PathBuf,
    managed_theme: PathBuf,
    usage: File,
}

impl ThemeLink {
    fn cleanup(&self) -> Result<()> {
        let _lock = lock_updates(&self.managed_root)?;
        self.usage.unlock()?;
        match self.usage.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Error(error)) => {
                return Err(error).context("could not lock cursor theme cleanup");
            }
        }
        if occupied_by_user(&self.public_link, &self.public_target)? {
            return Ok(());
        }
        match fs::remove_file(&self.public_link) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("could not remove managed cursor link"),
        }
        for name in ["source", "current"] {
            let path = self.managed_theme.join(name);
            if fs::symlink_metadata(&path)?.is_symlink() {
                fs::remove_file(path)?;
            }
        }
        fs::remove_file(self.managed_theme.join("users.lock"))?;
        fs::remove_dir(&self.managed_theme)?;
        Ok(())
    }
}

impl Drop for ThemeLink {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            warn!(path = %self.public_link.display(), %error, "could not clean up managed cursor theme");
        }
    }
}

fn lock_updates(managed_root: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(managed_root.join(".lock"))?;
    lock.lock()?;
    Ok(lock)
}

pub(crate) fn expose(theme_name: &str) -> Result<Option<ThemeLink>> {
    let data_home = BaseDirectories::new()
        .get_data_home()
        .context("user data directory is unavailable")?;
    expose_in(&data_home, theme_name, || {
        let theme = CursorTheme::load(theme_name);
        std::iter::once(CursorIcon::Default.name())
            .chain(CursorIcon::Default.alt_names().iter().copied())
            .find_map(|name| {
                let (icon, depth) = theme.load_icon_with_depth(name)?;
                // Retain the selected theme's directory, including its inheritance metadata.
                if depth != 0 {
                    return None;
                }
                icon.parent()?.parent().map(Path::to_owned)
            })
    })
}

fn expose_in(
    data_home: &Path,
    theme_name: &str,
    discover: impl FnOnce() -> Option<PathBuf>,
) -> Result<Option<ThemeLink>> {
    ensure!(
        matches!(Path::new(theme_name).components().next(), Some(Component::Normal(name)) if name == theme_name),
        "cursor theme must be a single directory name"
    );
    let icons = data_home.join("icons");
    let public_link = icons.join(theme_name);
    let managed_root = data_home.join("weld/cursor-themes");
    let managed_theme = managed_root.join(theme_name);
    let public_target = Path::new("../weld/cursor-themes")
        .join(theme_name)
        .join("current");
    if occupied_by_user(&public_link, &public_target)? {
        return Ok(None);
    }

    fs::create_dir_all(&managed_root)?;
    let _lock = lock_updates(&managed_root)?;
    if occupied_by_user(&public_link, &public_target)? {
        return Ok(None);
    }

    let source_link = managed_theme.join("source");
    let source = match fs::read_link(&source_link) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let Some(source) = discover() else {
                return Ok(None);
            };
            ensure!(source.is_absolute(), "cursor theme source must be absolute");
            source
        }
        Err(error) => return Err(error).context("could not read managed cursor theme source"),
    };
    let resolved = source
        .canonicalize()
        .context("could not resolve cursor theme directory")?;
    ensure!(resolved.is_dir(), "cursor theme source is not a directory");
    ensure!(
        !resolved.starts_with(&managed_root),
        "cursor theme source points into managed links"
    );
    fs::create_dir_all(&managed_theme)?;
    match symlink(&source, &source_link) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("could not record cursor theme source"),
    }

    let current = managed_theme.join("current");
    match fs::symlink_metadata(&current) {
        Ok(metadata) => ensure!(
            metadata.is_symlink(),
            "managed cursor target is not a symlink"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("could not inspect managed cursor target"),
    }
    if fs::read_link(&current).ok().as_ref() != Some(&resolved) {
        let staging = tempfile::tempdir_in(&managed_theme)?;
        let replacement = staging.path().join("current");
        symlink(resolved, &replacement)?;
        fs::rename(replacement, &current)?;
    }
    fs::create_dir_all(icons)?;
    // Exclusive creation also preserves entries installed concurrently by another tool.
    match symlink(&public_target, &public_link) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("could not expose cursor theme in user icons"),
    }
    let usage = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(managed_theme.join("users.lock"))?;
    usage.lock_shared()?;
    Ok(Some(ThemeLink {
        public_link,
        public_target,
        managed_root,
        managed_theme,
        usage,
    }))
}

fn occupied_by_user(link: &Path, managed_target: &Path) -> Result<bool> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.is_symlink() => Ok(fs::read_link(link)? != managed_target),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("could not inspect user cursor theme entry"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_and_refreshes_owned_theme_after_profile_upgrade() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let first = temp.path().join("package-1");
        let second = temp.path().join("package-2");
        fs::create_dir(&first)?;
        fs::create_dir(&second)?;
        let profile = temp.path().join("profile");
        symlink(&first, &profile)?;
        let first_session = expose_in(&data, "Theme", || Some(profile.clone()))?;
        let public = data.join("icons/Theme");
        assert_eq!(public.canonicalize()?, first);
        fs::remove_file(&profile)?;
        symlink(&second, &profile)?;
        let second_session = expose_in(&data, "Theme", || None)?;
        assert_eq!(public.canonicalize()?, second);
        drop(first_session);
        assert_eq!(public.canonicalize()?, second);
        drop(second_session);
        assert!(fs::symlink_metadata(public).is_err());
        assert!(!data.join("weld/cursor-themes/Theme").exists());
        Ok(())
    }

    #[test]
    fn preserves_existing_files_directories_and_dangling_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let icons = temp.path().join("icons");
        fs::create_dir(&icons)?;
        fs::write(icons.join("File"), "user-owned")?;
        fs::create_dir(icons.join("Directory"))?;
        symlink("missing", icons.join("Link"))?;
        for theme in ["File", "Directory", "Link"] {
            expose_in(temp.path(), theme, || {
                panic!("existing entry must be preserved")
            })?;
        }
        assert_eq!(fs::read_to_string(icons.join("File"))?, "user-owned");
        assert!(icons.join("Directory").is_dir());
        assert_eq!(fs::read_link(icons.join("Link"))?, Path::new("missing"));
        assert!(!temp.path().join("weld").exists());
        Ok(())
    }

    #[test]
    fn rejects_path_names_and_keeps_old_target_when_source_disappears() -> Result<()> {
        let temp = tempfile::tempdir()?;
        for theme in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "nested/theme",
            "Theme/",
        ] {
            assert!(expose_in(temp.path(), theme, || None).is_err(), "{theme:?}");
        }
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        let session = expose_in(temp.path(), "Theme", || Some(source.clone()))?;
        fs::remove_dir(&source)?;
        assert!(expose_in(temp.path(), "Theme", || None).is_err());
        assert_eq!(
            fs::read_link(temp.path().join("weld/cursor-themes/Theme/current"))?,
            source
        );
        drop(session);
        assert!(fs::symlink_metadata(temp.path().join("icons/Theme")).is_err());
        Ok(())
    }

    #[test]
    fn changing_theme_cleans_old_exposure_and_preserves_user_replacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        let mut selected = expose_in(temp.path(), "First", || Some(source.clone()))?;
        assert!(selected.is_some());
        selected = expose_in(temp.path(), "Second", || Some(source.clone()))?;
        assert!(fs::symlink_metadata(temp.path().join("icons/First")).is_err());
        let public = temp.path().join("icons/Second");
        fs::remove_file(&public)?;
        symlink(&source, &public)?;
        drop(selected);
        assert_eq!(fs::read_link(public)?, source);
        Ok(())
    }
}
