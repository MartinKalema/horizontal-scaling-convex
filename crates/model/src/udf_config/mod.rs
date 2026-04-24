use std::sync::{
    Arc,
    LazyLock,
};

use anyhow::Context;
use common::{
    document::{
        ParseDocument,
        ParsedDocument,
    },
    runtime::Runtime,
};
use database::{
    unauthorized_error,
    SystemMetadataModel,
    Transaction,
};
use errors::ErrorMetadataAnyhowExt;

pub mod types;
use types::UdfConfig;
use value::{
    DocumentObject,
    InternalId,
    PublicDocumentId,
    ResolvedDocumentId,
    TableName,
    TableNamespace,
};

use crate::{
    config::types::UdfServerVersionDiff,
    SystemIndex,
    SystemTable,
};

pub static UDF_CONFIG_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_udf_config"
        .parse()
        .expect("Invalid built-in UDF config table")
});

const GLOBAL_UDF_CONFIG_INTERNAL_ID: InternalId = InternalId(*b"udf_config_root!");

fn fixed_internal_id(namespace: TableNamespace) -> InternalId {
    match namespace {
        TableNamespace::Global => GLOBAL_UDF_CONFIG_INTERNAL_ID,
        TableNamespace::ByComponent(component_id) => component_id.internal_id(),
    }
}

pub struct UdfConfigTable;
impl SystemTable for UdfConfigTable {
    type Metadata = UdfConfig;

    fn table_name() -> &'static TableName {
        &UDF_CONFIG_TABLE
    }

    fn indexes() -> Vec<SystemIndex<Self>> {
        vec![]
    }
}

pub struct UdfConfigModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
    namespace: TableNamespace,
}

impl<'a, RT: Runtime> UdfConfigModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>, namespace: TableNamespace) -> Self {
        Self { tx, namespace }
    }

    async fn all_configs(&mut self) -> anyhow::Result<Vec<Arc<ParsedDocument<UdfConfig>>>> {
        self.tx
            .query_system(self.namespace, &SystemIndex::<UdfConfigTable>::by_id())?
            .all()
            .await
    }

    fn preferred_config(
        configs: &[Arc<ParsedDocument<UdfConfig>>],
        fixed_id: InternalId,
    ) -> Option<Arc<ParsedDocument<UdfConfig>>> {
        configs
            .iter()
            .find(|doc| doc.id().internal_id() == fixed_id)
            .cloned()
            .or_else(|| {
                configs
                    .iter()
                    .max_by_key(|doc| (doc.creation_time(), doc.id()))
                    .cloned()
            })
    }

    fn fixed_doc_id(&mut self) -> anyhow::Result<ResolvedDocumentId> {
        let table_id = self
            .tx
            .table_mapping()
            .namespace(self.namespace)
            .id(&UDF_CONFIG_TABLE)?;
        Ok(ResolvedDocumentId::new(
            table_id.tablet_id,
            PublicDocumentId::new(table_id.table_number, fixed_internal_id(self.namespace)),
        ))
    }

    async fn replace_fixed_config(
        &mut self,
        value: DocumentObject,
    ) -> anyhow::Result<Arc<ParsedDocument<UdfConfig>>> {
        let fixed_doc_id = self.fixed_doc_id()?;
        let existing_doc = SystemMetadataModel::new(self.tx, self.namespace)
            .get(fixed_doc_id)
            .await?
            .context("Fixed _udf_config row appeared but could not be loaded")?;
        let parsed: ParsedDocument<UdfConfig> = existing_doc.parse()?;
        SystemMetadataModel::new(self.tx, self.namespace)
            .replace(fixed_doc_id, value)
            .await?;
        Ok(Arc::new(parsed))
    }

    pub async fn get(&mut self) -> anyhow::Result<Option<Arc<ParsedDocument<UdfConfig>>>> {
        let configs = self.all_configs().await?;
        if configs.len() > 1 {
            tracing::warn!(
                namespace = ?self.namespace,
                count = configs.len(),
                "Multiple _udf_config rows found; using fixed row"
            );
        }
        Ok(Self::preferred_config(
            &configs,
            fixed_internal_id(self.namespace),
        ))
    }

    #[fastrace::trace]
    pub async fn set(
        &mut self,
        new_config: UdfConfig,
    ) -> anyhow::Result<Option<UdfServerVersionDiff>> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("set_udf_config"));
        }
        let new_server_version = new_config.server_version.clone();
        let value: DocumentObject = new_config.try_into()?;
        let fixed_id = fixed_internal_id(self.namespace);
        let existing_docs = self.all_configs().await?;
        let preferred_doc = Self::preferred_config(&existing_docs, fixed_id);
        let opt_previous_version = if let Some(existing_doc) = preferred_doc {
            if existing_doc.id().internal_id() == fixed_id {
                SystemMetadataModel::new(self.tx, self.namespace)
                    .replace(existing_doc.id(), value.clone())
                    .await?;
            } else {
                match SystemMetadataModel::new(self.tx, self.namespace)
                    .insert_with_internal_id(&UDF_CONFIG_TABLE, fixed_id, value.clone())
                    .await
                {
                    Ok(_) => {},
                    Err(err) if err.short_msg() == "DocumentExists" => {
                        self.replace_fixed_config(value.clone()).await?;
                    },
                    Err(err) => return Err(err),
                }
            }
            for stale_doc in existing_docs {
                if stale_doc.id().internal_id() != fixed_id {
                    SystemMetadataModel::new(self.tx, self.namespace)
                        .delete(stale_doc.id())
                        .await?;
                }
            }
            Some(existing_doc.server_version.clone())
        } else {
            match SystemMetadataModel::new(self.tx, self.namespace)
                .insert_with_internal_id(&UDF_CONFIG_TABLE, fixed_id, value.clone())
                .await
            {
                Ok(_) => None,
                Err(err) if err.short_msg() == "DocumentExists" => {
                    let existing_doc = self.replace_fixed_config(value).await?;
                    Some(existing_doc.server_version.clone())
                },
                Err(err) => return Err(err),
            }
        };

        let version_diff = match opt_previous_version {
            Some(previous_version) => {
                if previous_version != new_server_version {
                    Some(UdfServerVersionDiff {
                        previous_version: previous_version.to_string(),
                        next_version: new_server_version.to_string(),
                    })
                } else {
                    None
                }
            },
            None => Some(UdfServerVersionDiff {
                previous_version: "Unspecified version".to_string(),
                next_version: new_server_version.to_string(),
            }),
        };
        Ok(version_diff)
    }
}

