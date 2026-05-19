use std::{io, path::Path};

use nojson::Json;

use super::ScanResult;

pub async fn load_result(path: &Path) -> io::Result<ScanResult> {
    let text = tokio::fs::read_to_string(path).await?;
    parse_result(&text)
}

pub fn save_result_sync(result: &ScanResult, path: &Path) -> io::Result<()> {
    std::fs::write(path, Json(result).to_string())
}

pub fn load_result_sync(path: &Path) -> io::Result<ScanResult> {
    let text = std::fs::read_to_string(path)?;
    parse_result(&text)
}

fn parse_result(text: &str) -> io::Result<ScanResult> {
    let parsed = text
        .parse::<Json<ScanResult>>()
        .map_err(|error: nojson::JsonParseError| {
            io::Error::new(io::ErrorKind::InvalidData, error.to_string())
        })?;
    Ok(parsed.0)
}
