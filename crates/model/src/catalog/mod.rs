use std::{
    fmt,
    str::FromStr,
    sync::LazyLock,
};

use common::{
    document::{
        ParseDocument,
        ParsedDocument,
    },
    runtime::Runtime,
};
use database::{
    SystemMetadataModel,
    Transaction,
};
use value::{
    sha256::{
        Sha256,
        Sha256Digest,
    },
    InternalId,
    PublicDocumentId,
    ResolvedDocumentId,
    TableName,
    TableNamespace,
};

use crate::{
    catalog::types::CatalogEntry,
    SystemIndex,
    SystemTable,
};

pub mod types;

pub static CATALOG_VERSIONS_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_catalog_versions"
        .parse()
        .expect("Invalid built-in catalog versions table")
});

const ACTIVE_CATALOG_INTERNAL_ID: InternalId = InternalId(*b"catalog_active!!");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogVersion(String);

impl CatalogVersion {
    fn from_internal_id(id: InternalId) -> Self {
        Self(String::from(id))
    }

    fn internal_id(&self) -> anyhow::Result<InternalId> {
        InternalId::from_str(&self.0)
    }
}

impl fmt::Display for CatalogVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for CatalogVersion {
    type Err = anyhow::Error;

    fn from_str(version: &str) -> anyhow::Result<Self> {
        InternalId::from_str(version)?;
        Ok(Self(version.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogManifestDigest(String);

impl CatalogManifestDigest {
    pub fn hash(bytes: &[u8]) -> Self {
        Self(Sha256::hash(bytes).as_base64())
    }

    fn as_sha256(&self) -> anyhow::Result<Sha256Digest> {
        Sha256Digest::from_base64(&self.0)
    }
}

impl fmt::Display for CatalogManifestDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for CatalogManifestDigest {
    type Err = anyhow::Error;

    fn from_str(digest: &str) -> anyhow::Result<Self> {
        let parsed = Self(digest.to_owned());
        let _ = parsed.as_sha256()?;
        Ok(parsed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogActivation {
    version: CatalogVersion,
    manifest_digest: CatalogManifestDigest,
}

impl CatalogActivation {
    pub fn new(version: CatalogVersion, manifest_digest: CatalogManifestDigest) -> Self {
        Self {
            version,
            manifest_digest,
        }
    }

    pub fn version(&self) -> &CatalogVersion {
        &self.version
    }

    pub fn manifest_digest(&self) -> &CatalogManifestDigest {
        &self.manifest_digest
    }
}

pub struct CatalogVersionsTable;

impl SystemTable for CatalogVersionsTable {
    type Metadata = CatalogEntry;

    fn table_name() -> &'static TableName {
        &CATALOG_VERSIONS_TABLE
    }

    fn indexes() -> Vec<SystemIndex<Self>> {
        vec![]
    }
}

pub struct CatalogModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
}

impl<'a, RT: Runtime> CatalogModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    fn resolved_id(&mut self, internal_id: InternalId) -> anyhow::Result<ResolvedDocumentId> {
        let table_id = self
            .tx
            .table_mapping()
            .namespace(TableNamespace::Global)
            .id(&CATALOG_VERSIONS_TABLE)?;
        Ok(ResolvedDocumentId::new(
            table_id.tablet_id,
            PublicDocumentId::new(table_id.table_number, internal_id),
        ))
    }

    async fn get_entry(
        &mut self,
        internal_id: InternalId,
    ) -> anyhow::Result<Option<ParsedDocument<CatalogEntry>>> {
        let id = self.resolved_id(internal_id)?;
        SystemMetadataModel::new_global(self.tx)
            .get(id)
            .await?
            .map(ParseDocument::<CatalogEntry>::parse)
            .transpose()
    }

    /// Persist an immutable catalog generation before any request may select
    /// it.
    pub async fn stage_version(
        &mut self,
        manifest_digest: CatalogManifestDigest,
    ) -> anyhow::Result<CatalogActivation> {
        let expected_active_version = self
            .active_catalog()
            .await?
            .map(|active| active.version.to_string());
        let internal_id = SystemMetadataModel::new_global(self.tx).allocate_internal_id()?;
        anyhow::ensure!(internal_id != ACTIVE_CATALOG_INTERNAL_ID);
        let version = CatalogVersion::from_internal_id(internal_id);
        SystemMetadataModel::new_global(self.tx)
            .insert_with_internal_id(
                &CATALOG_VERSIONS_TABLE,
                internal_id,
                CatalogEntry::StagedVersion {
                    version: version.to_string(),
                    manifest_digest: manifest_digest.to_string(),
                    expected_active_version,
                }
                .try_into()?,
            )
            .await?;
        Ok(CatalogActivation::new(version, manifest_digest))
    }

    pub async fn active_catalog(&mut self) -> anyhow::Result<Option<CatalogActivation>> {
        let Some(entry) = self.get_entry(ACTIVE_CATALOG_INTERNAL_ID).await? else {
            return Ok(None);
        };
        match entry.into_value() {
            CatalogEntry::ActiveVersion {
                version,
                manifest_digest,
            } => Ok(Some(CatalogActivation::new(
                version.parse()?,
                manifest_digest.parse()?,
            ))),
            CatalogEntry::StagedVersion { .. } => {
                anyhow::bail!("The fixed catalog activation row contains a staged version")
            },
        }
    }

    /// Select an already-staged immutable generation as active.
    ///
    /// Call this in the same transaction that publishes the generation's
    /// modules, schema, and index metadata. The selector is the only
    /// mutable catalog record.
    ///
    /// This selector proves atomic metadata activation on the authoritative
    /// persistence path. It is not an all-node readiness certificate: local
    /// index backfill and retained-generation execution require later #286
    /// slices before requests may use it as a routing or execution fence.
    pub async fn activate(
        &mut self,
        activation: &CatalogActivation,
        actual_manifest_digest: &CatalogManifestDigest,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            activation.manifest_digest() == actual_manifest_digest,
            "Catalog activation manifest does not match the finish_push payload"
        );
        let staged = self
            .get_entry(activation.version.internal_id()?)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("Catalog version {} was not staged", activation.version)
            })?;
        let CatalogEntry::StagedVersion {
            version,
            manifest_digest,
            expected_active_version,
        } = staged.into_value()
        else {
            anyhow::bail!(
                "Catalog version {} does not refer to an immutable staged record",
                activation.version
            );
        };
        anyhow::ensure!(
            version == activation.version.to_string()
                && manifest_digest == actual_manifest_digest.to_string(),
            "Catalog version {} does not match its staged manifest",
            activation.version
        );

        let current_active_version = self
            .active_catalog()
            .await?
            .map(|active| active.version.to_string());
        anyhow::ensure!(
            current_active_version == expected_active_version,
            "Catalog activation lost compare-and-swap: expected active version {:?}, found {:?}",
            expected_active_version,
            current_active_version
        );

        let active_value: value::DocumentObject = CatalogEntry::ActiveVersion {
            version,
            manifest_digest,
        }
        .try_into()?;
        if self.get_entry(ACTIVE_CATALOG_INTERNAL_ID).await?.is_some() {
            let active_id = self.resolved_id(ACTIVE_CATALOG_INTERNAL_ID)?;
            SystemMetadataModel::new_global(self.tx)
                .replace(active_id, active_value)
                .await?;
        } else {
            SystemMetadataModel::new_global(self.tx)
                .insert_with_internal_id(
                    &CATALOG_VERSIONS_TABLE,
                    ACTIVE_CATALOG_INTERNAL_ID,
                    active_value,
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use database::test_helpers::DbFixtures;
    use runtime::testing::TestRuntime;

    use crate::{
        catalog::{
            CatalogManifestDigest,
            CatalogModel,
        },
        test_helpers::DbFixturesWithModel,
        udf_config::{
            types::UdfConfig,
            UdfConfigModel,
        },
    };

    #[convex_macro::test_runtime]
    async fn catalog_activation_is_atomic(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let manifest_one = CatalogManifestDigest::hash(b"catalog one");
        let manifest_two = CatalogManifestDigest::hash(b"catalog two");

        let mut tx = db.begin_system().await?;
        let version_one = CatalogModel::new(&mut tx)
            .stage_version(manifest_one.clone())
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
            .set(UdfConfig::new_for_test(&rt, "1.0.0".parse()?))
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&version_one, &manifest_one)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let version_two = CatalogModel::new(&mut tx)
            .stage_version(manifest_two.clone())
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        assert_eq!(
            CatalogModel::new(&mut tx).active_catalog().await?,
            Some(version_one.clone())
        );
        assert_eq!(
            UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
                .get()
                .await?
                .expect("version one UDF config")
                .server_version,
            semver::Version::new(1, 0, 0)
        );

        let mut tx = db.begin_system().await?;
        UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
            .set(UdfConfig::new_for_test(&rt, "2.0.0".parse()?))
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&version_two, &manifest_two)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        assert_eq!(
            CatalogModel::new(&mut tx).active_catalog().await?,
            Some(version_two)
        );
        assert_eq!(
            UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
                .get()
                .await?
                .expect("version two UDF config")
                .server_version,
            semver::Version::new(2, 0, 0)
        );
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn failed_catalog_rollout_preserves_previous_activation(
        rt: TestRuntime,
    ) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let manifest_one = CatalogManifestDigest::hash(b"catalog one");
        let manifest_two = CatalogManifestDigest::hash(b"catalog two");

        let mut tx = db.begin_system().await?;
        let version_one = CatalogModel::new(&mut tx)
            .stage_version(manifest_one.clone())
            .await?;
        UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
            .set(UdfConfig::new_for_test(&rt, "1.0.0".parse()?))
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&version_one, &manifest_one)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let version_two = CatalogModel::new(&mut tx)
            .stage_version(manifest_two.clone())
            .await?;
        db.commit(tx).await?;

        let mut failed_tx = db.begin_system().await?;
        UdfConfigModel::new(&mut failed_tx, value::TableNamespace::Global)
            .set(UdfConfig::new_for_test(&rt, "2.0.0".parse()?))
            .await?;
        CatalogModel::new(&mut failed_tx)
            .activate(&version_two, &manifest_two)
            .await?;
        drop(failed_tx);

        let mut tx = db.begin_system().await?;
        assert_eq!(
            CatalogModel::new(&mut tx).active_catalog().await?,
            Some(version_one)
        );
        assert_eq!(
            UdfConfigModel::new(&mut tx, value::TableNamespace::Global)
                .get()
                .await?
                .expect("previous UDF config")
                .server_version,
            semver::Version::new(1, 0, 0)
        );
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn activation_rejects_a_swapped_manifest(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let staged_manifest = CatalogManifestDigest::hash(b"staged payload");
        let swapped_manifest = CatalogManifestDigest::hash(b"different payload");

        let mut tx = db.begin_system().await?;
        let activation = CatalogModel::new(&mut tx)
            .stage_version(staged_manifest)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let error = CatalogModel::new(&mut tx)
            .activate(&activation, &swapped_manifest)
            .await
            .expect_err("swapped manifest must fail closed");
        assert!(error.to_string().contains("manifest"));
        assert_eq!(CatalogModel::new(&mut tx).active_catalog().await?, None);
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn replayed_activation_fails_closed(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let manifest = CatalogManifestDigest::hash(b"one-shot activation");

        let mut tx = db.begin_system().await?;
        let activation = CatalogModel::new(&mut tx)
            .stage_version(manifest.clone())
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&activation, &manifest)
            .await?;
        db.commit(tx).await?;

        let mut replay_tx = db.begin_system().await?;
        let error = CatalogModel::new(&mut replay_tx)
            .activate(&activation, &manifest)
            .await
            .expect_err("a catalog activation must not be replayable");
        assert!(error.to_string().contains("compare-and-swap"));
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn delayed_activation_cannot_overwrite_a_newer_catalog(
        rt: TestRuntime,
    ) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let base_manifest = CatalogManifestDigest::hash(b"base");
        let delayed_manifest = CatalogManifestDigest::hash(b"delayed");
        let newer_manifest = CatalogManifestDigest::hash(b"newer");

        let mut tx = db.begin_system().await?;
        let base = CatalogModel::new(&mut tx)
            .stage_version(base_manifest.clone())
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&base, &base_manifest)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let delayed = CatalogModel::new(&mut tx)
            .stage_version(delayed_manifest.clone())
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let newer = CatalogModel::new(&mut tx)
            .stage_version(newer_manifest.clone())
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        CatalogModel::new(&mut tx)
            .activate(&newer, &newer_manifest)
            .await?;
        db.commit(tx).await?;

        let mut delayed_tx = db.begin_system().await?;
        let error = CatalogModel::new(&mut delayed_tx)
            .activate(&delayed, &delayed_manifest)
            .await
            .expect_err("delayed activation must lose the compare-and-swap");
        assert!(error.to_string().contains("compare-and-swap"));

        let mut tx = db.begin_system().await?;
        assert_eq!(
            CatalogModel::new(&mut tx).active_catalog().await?,
            Some(newer)
        );
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn concurrent_activations_conflict_at_the_active_selector(
        rt: TestRuntime,
    ) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let base_manifest = CatalogManifestDigest::hash(b"base");
        let first_manifest = CatalogManifestDigest::hash(b"first concurrent deploy");
        let second_manifest = CatalogManifestDigest::hash(b"second concurrent deploy");

        let mut tx = db.begin_system().await?;
        let base = CatalogModel::new(&mut tx)
            .stage_version(base_manifest.clone())
            .await?;
        CatalogModel::new(&mut tx)
            .activate(&base, &base_manifest)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let first = CatalogModel::new(&mut tx)
            .stage_version(first_manifest.clone())
            .await?;
        db.commit(tx).await?;
        let mut tx = db.begin_system().await?;
        let second = CatalogModel::new(&mut tx)
            .stage_version(second_manifest.clone())
            .await?;
        db.commit(tx).await?;

        let mut first_tx = db.begin_system().await?;
        CatalogModel::new(&mut first_tx)
            .activate(&first, &first_manifest)
            .await?;
        let mut second_tx = db.begin_system().await?;
        CatalogModel::new(&mut second_tx)
            .activate(&second, &second_manifest)
            .await?;

        db.commit(first_tx).await?;
        db.commit(second_tx)
            .await
            .expect_err("only one transaction may replace the active selector");

        let mut tx = db.begin_system().await?;
        assert_eq!(
            CatalogModel::new(&mut tx).active_catalog().await?,
            Some(first),
        );
        Ok(())
    }
}
