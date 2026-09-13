//! Same-user private storage, not attestation or protection from that OS user.
//! Directory fds pin checked parents; writes publish complete files without overwrite.

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, Mode, OFlags, RenameFlags, mkdirat, open, openat, renameat_with, unlinkat},
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
            // Android app SELinux policy permits rename in private storage but
            // denies hard links. NOREPLACE preserves atomic first-writer wins.
            match renameat_with(&self.0, &temporary, &self.0, name, RenameFlags::NOREPLACE) {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(error @ (Errno::INVAL | Errno::NOSYS)) => {
                    Err(error).context("filesystem does not support atomic no-replace rename")
                }
                Err(error) => Err(error).context("could not publish private Iroh file"),
            }
        })();
        // A successful rename consumed our temporary name. Never unlink a new
        // file that could subsequently occupy it.
        let cleanup = if matches!(&result, Ok(true)) {
            Ok(())
        } else {
            unlinkat(&self.0, &temporary, AtFlags::empty())
        };
        // Persist publication or removal of losing temporary key material.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rendezvous::tests::ExchangeDirectory;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn atomic_publication_consumes_only_its_own_temporary_and_never_replaces() {
        let directory = ExchangeDirectory::new();
        let storage = PrivateDirectory::open(&directory.0).expect("private directory");
        let name = OsStr::new("record");
        assert!(storage.create_new(name, b"first").expect("publish"));
        assert!(!storage.create_new(name, b"second").expect("existing"));
        assert_eq!(fs::read(directory.0.join(name)).expect("winner"), b"first");
        assert_eq!(fs::read_dir(&directory.0).expect("entries").count(), 1);
        symlink("record", directory.0.join("alias")).expect("symlink");
        assert!(
            !storage
                .create_new(OsStr::new("alias"), b"third")
                .expect("existing symlink")
        );
        assert_eq!(
            fs::read(directory.0.join(name)).expect("unchanged"),
            b"first"
        );
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("no orphan temporaries")
                .count(),
            2
        );
    }
}