#[cfg(test)]
mod tests {
    use database::test_helpers::DbFixtures;
    use keybroker::Identity;
    use runtime::testing::TestRuntime;
    use value::TableNamespace;

    use super::*;
    use crate::{
        test_helpers::DbFixturesWithModel,
        udf_config::types::UdfConfig,
    };

    #[convex_macro::test_runtime]
    async fn test_set_converges_duplicate_udf_config_rows(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;

        let config_v1 = UdfConfig::new_for_test(&rt, "1000.0.0".parse()?);
        let config_v2 = UdfConfig::new_for_test(&rt, "1001.0.0".parse()?);

        let mut tx = db.begin(Identity::system()).await?;
        SystemMetadataModel::new(&mut tx, TableNamespace::Global)
            .insert(&UDF_CONFIG_TABLE, config_v1.clone().try_into()?)
            .await?;
        SystemMetadataModel::new(&mut tx, TableNamespace::Global)
            .insert(&UDF_CONFIG_TABLE, config_v1.try_into()?)
            .await?;
        db.commit(tx).await?;

        let mut tx = db.begin(Identity::system()).await?;
        let diff = UdfConfigModel::new(&mut tx, TableNamespace::Global)
            .set(config_v2.clone())
            .await?;
        assert!(diff.is_some());
        db.commit(tx).await?;

        let mut tx = db.begin_system().await?;
        let docs = tx
            .query_system(
                TableNamespace::Global,
                &SystemIndex::<UdfConfigTable>::by_id(),
            )?
            .all()
            .await?;
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id().internal_id(), GLOBAL_UDF_CONFIG_INTERNAL_ID);
        assert_eq!(**docs[0], config_v2);

        Ok(())
    }
}
