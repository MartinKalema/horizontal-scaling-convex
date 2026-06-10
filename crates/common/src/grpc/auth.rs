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
}

impl std::fmt::Debug for ClusterGrpcAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterGrpcAuth")
            .field("header", &CLUSTER_GRPC_AUTH_HEADER)
            .finish_non_exhaustive()
    }
}

impl ClusterGrpcAuth {
    pub fn from_shared_secret(secret: impl AsRef<[u8]>) -> anyhow::Result<Self> {
        let mut hasher = Sha256::new();
        hasher.update(b"convex-cluster-grpc-v1");
        hasher.update(secret.as_ref());
        Self::from_bearer_token(hex::encode(hasher.finalize()))
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_test(token: &str) -> anyhow::Result<Self> {
        Self::from_bearer_token(token)
    }

    fn from_bearer_token(token: impl AsRef<str>) -> anyhow::Result<Self> {
        let token = token.as_ref().trim();
        anyhow::ensure!(!token.is_empty(), "cluster gRPC auth token cannot be empty");
        let header_value = MetadataValue::try_from(token)
            .context("cluster gRPC auth token is not valid ASCII metadata")?;
        Ok(Self { header_value })
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
        if constant_time_eq(actual.as_bytes(), self.header_value.as_bytes()) {
            Ok(())
        } else {
            Err(Status::unauthenticated("Invalid cluster gRPC auth token"))
        }
    }
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
}
