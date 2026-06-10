use common::bootstrap_model::index::{
    database_index::{
        DatabaseIndexSpec,
        DatabaseIndexState,
    },
    text_index::{
        TextIndexSpec,
        TextIndexState,
    },
    vector_index::{
        VectorIndexSpec,
        VectorIndexState,
    },
    IndexConfig,
    IndexMetadata,
};
use serde::Serialize;
use serde_json::{
    json,
    Value as JsonValue,
};
use value::{
    ConvexValue,
    TableName,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackfillResponse {
    state: String,
}

// When updating this, please keep `IndexMetadata`
// in `npm-packages/convex/src/cli/lib/indexes.ts` in sync
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexMetadataResponse {
    table: String,
    name: String,
    // Either:
    // - an array of fields (`string[]`) for a database index
    // - `{ searchField: string, filterFields: string[] }` for a search index
    // - `{ dimensions: number, vectorField: string, filterFields: string[] }` for a vector index
    fields: JsonValue,
    backfill: BackfillResponse,
    staged: bool,
}

impl TryFrom<IndexMetadata<TableName>> for IndexMetadataResponse {
    type Error = anyhow::Error;

    fn try_from(meta: IndexMetadata<TableName>) -> Result<Self, Self::Error> {
        let table = meta.name.table().to_string();
        let name = meta.name.descriptor().to_string();
        Ok(match meta.config {
            IndexConfig::Database {
                spec: DatabaseIndexSpec { fields },
                on_disk_state,
            } => {
                let backfill_state = match on_disk_state {
                    DatabaseIndexState::Backfilling(_) => "in_progress".to_string(),
                    // TODO(CX-3851): The result of this is used to poll for state
                    // in the CLI and also for display in the dashboard. We
                    // might consider a new value that would let us
                    // differentiate between Backfilled and Enabled in the
                    // dashboard. The CLI doesn't currently care.
                    DatabaseIndexState::Enabled | DatabaseIndexState::Backfilled { .. } => {
                        "done".to_string()
                    },
                };

                IndexMetadataResponse {
                    table,
                    name,
                    fields: JsonValue::from(ConvexValue::try_from(fields)?),
                    backfill: BackfillResponse {
                        state: backfill_state,
                    },
                    staged: on_disk_state.is_staged(),
                }
            },
            IndexConfig::Text {
                on_disk_state,
                spec:
                    TextIndexSpec {
                        search_field,
                        filter_fields,
                    },
            } => {
                let backfill_state = match on_disk_state {
                    TextIndexState::Backfilling(_) => "in_progress".to_string(),
                    // TODO(CX-3851): The result of this is used to poll for state in the CLI and
                    // also for display in the dashboard. We might consider a new value that would
                    // let us differentiate between Backfilled and SnapshottedAt in the dashboard.
                    // The CLI doesn't currently care.
                    TextIndexState::SnapshottedAt(_) | TextIndexState::Backfilled { .. } => {
                        "done".to_string()
                    },
                };
                IndexMetadataResponse {
                    table,
                    name,
                    fields: json!({
                        "searchField":  String::from(search_field),
                        "filterFields": filter_fields.into_iter().map(String::from).collect::<Vec<_>>()
                    }),
                    backfill: BackfillResponse {
                        state: backfill_state,
                    },
                    staged: on_disk_state.is_staged(),
                }
            },
            IndexConfig::Vector {
                spec:
                    VectorIndexSpec {
                        dimensions,
                        vector_field,
                        filter_fields,
                    },
                on_disk_state,
            } => {
                let backfill_state = match on_disk_state {
                    VectorIndexState::Backfilling(_) => "in_progress".to_string(),
                    VectorIndexState::Backfilled { .. } | VectorIndexState::SnapshottedAt(_) => {
                        "done".to_string()
                    },
                };
                IndexMetadataResponse {
                    table,
                    name,
                    fields: json!({
                        "dimensions": u32::from(dimensions),
                        "vectorField": String::from(vector_field),
                        "filterFields": filter_fields.into_iter().map(String::from).collect::<Vec<_>>()
                    }),
                    backfill: BackfillResponse {
                        state: backfill_state,
                    },
                    staged: on_disk_state.is_staged(),
                }
            },
        })
    }
}
