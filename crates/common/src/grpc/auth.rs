use std::sync::Arc;

use anyhow::Context;
use sha2::{
    Digest,
    Sha256,
};
use tonic::{
    metadata::{
        Ascii,
        MetadataValue,
    },
    Request,
    Status,
};

pub const CLUSTER_GRPC_AUTH_HEADER: &str = "x-convex-cluster-auth";

#[derive(Clone)]
pub struct ClusterGrpcAuth {
    header_value: MetadataValue<Ascii>,
    accepted_token_digests: Arc<[[u8; 32]]>,
}

impl std::fmt::Debug for ClusterGrpcAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterGrpcAuth")
            .field("header", &CLUSTER_GRPC_AUTH_HEADER)
            .field("accepted_token_count", &self.accepted_token_digests.len())
            .finish_non_exhaustive()
    }
}

impl ClusterGrpcAuth {
    pub fn from_shared_secret(secret: impl AsRef<[u8]>) -> anyhow::Result<Self> {
        Self::from_bearer_tokens(Self::derive_token(secret.as_ref()), Vec::new())
    }

    pub fn from_shared_secret_with_previous(
        current_secret: impl AsRef<[u8]>,
        previous_secret: impl AsRef<[u8]>,
    ) -> anyhow::Result<Self> {
        Self::from_bearer_tokens(
            Self::derive_token(current_secret.as_ref()),
            vec![Self::derive_token(previous_secret.as_ref())],
        )
    }

    fn derive_token(secret: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"convex-cluster-grpc-v1");
        hasher.update(secret);
        hex::encode(hasher.finalize())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(token: &str) -> anyhow::Result<Self> {
        Self::from_bearer_tokens(token.to_string(), Vec::new())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_test_with_previous(token: &str, previous_token: &str) -> anyhow::Result<Self> {
        Self::from_bearer_tokens(token.to_string(), vec![previous_token.to_string()])
    }

    fn from_bearer_tokens(
        current_token: String,
        previous_tokens: Vec<String>,
    ) -> anyhow::Result<Self> {
        let current_token = current_token.trim();
        anyhow::ensure!(
            !current_token.is_empty(),
            "cluster gRPC auth token cannot be empty"
        );
        let header_value = MetadataValue::try_from(current_token)
            .context("cluster gRPC auth token is not valid ASCII metadata")?;
        let mut accepted_token_digests = Vec::with_capacity(previous_tokens.len() + 1);
        accepted_token_digests.push(token_digest(current_token.as_bytes()));
        for token in previous_tokens {
            let token = token.trim();
            anyhow::ensure!(
                !token.is_empty(),
                "previous cluster gRPC auth token cannot be empty"
            );
            MetadataValue::try_from(token)
                .context("previous cluster gRPC auth token is not valid ASCII metadata")?;
            accepted_token_digests.push(token_digest(token.as_bytes()));
        }
        Ok(Self {
            header_value,
            accepted_token_digests: Arc::from(accepted_token_digests.into_boxed_slice()),
        })
    }

    pub fn attach<T>(&self, request: &mut Request<T>) {
        request
            .metadata_mut()
            .insert(CLUSTER_GRPC_AUTH_HEADER, self.header_value.clone());
    }

    pub fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        self.attach(&mut request);
        request
    }

    pub fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let actual = request
            .metadata()
            .get(CLUSTER_GRPC_AUTH_HEADER)
            .ok_or_else(|| Status::unauthenticated("Missing cluster gRPC auth token"))?;
        let actual_digest = token_digest(actual.as_bytes());
        if self
            .accepted_token_digests
            .iter()
            .any(|expected| constant_time_eq(&actual_digest, expected))
        {
            Ok(())
        } else {
            Err(Status::unauthenticated("Invalid cluster gRPC auth token"))
        }
    }
}

fn token_digest(token: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"convex-cluster-grpc-auth-token-v1");
    hasher.update(token);
    hasher.finalize().into()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authenticate_requires_matching_token() -> anyhow::Result<()> {
        let auth = ClusterGrpcAuth::for_test("secret")?;
        let mut request = Request::new(());
        assert_eq!(
            auth.authenticate(&request).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );

        request
            .metadata_mut()
            .insert(CLUSTER_GRPC_AUTH_HEADER, MetadataValue::try_from("wrong")?);
        assert_eq!(
            auth.authenticate(&request).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );

        auth.attach(&mut request);
        auth.authenticate(&request)?;
        Ok(())
    }

    #[test]
    fn test_authenticate_accepts_previous_token_during_rotation() -> anyhow::Result<()> {
        let auth = ClusterGrpcAuth::for_test_with_previous("current", "previous")?;
        let previous = ClusterGrpcAuth::for_test("previous")?;
        let current = ClusterGrpcAuth::for_test("current")?;

        let mut previous_request = Request::new(());
        previous.attach(&mut previous_request);
        auth.authenticate(&previous_request)?;

        let mut current_request = Request::new(());
        current.attach(&mut current_request);
        auth.authenticate(&current_request)?;

        let outbound = auth.request(());
        assert_eq!(
            outbound.metadata().get(CLUSTER_GRPC_AUTH_HEADER),
            Some(&auth.header_value),
            "rotating nodes should send the current token, not the previous token",
        );
        Ok(())
    }

    #[test]
    fn test_auth_debug_does_not_expose_token_material() -> anyhow::Result<()> {
        let auth = ClusterGrpcAuth::for_test_with_previous("current-secret", "previous-secret")?;
        let debug = format!("{auth:?}");
        assert!(debug.contains(CLUSTER_GRPC_AUTH_HEADER));
        assert!(debug.contains("accepted_token_count"));
        assert!(!debug.contains("current-secret"));
        assert!(!debug.contains("previous-secret"));
        Ok(())
    }
}
