use crate::Feature;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    UnsupportedConfigVersion(u32),
    SpawnRequiresJit,
    MissingPrompt(Feature),
    MissingToolDescription(&'static str),
    InvalidHandlerCardinality {
        context_owners: usize,
        observers: usize,
    },
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
            Self::MissingPrompt(feature) => write!(formatter, "missing prompt for {feature:?}"),
            Self::MissingToolDescription(name) => {
                write!(formatter, "missing tool description for spine.{name}")
            }
            Self::InvalidHandlerCardinality {
                context_owners,
                observers,
            } => write!(
                formatter,
                "active Spine runtime requires exactly one context owner and at least one observer \
                 (found {context_owners} context owners and {observers} observers)"
            ),
        }
    }
}

impl std::error::Error for InitError {}
