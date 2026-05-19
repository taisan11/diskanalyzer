use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use nojson::{DisplayJson, JsonFormatter, JsonParseError, RawJsonValue};

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub root: Node,
    pub scanned_at_unix: u64,
    pub scan_duration_ms: u64,
    pub warnings: Vec<String>,
}

impl ScanResult {
    pub fn new(root: Node, warnings: Vec<String>, scan_duration_ms: u64) -> Self {
        let scanned_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            root,
            scanned_at_unix,
            scan_duration_ms,
            warnings,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub kind: NodeKind,
    pub size: u64,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child_count(&self) -> usize {
        self.children.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
}

impl DisplayJson for ScanResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> fmt::Result {
        f.object(|f| {
            f.member("root", &self.root)?;
            f.member("scanned_at_unix", self.scanned_at_unix)?;
            f.member("scan_duration_ms", self.scan_duration_ms)?;
            f.member("warnings", &self.warnings)
        })
    }
}

impl DisplayJson for Node {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> fmt::Result {
        f.object(|f| {
            f.member("name", &self.name)?;
            f.member("path", &self.path)?;
            f.member("kind", self.kind)?;
            f.member("size", self.size)?;
            f.member("children", &self.children)
        })
    }
}

impl DisplayJson for NodeKind {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> fmt::Result {
        let kind = match self {
            NodeKind::Directory => "directory",
            NodeKind::File => "file",
            NodeKind::Symlink => "symlink",
        };
        f.value(kind)
    }
}

impl<'text, 'raw> TryFrom<RawJsonValue<'text, 'raw>> for ScanResult {
    type Error = JsonParseError;

    fn try_from(value: RawJsonValue<'text, 'raw>) -> Result<Self, Self::Error> {
        let root: Node = value.to_member("root")?.required()?.try_into()?;
        let scanned_at_unix: u64 = value.to_member("scanned_at_unix")?.required()?.try_into()?;
        let scan_duration_ms: u64 = value
            .to_member("scan_duration_ms")?
            .map(|v| v.try_into())?
            .unwrap_or(0);
        let warnings: Vec<String> = value.to_member("warnings")?.required()?.try_into()?;

        Ok(Self {
            root,
            scanned_at_unix,
            scan_duration_ms,
            warnings,
        })
    }
}

impl<'text, 'raw> TryFrom<RawJsonValue<'text, 'raw>> for Node {
    type Error = JsonParseError;

    fn try_from(value: RawJsonValue<'text, 'raw>) -> Result<Self, Self::Error> {
        let name: String = value.to_member("name")?.required()?.try_into()?;
        let path: String = value.to_member("path")?.required()?.try_into()?;
        let kind: NodeKind = value.to_member("kind")?.required()?.try_into()?;
        let size: u64 = value.to_member("size")?.required()?.try_into()?;
        let children: Vec<Node> = value.to_member("children")?.required()?.try_into()?;

        Ok(Self {
            name,
            path,
            kind,
            size,
            children,
        })
    }
}

impl<'text, 'raw> TryFrom<RawJsonValue<'text, 'raw>> for NodeKind {
    type Error = JsonParseError;

    fn try_from(value: RawJsonValue<'text, 'raw>) -> Result<Self, Self::Error> {
        let kind: String = value.try_into()?;
        match kind.as_str() {
            "directory" => Ok(NodeKind::Directory),
            "file" => Ok(NodeKind::File),
            "symlink" => Ok(NodeKind::Symlink),
            _ => Err(value.invalid(format!(
                "unknown kind: {kind}. expected directory|file|symlink"
            ))),
        }
    }
}

pub fn format_bytes(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = size as f64;
    let mut unit_idx = 0usize;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{size} {}", UNITS[unit_idx])
    } else {
        format!("{value:.2} {}", UNITS[unit_idx])
    }
}
