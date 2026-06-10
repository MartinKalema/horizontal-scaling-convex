//! Externalizable tokens that record the currently-observed state within a
//! transaction.

use std::{
    collections::BTreeMap,
    sync::Arc,
};

use anyhow::Context;
#[cfg(any(test, feature = "testing"))]
use common::types::TabletIndexName as TestTabletIndexName;
use common::{
    bootstrap_model::index::database_index::IndexedFields,
    interval::IntervalSet,
    query::FilterValue,
    types::{
        IndexDescriptor,
        TabletIndexName,
        Timestamp,
    },
};
use pb::{
    replication::{
        QueryToken as QueryTokenProto,
        TwoPcFilterConditionRead,
        TwoPcIndexedRead,
        TwoPcSearchRead,
        TwoPcTextQueryTermRead,
    },
    searchlight,
};
use search::{
    query::{
        FuzzyDistance,
        QueryReads,
        TextQueryTerm,
    },
    FilterConditionRead,
    TextQueryTermRead,
};
use value::{
    heap_size::HeapSize,
    FieldPath,
    TabletId,
};

use crate::reads::{
    IndexReads,
    ReadSet,
};

/// Serialized representation of [`Token`].
pub type SerializedToken = String;

/// A token is a base64 serializable representation of the current read-state
/// for a transaction. This can be externalized to a user and used to represent
/// current transaction state.
#[derive(Clone, Debug)]
#[cfg_attr(
    any(test, feature = "testing"),
    derive(proptest_derive::Arbitrary, Eq, PartialEq)
)]
pub struct Token {
    read_set: Arc<ReadSet>,
    ts: Timestamp,
}

impl Token {
    #[allow(unused)]
    #[cfg(any(test, feature = "testing"))]
    pub fn new_for_testing(read_set: ReadSet, ts: Timestamp) -> Self {
        Self::new(Arc::new(read_set), ts)
    }

    #[allow(unused)]
    #[cfg(any(test, feature = "testing"))]
    pub fn text_search_token(
        index_name: TestTabletIndexName,
        field_path: FieldPath,
        terms: Vec<TextQueryTerm>,
    ) -> Self {
        use std::time::Duration;

        use common::types::TabletIndexName;
        use pb::searchlight::TextQueryTerm;
        use search::{
            QueryReads,
            TextQueryTermRead,
        };
        use value::heap_size::WithHeapSize;

        use crate::TransactionReadSet;

        let mut read_set = TransactionReadSet::new();
        let mut text_queries: WithHeapSize<Vec<TextQueryTermRead>> = WithHeapSize::default();

        for term in terms {
            text_queries.push(TextQueryTermRead::new(field_path.clone(), term));
        }

        let query_reads = QueryReads::new(text_queries, WithHeapSize::default());

        read_set.record_search(index_name, query_reads);
        let read_set = read_set.into_read_set();
        Token::new_for_testing(
            read_set,
            Timestamp::MIN.add(Duration::from_secs(1)).unwrap(),
        )
    }

    pub fn new(read_set: Arc<ReadSet>, ts: Timestamp) -> Self {
        Self { read_set, ts }
    }

    pub fn empty(ts: Timestamp) -> Self {
        Self {
            read_set: Arc::new(ReadSet::empty()),
            ts,
        }
    }

    pub fn ts(&self) -> Timestamp {
        self.ts
    }

    pub fn reads(&self) -> &ReadSet {
        &self.read_set
    }

    pub fn reads_owned(&self) -> Arc<ReadSet> {
        self.read_set.clone()
    }

    /// Advance the token's timestamp to a new timestamp.
    pub fn advance_ts(&mut self, ts: Timestamp) {
        assert!(self.ts < ts);
        self.ts = ts;
    }
}

impl HeapSize for Token {
    fn heap_size(&self) -> usize {
        self.read_set.heap_size()
    }
}

impl TryFrom<QueryTokenProto> for Token {
    type Error = anyhow::Error;

