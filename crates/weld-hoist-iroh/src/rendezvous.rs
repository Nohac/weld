//! One-shot public identity exchange through a trusted local user's private directory.
//!
//! Directory-relative operations pin the checked directory across filesystem renames.
//! This is local launch authorization, not a device credential or protection from the
//! same OS user. Stale publications are never replaced: each launch needs fresh paths.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, Mode, OFlags, linkat, open, openat, unlinkat},
    io::Errno,
    process::geteuid,
};

const MAX_PUBLICATION_BYTES: usize = 4096;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn private_parent(path: &Path) -> Result<(File, &OsStr)> {
    let name = path
        .file_name()
        .context("Iroh publication needs a file name")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = File::from(
        open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("could not open private Iroh directory {}", parent.display()))?,
    );
    let metadata = directory.metadata()?;
    ensure!(
        metadata.uid() == geteuid().as_raw() && metadata.mode() & 0o777 == 0o700,
        "Iroh exchange directory must be owned by the current user with mode 0700: {}",
        parent.display()
    );
    Ok((directory, name))
}

pub(crate) fn publish(path: &Path, value: &str) -> Result<()> {
    ensure!(
        value.len() < MAX_PUBLICATION_BYTES,
        "Iroh publication is too large"
    );
    let (directory, name) = private_parent(path)?;
    let temporary = format!(
        ".weld-iroh-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    );
    let mut file = File::from(
        openat(
            &directory,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .context("could not create private Iroh publication")?,
    );
    let result: Result<()> = (|| {
        writeln!(file, "{value}")?;
        file.sync_all()?;
        linkat(&directory, &temporary, &directory, name, AtFlags::empty()).with_context(|| {
            format!(
                "could not publish {}; use fresh paths (existing publications are not overwritten)",
                path.display()
            )
        })?;
        Ok(())
    })();
    let cleanup = unlinkat(&directory, &temporary, AtFlags::empty());
    result?;
    cleanup.context("could not remove temporary Iroh publication")
}

/// A checked, pinned directory and publication name shared by blocking and
/// nonblocking admission. Only an absent file is retryable.
pub(crate) struct PublicationReader {
    directory: File,
    name: OsString,
    path: PathBuf,
}

impl PublicationReader {
    pub fn new(path: &Path) -> Result<Self> {
        let (directory, name) = private_parent(path)?;
        Ok(Self {
            directory,
            name: name.to_owned(),
            path: path.to_owned(),
        })
    }
    pub fn try_read(&self) -> Result<Option<String>> {
        let file = match openat(
            &self.directory,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not read Iroh publication {}", self.path.display())
                });
            }
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o600,
            "Iroh publication must be a regular file owned by the current user with mode 0600: {}",
            self.path.display()
        );
        let mut value = String::new();
        file.take(u64::try_from(MAX_PUBLICATION_BYTES + 1)?)
            .read_to_string(&mut value)
            .context("could not read UTF-8 Iroh publication")?;
        ensure!(
            value.len() <= MAX_PUBLICATION_BYTES,
            "Iroh publication exceeds 4096 bytes"
        );
        ensure!(!value.trim().is_empty(), "Iroh publication is empty");
        Ok(Some(value))
    }
}

/// Wait only for an absent file. Invalid permissions, type, or contents fail immediately.
pub(crate) fn read(path: &Path, deadline: Instant) -> Result<String> {
    let reader = PublicationReader::new(path)?;
    loop {
        if let Some(value) = reader.try_read()? {
            return Ok(value);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "timed out waiting for Iroh publication {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20).min(remaining));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
        path::PathBuf,
    };

    use super::*;

    pub(crate) struct ExchangeDirectory(pub PathBuf);

    impl ExchangeDirectory {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "weld-iroh-exchange-{}-{}",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private test directory");
            Self(path)
        }
    }

    impl Drop for ExchangeDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publication_is_private_atomic_and_never_overwrites() {
        let directory = ExchangeDirectory::new();
        let path = directory.0.join("identity");
        publish(&path, "first").expect("publish");
        assert_eq!(read(&path, Instant::now()).expect("read"), "first\n");
        assert!(publish(&path, "second").is_err());
        assert_eq!(fs::read_to_string(&path).expect("unchanged"), "first\n");
        assert_eq!(fs::read_dir(&directory.0).expect("entries").count(), 1);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("change permissions");
        assert!(read(&path, Instant::now()).is_err());
    }

    #[test]
    fn rejects_unsafe_exchange_files_and_directories() {
        let directory = ExchangeDirectory::new();
        let missing = directory.0.join("missing");
        assert!(read(&missing, Instant::now()).is_err());
        let target = directory.0.join("target");
        publish(&target, "target").expect("publish target");
        symlink(&target, &missing).expect("symlink");
        assert!(read(&missing, Instant::now()).is_err());
        assert!(publish(&missing, "replace").is_err());
        let fifo = directory.0.join("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).expect("FIFO");
        assert!(read(&fifo, Instant::now()).is_err());
        let oversized = directory.0.join("oversized");
        publish(&oversized, "initial").expect("publish");
        fs::write(&oversized, vec![b'x'; 4097]).expect("oversized input");
        assert!(read(&oversized, Instant::now()).is_err());
        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755))
            .expect("public directory");
        assert!(read(&target, Instant::now()).is_err());
        assert!(publish(&directory.0.join("another"), "x").is_err());
    }

    #[test]
    fn waits_for_atomic_publication_but_not_invalid_content() {
        let directory = ExchangeDirectory::new();
        let path = directory.0.join("identity");
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            publish(&writer_path, "ready").expect("delayed publication");
        });
        assert_eq!(
            read(&path, Instant::now() + Duration::from_secs(1)).expect("delayed read"),
            "ready\n"
        );
        writer.join().expect("writer");
        let empty = directory.0.join("empty");
        publish(&empty, "").expect("empty file");
        assert!(read(&empty, Instant::now()).is_err());
    }
}
