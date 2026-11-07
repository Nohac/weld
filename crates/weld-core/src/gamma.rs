//! Bounded output gamma tables shared by the Wayland and physical-output boundaries.
use anyhow::{Result, ensure};
use std::{fs::File, os::unix::fs::FileExt};

/// A resource bound, not an advertised synthetic table size. Larger hardware
/// tables are unsupported rather than resized behind the client's back.
pub(crate) const MAX_GAMMA_ENTRIES: u32 = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GammaRamp {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}
impl GammaRamp {
    /// The protocol uses three consecutive native-endian arrays of u16 values.
    /// Only regular files of exactly the advertised size are accepted. Positional
    /// reads neither depend on nor modify the sender's shared file offset; pipes
    /// and devices are rejected before reading so they cannot stall dispatch.
    pub fn read(file: &File, size: u32) -> Result<Self> {
        ensure!(
            (1..=MAX_GAMMA_ENTRIES).contains(&size),
            "unsupported gamma table size"
        );
        let metadata = file.metadata()?;
        let length = u64::from(size) * 3 * 2;
        ensure!(
            metadata.is_file() && metadata.len() == length,
            "gamma file has an invalid type or size"
        );
        let mut bytes = vec![0; length as usize];
        file.read_exact_at(&mut bytes, 0)?;
        let mut values = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_ne_bytes(*bytes));
        Ok(Self {
            red: values.by_ref().take(size as usize).collect(),
            green: values.by_ref().take(size as usize).collect(),
            blue: values.collect(),
        })
    }

    pub fn len(&self) -> usize {
        self.red.len()
    }

    /// DRM's gamma blob has one native-endian red/green/blue/reserved u16 tuple
    /// per entry. Encoding bytes explicitly avoids depending on Rust padding.
    pub fn drm_bytes(&self) -> Vec<u8> {
        self.red
            .iter()
            .zip(&self.green)
            .zip(&self.blue)
            .flat_map(|((red, green), blue)| {
                [*red, *green, *blue, 0]
                    .into_iter()
                    .flat_map(u16::to_ne_bytes)
            })
            .collect()
    }
}
