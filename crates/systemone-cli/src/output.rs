use std::io::Write;

use serde::Serialize;

#[derive(Serialize)]
pub struct HelpOutput {
    pub schema: &'static str,
    pub command: &'static str,
    pub usage: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct VersionOutput {
    pub schema: &'static str,
    pub version: &'static str,
    pub build: &'static str,
    pub openjev: &'static str,
}

pub fn write_json<W: Write + ?Sized, T: Serialize>(
    writer: &mut W,
    value: &T,
    pretty: bool,
) -> std::io::Result<()> {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .map_err(std::io::Error::other)?;
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}
