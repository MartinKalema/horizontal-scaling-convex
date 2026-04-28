use value::PublicDocumentId;

/// Metrics related to document writes
pub struct BiggestDocumentWrites {
    pub max_size: (PublicDocumentId, usize),
    pub max_nesting: (PublicDocumentId, usize),
}