    fn try_from(value: QueryTokenProto) -> anyhow::Result<Self> {
        let ts = value.ts.context("Missing query token ts")?.try_into()?;
        let indexed_reads = value
            .indexed_reads
            .into_iter()
            .map(
                |TwoPcIndexedRead {
                     tablet_id,
                     descriptor,
                     fields,
                     intervals,
                 }| {
                    let tablet_id = tablet_id_from_bytes(tablet_id)?;
                    let index_name = descriptor_to_index_name(tablet_id, descriptor)?;
                    let fields = fields
                        .into_iter()
                        .map(FieldPath::try_from)
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok((
                        index_name,
                        IndexReads {
                            fields: IndexedFields::try_from(fields)?,
                            intervals: IntervalSet::try_from(intervals)?,
                            stack_traces: None,
                        },
                    ))
                },
            )
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

        let search_reads = value
            .search_reads
            .into_iter()
            .map(
                |TwoPcSearchRead {
                     tablet_id,
                     descriptor,
                     text_queries,
                     filter_conditions,
                 }| {
                    let tablet_id = tablet_id_from_bytes(tablet_id)?;
                    let index_name = descriptor_to_index_name(tablet_id, descriptor)?;
                    let text_queries = text_queries
                        .into_iter()
                        .map(
                            |TwoPcTextQueryTermRead { field_path, term }| -> anyhow::Result<_> {
                                let field_path = field_path
                                    .context("TwoPcTextQueryTermRead missing field_path")?
                                    .try_into()?;
                                let term = text_query_term_from_proto(
                                    term.context("TwoPcTextQueryTermRead missing term")?,
                                )?;
                                Ok(TextQueryTermRead::new(field_path, term))
                            },
                        )
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    let filter_conditions = filter_conditions
                        .into_iter()
                        .map(
                            |TwoPcFilterConditionRead { field_path, value }| -> anyhow::Result<_> {
                                let field_path = field_path
                                    .context("TwoPcFilterConditionRead missing field_path")?
                                    .try_into()?;
                                Ok(FilterConditionRead::Must(
                                    field_path,
                                    FilterValue::from(value),
                                ))
                            },
                        )
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok((
                        index_name,
                        QueryReads::new(text_queries.into(), filter_conditions.into()),
                    ))
                },
            )
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

        Ok(Token::new(
            Arc::new(ReadSet::new(indexed_reads, search_reads)),
            ts,
        ))
    }
}

impl TryFrom<Token> for QueryTokenProto {
    type Error = anyhow::Error;

    fn try_from(value: Token) -> anyhow::Result<Self> {
        let indexed_reads = value
            .reads()
            .iter_indexed()
            .map(|(index_name, reads)| TwoPcIndexedRead {
                tablet_id: tablet_id_to_bytes(*index_name.table()),
                descriptor: index_name.descriptor().to_string(),
                fields: Vec::<pb::common::FieldPath>::from(reads.fields.clone()),
                intervals: reads.intervals.clone().into(),
            })
            .collect();

        let search_reads = value
            .reads()
            .iter_search()
            .map(|(index_name, reads)| -> anyhow::Result<TwoPcSearchRead> {
                Ok(TwoPcSearchRead {
                    tablet_id: tablet_id_to_bytes(*index_name.table()),
                    descriptor: index_name.descriptor().to_string(),
                    text_queries: reads
                        .text_queries
                        .iter()
                        .map(|read| {
                            Ok(TwoPcTextQueryTermRead {
                                field_path: Some(read.field_path.clone().into()),
                                term: Some(text_query_term_to_proto(read.term.clone())),
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    filter_conditions: reads
                        .filter_conditions
                        .iter()
                        .map(|read| match read {
                            FilterConditionRead::Must(field_path, value) => {
                                Ok(TwoPcFilterConditionRead {
                                    field_path: Some(field_path.clone().into()),
                                    value: value.clone().into(),
                                })
                            },
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(QueryTokenProto {
            ts: Some(u64::from(value.ts())),
            indexed_reads,
            search_reads,
        })
    }
}

fn descriptor_to_index_name(
    tablet_id: TabletId,
    descriptor: String,
) -> anyhow::Result<TabletIndexName> {
    match descriptor.as_str() {
        "by_id" => Ok(TabletIndexName::by_id(tablet_id)),
        "by_creation_time" => Ok(TabletIndexName::by_creation_time(tablet_id)),
        _ => TabletIndexName::new(tablet_id, IndexDescriptor::new(descriptor)?),
    }
}

fn tablet_id_to_bytes(tablet_id: TabletId) -> Vec<u8> {
    tablet_id.0 .0.to_vec()
}

fn tablet_id_from_bytes(bytes: Vec<u8>) -> anyhow::Result<TabletId> {
    Ok(TabletId(bytes.try_into()?))
}

fn text_query_term_from_proto(value: searchlight::TextQueryTerm) -> anyhow::Result<TextQueryTerm> {
    match value.term_type {
        Some(searchlight::text_query_term::TermType::Exact(exact)) => {
            Ok(TextQueryTerm::Exact(exact.token))
        },
        Some(searchlight::text_query_term::TermType::Fuzzy(fuzzy)) => Ok(TextQueryTerm::Fuzzy {
            token: fuzzy.token,
            max_distance: FuzzyDistance::try_from(fuzzy.max_distance as u8)?,
            prefix: fuzzy.prefix,
        }),
        None => anyhow::bail!("TextQueryTerm missing term_type"),
    }
}

fn text_query_term_to_proto(value: TextQueryTerm) -> searchlight::TextQueryTerm {
    let term_type = match value {
        TextQueryTerm::Exact(token) => {
            searchlight::text_query_term::TermType::Exact(searchlight::ExactTextTerm { token })
        },
        TextQueryTerm::Fuzzy {
            token,
            max_distance,
            prefix,
        } => searchlight::text_query_term::TermType::Fuzzy(searchlight::FuzzyTextTerm {
            token,
            max_distance: u8::from(max_distance) as u32,
            prefix,
        }),
    };
    searchlight::TextQueryTerm {
        term_type: Some(term_type),
    }
}
