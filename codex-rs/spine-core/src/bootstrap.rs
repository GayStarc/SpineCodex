use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    Jit,
    Trim,
    Spawn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpineRegistration {
    features: BTreeSet<Feature>,
}

impl SpineRegistration {
    pub fn builder() -> SpineRegistrationBuilder {
        SpineRegistrationBuilder {
            features: BTreeSet::new(),
        }
    }

    pub fn is_enabled(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }

    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct SpineRegistrationBuilder {
    features: BTreeSet<Feature>,
}

impl SpineRegistrationBuilder {
    pub fn enable(mut self, feature: Feature) -> Self {
        self.features.insert(feature);
        self
    }

    pub fn build(self) -> Result<SpineRegistration, InitError> {
        if self.features.contains(&Feature::Spawn) && !self.features.contains(&Feature::Jit) {
            return Err(InitError::SpawnRequiresJit);
        }
        Ok(SpineRegistration {
            features: self.features,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    UnsupportedConfigVersion(u32),
    SpawnRequiresJit,
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedConfigVersion(version) => {
                write!(
                    formatter,
                    "unsupported Spine config schema version {version}"
                )
            }
            Self::SpawnRequiresJit => formatter.write_str("Spine spawn requires JIT"),
        }
    }
}

impl std::error::Error for InitError {}
