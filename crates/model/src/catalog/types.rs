use serde::{
    Deserialize,
    Serialize,
};
use value::codegen_convex_serialization;

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum CatalogEntry {
    StagedVersion {
        version: String,
        manifest_digest: String,
        expected_active_version: Option<String>,
    },
    ActiveVersion {
        version: String,
        manifest_digest: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedCatalogEntry {
    kind: String,
    version: String,
    manifest_digest: String,
    expected_active_version: Option<String>,
}

impl From<CatalogEntry> for SerializedCatalogEntry {
    fn from(entry: CatalogEntry) -> Self {
        match entry {
            CatalogEntry::StagedVersion {
                version,
                manifest_digest,
                expected_active_version,
            } => Self {
                kind: "stagedVersion".to_owned(),
                version,
                manifest_digest,
                expected_active_version,
            },
            CatalogEntry::ActiveVersion {
                version,
                manifest_digest,
            } => Self {
                kind: "activeVersion".to_owned(),
                version,
                manifest_digest,
                expected_active_version: None,
            },
        }
    }
}

impl TryFrom<SerializedCatalogEntry> for CatalogEntry {
    type Error = anyhow::Error;

    fn try_from(entry: SerializedCatalogEntry) -> anyhow::Result<Self> {
        match entry.kind.as_str() {
            "stagedVersion" => Ok(Self::StagedVersion {
                version: entry.version,
                manifest_digest: entry.manifest_digest,
                expected_active_version: entry.expected_active_version,
            }),
            "activeVersion" => Ok(Self::ActiveVersion {
                version: entry.version,
                manifest_digest: entry.manifest_digest,
            }),
            kind => anyhow::bail!("Unknown catalog entry kind {kind:?}"),
        }
    }
}

codegen_convex_serialization!(CatalogEntry, SerializedCatalogEntry);
