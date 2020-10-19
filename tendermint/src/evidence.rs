//! Evidence of malfeasance by validators (i.e. signing conflicting votes).

use crate::{block::signed_header::SignedHeader, serializers, Error, Kind, Vote};
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};
use std::slice;
use tendermint_proto::types::evidence::Sum as RawSum;
use tendermint_proto::types::evidence::Sum;
use tendermint_proto::types::DuplicateVoteEvidence as RawDuplicateVoteEvidence;
use tendermint_proto::types::Evidence as RawEvidence;
use tendermint_proto::types::EvidenceData as RawEvidenceData;

/// Evidence of malfeasance by validators (i.e. signing conflicting votes).
/// encoded using an Amino prefix. There is currently only a single type of
/// evidence: `DuplicateVoteEvidence`.
///
/// <https://github.com/tendermint/spec/blob/d46cd7f573a2c6a2399fcab2cde981330aa63f37/spec/core/data_structures.md#evidence>
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
//#[serde(tag = "type", content = "value")]
#[serde(try_from = "RawEvidence", into = "RawEvidence")] // Used by RPC /broadcast_evidence endpoint
pub enum Evidence {
    /// Duplicate vote evidence
    //#[serde(rename = "tendermint/DuplicateVoteEvidence")]
    DuplicateVote(DuplicateVoteEvidence),

    /// Conflicting headers evidence - Todo: this is not implemented in protobuf, it's ignored now
    //#[serde(rename = "tendermint/ConflictingHeadersEvidence")]
    ConflictingHeaders(Box<ConflictingHeadersEvidence>),

    /// LightClient attack evidence - Todo: Implement details
    LightClientAttackEvidence,
}

impl TryFrom<RawEvidence> for Evidence {
    type Error = Error;

    fn try_from(value: RawEvidence) -> Result<Self, Self::Error> {
        match value.sum.ok_or(Kind::InvalidEvidence)? {
            Sum::DuplicateVoteEvidence(ev) => Ok(Evidence::DuplicateVote(ev.try_into()?)),
            Sum::LightClientAttackEvidence(_ev) => Ok(Evidence::LightClientAttackEvidence),
        }
    }
}

impl From<Evidence> for RawEvidence {
    fn from(value: Evidence) -> Self {
        match value {
            Evidence::DuplicateVote(ev) => RawEvidence {
                sum: Some(RawSum::DuplicateVoteEvidence(ev.into())),
            },
            Evidence::ConflictingHeaders(_ev) => RawEvidence { sum: None }, // Todo: implement
            Evidence::LightClientAttackEvidence => RawEvidence { sum: None }, // Todo: implement
        }
    }
}

/// Duplicate vote evidence
#[derive(Clone, Debug, PartialEq)]
pub struct DuplicateVoteEvidence {
    vote_a: Vote,
    vote_b: Vote,
}

impl TryFrom<RawDuplicateVoteEvidence> for DuplicateVoteEvidence {
    type Error = Error;

    fn try_from(value: RawDuplicateVoteEvidence) -> Result<Self, Self::Error> {
        Ok(Self {
            vote_a: value.vote_a.ok_or(Kind::MissingEvidence)?.try_into()?,
            vote_b: value.vote_b.ok_or(Kind::MissingEvidence)?.try_into()?,
        })
    }
}

impl From<DuplicateVoteEvidence> for RawDuplicateVoteEvidence {
    fn from(value: DuplicateVoteEvidence) -> Self {
        RawDuplicateVoteEvidence {
            vote_a: Some(value.vote_a.into()),
            vote_b: Some(value.vote_b.into()),
        }
    }
}

impl DuplicateVoteEvidence {
    /// constructor
    pub fn new(vote_a: Vote, vote_b: Vote) -> Result<Self, Error> {
        if vote_a.height != vote_b.height {
            return Err(Kind::InvalidEvidence.into());
        }
        // Todo: make more assumptions about what is considered a valid evidence for duplicate vote
        Ok(Self { vote_a, vote_b })
    }
    /// Get votes
    pub fn votes(&self) -> (&Vote, &Vote) {
        (&self.vote_a, &self.vote_b)
    }
}

/// Conflicting headers evidence.
// Todo: This struct doesn't seem to have a protobuf definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictingHeadersEvidence {
    //#[serde(rename = "H1")]
    h1: SignedHeader,
    //#[serde(rename = "H2")]
    h2: SignedHeader,
}

impl ConflictingHeadersEvidence {
    /// Create a new evidence of conflicting headers
    pub fn new(h1: SignedHeader, h2: SignedHeader) -> Self {
        Self { h1, h2 }
    }
}

/// Evidence data is a wrapper for a list of `Evidence`.
///
/// <https://github.com/tendermint/spec/blob/d46cd7f573a2c6a2399fcab2cde981330aa63f37/spec/core/data_structures.md#evidencedata>
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    evidence: Option<Vec<Evidence>>,
}

impl TryFrom<RawEvidenceData> for Data {
    type Error = Error;
    fn try_from(value: RawEvidenceData) -> Result<Self, Self::Error> {
        if value.evidence.is_empty() {
            return Ok(Self { evidence: None });
        }
        let evidence: Result<Vec<Evidence>, Error> =
            value.evidence.into_iter().map(TryInto::try_into).collect();
        Ok(Self {
            evidence: Some(evidence?),
        })
    }
}

impl From<Data> for RawEvidenceData {
    fn from(value: Data) -> Self {
        RawEvidenceData {
            evidence: value
                .evidence
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl Data {
    /// Create a new evidence data collection
    pub fn new<I>(into_evidence: I) -> Data
    where
        I: Into<Vec<Evidence>>,
    {
        Data {
            evidence: Some(into_evidence.into()),
        }
    }

    /// Convert this evidence data into a vector
    pub fn into_vec(self) -> Vec<Evidence> {
        self.iter().cloned().collect()
    }

    /// Iterate over the evidence data
    pub fn iter(&self) -> slice::Iter<'_, Evidence> {
        self.as_ref().iter()
    }
}

impl AsRef<[Evidence]> for Data {
    fn as_ref(&self) -> &[Evidence] {
        self.evidence.as_deref().unwrap_or_else(|| &[])
    }
}

/// Evidence collection parameters
#[derive(Deserialize, Serialize, Clone, Debug, Eq, PartialEq)]
pub struct Params {
    /// Maximum allowed age for evidence to be collected
    #[serde(with = "serializers::from_str")]
    pub max_age_num_blocks: u64,

    /// Max age duration
    pub max_age_duration: Duration,
}

/// Duration is a wrapper around std::time::Duration
/// essentially, to keep the usages look cleaner
/// i.e. you can avoid using serde annotations everywhere
#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Duration(#[serde(with = "serializers::time_duration")] pub std::time::Duration);

impl From<Duration> for std::time::Duration {
    fn from(d: Duration) -> std::time::Duration {
        d.0
    }
}
