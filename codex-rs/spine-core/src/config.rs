#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineConfig {
    schema_version: u32,
}

impl SpineConfig {
    pub const fn v1() -> Self {
        Self { schema_version: 1 }
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }
}
