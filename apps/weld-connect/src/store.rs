//! Saved host profiles keyed by authenticated peer identity.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use weld_hoist_iroh::{IrohConnectionProfile, IrohDeviceIdentity};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    profile: String,
}
impl Host {
    pub fn profile(&self) -> Result<IrohConnectionProfile> {
        self.profile.parse()
    }
    pub fn id(&self) -> Result<String> {
        Ok(self.profile()?.peer().as_str().to_owned())
    }
}

pub struct Store {
    directory: PathBuf,
    pub identity: IrohDeviceIdentity,
    pub hosts: Vec<Host>,
    _lock: File,
}
impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let identity = IrohDeviceIdentity::load_or_create(directory)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("client.lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("could not lock Weld Connect state directory")?;
        let hosts: Vec<Host> = match File::open(directory.join("hosts.json")) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(131_073).read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= 131_072, "host store exceeds size limit");
                serde_json::from_slice(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        ensure!(hosts.len() <= 64, "host store exceeds device limit");
        let mut ids = std::collections::BTreeSet::new();
        for host in &hosts {
            validate_name(&host.name)?;
            ensure!(ids.insert(host.id()?), "duplicate saved host identity");
        }
        Ok(Self {
            directory: directory.into(),
            identity,
            hosts,
            _lock: lock,
        })
    }
    pub fn remember(&mut self, name: String, profile: IrohConnectionProfile) -> Result<()> {
        validate_name(&name)?;
        let id = profile.peer().as_str();
        let mut hosts = self.hosts.clone();
        let entry = Host {
            name,
            profile: profile.to_string(),
        };
        if let Some(existing) = hosts
            .iter_mut()
            .find(|host| host.id().is_ok_and(|value| value == id))
        {
            *existing = entry;
        } else {
            ensure!(hosts.len() < 64, "saved host limit reached");
            hosts.push(entry);
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&serde_json::to_vec(&hosts)?)?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("hosts.json"))?;
        File::open(&self.directory)?.sync_all()?;
        self.hosts = hosts;
        Ok(())
    }
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
        "invalid host name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_hoist_iroh::IrohNetwork;
    #[test]
    fn pairing_retains_other_hosts_and_repair_updates_the_same_identity() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut store = Store::open(&temporary.path().join("client"))?;
        let first = IrohDeviceIdentity::load_or_create(temporary.path().join("first"))?;
        let second = IrohDeviceIdentity::load_or_create(temporary.path().join("second"))?;
        let profile = |identity: &IrohDeviceIdentity| {
            IrohConnectionProfile::new(identity.public_id(), IrohNetwork::N0, vec![])
        };
        store.remember("Laptop".into(), profile(&first)?)?;
        store.remember("Desktop".into(), profile(&second)?)?;
        store.remember("Renamed laptop".into(), profile(&first)?)?;
        assert_eq!(store.hosts.len(), 2);
        let saved = store.hosts.clone();
        assert!(Store::open(&temporary.path().join("client")).is_err());
        drop(store);
        assert_eq!(Store::open(&temporary.path().join("client"))?.hosts, saved);
        Ok(())
    }
}
