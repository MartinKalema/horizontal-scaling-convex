use value::{
    obj,
    DocumentObject,
};

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct MockSinkConfig {}

impl TryFrom<DocumentObject> for MockSinkConfig {
    type Error = anyhow::Error;

    fn try_from(_value: DocumentObject) -> Result<Self, Self::Error> {
        Ok(MockSinkConfig {})
    }
}

impl TryFrom<MockSinkConfig> for DocumentObject {
    type Error = anyhow::Error;

    fn try_from(_value: MockSinkConfig) -> Result<Self, Self::Error> {
        obj!(
            "type" => "mock",
        )
    }
}
