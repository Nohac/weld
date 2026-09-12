//! Same-user private storage, not attestation or protection from that OS user.
//! Directory fds pin checked parents; writes publish complete files without overwrite.

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, Mode, OFlags, linkat, mkdirat, open, openat, unlinkat},
    io::Errno,
    process::geteuid,
};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Write,
    os::unix::fs::MetadataExt,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

pub(crate) struct PrivateDirectory(File);

impl PrivateDirectory {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::from(
            open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .with_context(|| format!("could not open private Iroh directory {}", path.display()))?,
        );
        Self::verify(file)
    }

    /// Creates only the final component. Intermediate directories must exist.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let (parent, name) = split_path(path)?;
        let parent = File::from(
            open(
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("could not open device directory parent")?,
        );
        match mkdirat(&parent, name, Mode::RWXU) {
            Ok(()) => parent
                .sync_all()
                .context("could not sync new device directory")?,
            Err(Errno::EXIST) => {}
            Err(error) => return Err(error).context("could not create private device directory"),
        }
        let directory = File::from(
            openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("could not open private device directory")?,
        );
        Self::verify(directory)
    }

    pub fn for_file(path: &Path) -> Result<(Self, OsString)> {
        let (parent, name) = split_path(path)?;
        Ok((Self::open(parent)?, name.to_owned()))
    }

    fn verify(file: File) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == geteuid().as_raw() && metadata.mode() & 0o777 == 0o700,
            "Iroh directory must be owned by the current user with mode 0700 (observed {:04o})",
            metadata.mode() & 0o777
        );
        Ok(Self(file))
    }

    pub fn open_file(&self, name: &OsStr) -> Result<Option<File>> {
        let file = match openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error).context("could not read private Iroh file"),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o600,
            "Iroh file must be regular, owned by the current user with mode 0600 (observed {:04o})",
            metadata.mode() & 0o777
        );
        Ok(Some(file))
    }

    /// False means the destination already exists; nothing is overwritten.
    pub fn create_new(&self, name: &OsStr, contents: &[u8]) -> Result<bool> {
        let temporary = format!(
            ".weld-iroh-{}-{}",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        );
        let mut file = File::from(
            openat(
                &self.0,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .context("could not create private Iroh file")?,
        );
        let result = (|| -> Result<bool> {
            let mode = file.metadata()?.mode() & 0o777;
            ensure!(
                mode == 0o600,
                "new Iroh file must have mode 0600; check umask (observed {mode:04o})"
            );
            file.write_all(contents)?;
            file.sync_all()?;
            match linkat(&self.0, &temporary, &self.0, name, AtFlags::empty()) {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(error) => Err(error).context("could not publish private Iroh file"),
            }
        })();
        let cleanup = unlinkat(&self.0, &temporary, AtFlags::empty());
        // Persist both the published link and removal of temporary key material.
        let synced = self.0.sync_all();
        let created = result?;
        cleanup.context("could not remove temporary Iroh file")?;
        synced.context("could not sync private Iroh directory")?;
        Ok(created)
    }
}

fn split_path(path: &Path) -> Result<(&Path, &OsStr)> {
    let name = path.file_name().context("Iroh path needs a file name")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok((parent, name))
}
