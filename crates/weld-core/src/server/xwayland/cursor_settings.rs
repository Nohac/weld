//! Cursor resources consumed by X11 clients using libXcursor directly.

use anyhow::{Context, Result, ensure};
use smithay::reexports::x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt, PropMode},
    wrapper::ConnectionExt as _,
};

use crate::cursor::CursorConfiguration;

pub(super) fn publish(display: u32, configuration: &CursorConfiguration) -> Result<()> {
    ensure!(
        !configuration.theme().contains(['\n', '\r', '\0', '\\']),
        "cursor theme contains X resource control characters"
    );
    let (connection, screen) = smithay::reexports::x11rb::connect(Some(&format!(":{display}")))
        .context("could not connect to the session XWayland resource database")?;
    let root = connection.setup().roots[screen].root;
    let property = connection
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            262_144,
        )?
        .reply()?;
    ensure!(
        property.bytes_after == 0,
        "X resource database exceeds 1 MiB"
    );
    let previous = std::str::from_utf8(&property.value).context("X resources are not UTF-8")?;
    let resources = cursor_resources(previous, configuration);
    connection
        .change_property8(
            PropMode::REPLACE,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            resources.as_bytes(),
        )?
        .check()?;
    Ok(())
}

fn cursor_resources(previous: &str, configuration: &CursorConfiguration) -> String {
    let mut resources = String::with_capacity(previous.len() + configuration.theme().len() + 40);
    for line in previous.lines() {
        let owned = line
            .split_once(':')
            .is_some_and(|(name, _)| matches!(name.trim(), "Xcursor.theme" | "Xcursor.size"));
        if !owned {
            resources.push_str(line);
            resources.push('\n');
        }
    }
    resources.push_str(&format!(
        "Xcursor.theme: {}\nXcursor.size: {}\n",
        configuration.theme(),
        configuration.size()
    ));
    resources
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_replaces_cursor_resources_and_preserves_other_settings() -> Result<()> {
        let original = "Xft.dpi: 144\nXcursor.theme: Old\n Xcursor.size : 24\n! comment\n";
        let configuration = CursorConfiguration::new("New", 36)?;
        let updated = cursor_resources(original, &configuration);
        assert_eq!(
            updated,
            "Xft.dpi: 144\n! comment\nXcursor.theme: New\nXcursor.size: 36\n"
        );
        assert_eq!(cursor_resources(&updated, &configuration), updated);
        Ok(())
    }
}
