use super::{MAX_DEVICES, PairedDevice, validate_name};
use crate::{IrohPeerIdentity, private_file::PrivateDirectory};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, ffi::OsStr, fs::File, io::Read, path::Path};

pub(super) struct TrustStore {
    directory: PrivateDirectory,
    _lock: File,
}

impl TrustStore {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = PrivateDirectory::open_or_create(path)?;
        let lock = directory.lock_exclusive(OsStr::new("devices.lock"))?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }
    pub fn load(&self) -> Result<BTreeMap<String, PairedDevice>> {
        let Some(file) = self.directory.open_file(OsStr::new("devices.json"))? else {
            return Ok(BTreeMap::new());
        };
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "device database too large");
        let devices: Vec<PairedDevice> = serde_json::from_slice(&bytes)?;
        ensure!(
            devices.len() <= MAX_DEVICES,
            "device database exceeds limit"
        );
        let mut records = BTreeMap::new();
        for device in devices {
            let identity: IrohPeerIdentity = device.identity.parse()?;
            ensure!(
                identity.as_str() == device.identity,
                "device identity is not canonical"
            );
            validate_name(&device.name)?;
            ensure!(
                records.insert(device.identity.clone(), device).is_none(),
                "duplicate paired device"
            );
        }
        Ok(records)
    }
    pub fn save(&self, records: &BTreeMap<String, PairedDevice>) -> Result<()> {
        self.directory.replace(
            OsStr::new("devices.json"),
            &serde_json::to_vec(&records.values().collect::<Vec<_>>())?,
        )
    }
}
