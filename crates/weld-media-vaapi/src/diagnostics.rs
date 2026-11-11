//! File output for explicit media diagnostics; the VA utility only returns pixels.
use crate::{VaapiDmabuf, VppConverter};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

pub(crate) fn write_xrgb_ppm(
    vpp: &VppConverter,
    input: &VaapiDmabuf,
    width: u32,
    height: u32,
    path: &Path,
) -> Result<()> {
    let pixels = vpp.read_bgra(input, width, height)?;
    let file = File::create(path)
        .with_context(|| format!("creating diagnostic frame {}", path.display()))?;
    let mut output = BufWriter::new(file);
    write!(output, "P6\n{width} {height}\n255\n")?;
    for bgra in pixels.as_chunks::<4>().0 {
        output.write_all(&[bgra[2], bgra[1], bgra[0]])?;
    }
    output.flush()?;
    Ok(())
}
