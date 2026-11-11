//! Distribution defaults and overrides; no transport or codec policy lives here.

use std::ffi::OsString;

use anyhow::{Context, Result, ensure};
use weld_hoist_encoded::SharedBitrateBudget;

use crate::{
    AppArguments,
    arguments::{HoistCodec, HoistSurfaceMode},
};

const ENVIRONMENT: &str = "WELD_HOIST_BITRATE_TARGET_MBPS";

pub(super) fn for_source(arguments: &AppArguments) -> Result<Option<SharedBitrateBudget>> {
    let Some((target, origin)) = resolve(arguments, || std::env::var_os(ENVIRONMENT))? else {
        return Ok(None);
    };
    tracing::info!(
        bits_per_second = target,
        origin,
        "shared encoder bitrate target, not a bandwidth cap"
    );
    SharedBitrateBudget::new(target).map(Some)
}

fn resolve(
    arguments: &AppArguments,
    environment: impl FnOnce() -> Option<OsString>,
) -> Result<Option<(u64, &'static str)>> {
    let encoded_source = arguments.hoist_iroh_listen.is_some()
        || (arguments.hoist_listen.is_some()
            && arguments.hoist_surface_mode == Some(HoistSurfaceMode::EncodedOpaque));
    if !encoded_source {
        ensure!(
            arguments.hoist_bitrate_target_mbps.is_none(),
            "bitrate target requires an encoded hoist source"
        );
        return Ok(None);
    }
    let (megabits, origin) = if let Some(value) = arguments.hoist_bitrate_target_mbps {
        (value, "flag")
    } else if let Some(value) = environment() {
        let value = value
            .to_str()
            .context("invalid WELD_HOIST_BITRATE_TARGET_MBPS encoding")?;
        (
            value
                .parse::<u64>()
                .context("invalid WELD_HOIST_BITRATE_TARGET_MBPS")?,
            "environment",
        )
    } else {
        (
            match arguments.hoist_codec.unwrap_or_default() {
                HoistCodec::Av1 => 8,
                HoistCodec::H264 => 16,
            },
            "default",
        )
    };
    ensure!(megabits > 0, "shared bitrate target must be positive");
    Ok(Some((
        megabits
            .checked_mul(1_000_000)
            .context("shared bitrate target overflow")?,
        origin,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn source(codec: &str, extra: &[&str]) -> AppArguments {
        AppArguments::try_parse_from(
            [
                "weldwm",
                "--hoist-listen",
                "/tmp/weld.sock",
                "--hoist-surface-mode",
                "encoded-opaque",
                "--hoist-codec",
                codec,
            ]
            .into_iter()
            .chain(extra.iter().copied()),
        )
        .expect("arguments")
    }

    #[test]
    fn precedence_and_default_origins_are_resolved_before_gpu_startup() {
        assert_eq!(
            resolve(&source("av1", &[]), || None).expect("default"),
            Some((8_000_000, "default"))
        );
        assert_eq!(
            resolve(&source("h264", &[]), || None).expect("default"),
            Some((16_000_000, "default"))
        );
        assert_eq!(
            resolve(&source("av1", &[]), || Some("4".into())).expect("env"),
            Some((4_000_000, "environment"))
        );
        assert_eq!(
            resolve(
                &source("av1", &["--hoist-bitrate-target-mbps", "6"]),
                || panic!("flag must bypass environment")
            )
            .expect("flag"),
            Some((6_000_000, "flag"))
        );
    }

    #[test]
    fn irrelevant_environment_is_not_read_and_invalid_source_options_fail_early() {
        for args in [
            vec!["weldwm"],
            vec!["weldwm", "--hoist-connect", "/tmp/weld.sock"],
            vec!["weldwm", "--hoist-listen", "/tmp/weld.sock"],
        ] {
            let mut arguments = AppArguments::try_parse_from(args).expect("args");
            assert!(
                resolve(&arguments, || panic!("irrelevant environment"))
                    .expect("ignored")
                    .is_none()
            );
            arguments.hoist_bitrate_target_mbps = Some(8);
            assert!(crate::validate_hoist_arguments(&arguments).is_err());
        }
        for invalid in ["0", "oops", "18446744073709551615"] {
            assert!(resolve(&source("av1", &[]), || Some(invalid.into())).is_err());
        }
    }
}
