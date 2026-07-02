//! Boundary types for external LLM integrations.
//!
//! LLMs may propose contracts, candidate claims, entity links, and rendered
//! explanations. They must not mark facts verified or hide conflicts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalValidationStatus {
    Proposed,
    AcceptedByMemoryX,
    RejectedByMemoryX,
    NeedsHumanReview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal<T> {
    pub value: T,
    pub proposed_by: String,
    pub model: String,
    pub timestamp_unix_ns: u64,
    pub confidence: f32,
    pub validation_status: ProposalValidationStatus,
}

impl<T> Proposal<T> {
    pub fn new(
        value: T,
        proposed_by: impl Into<String>,
        model: impl Into<String>,
        timestamp_unix_ns: u64,
        confidence: f32,
    ) -> Self {
        Self {
            value,
            proposed_by: proposed_by.into(),
            model: model.into(),
            timestamp_unix_ns,
            confidence: confidence.clamp(0.0, 1.0),
            validation_status: ProposalValidationStatus::Proposed,
        }
    }

    pub fn with_validation_status(mut self, status: ProposalValidationStatus) -> Self {
        self.validation_status = status;
        self
    }

    pub fn is_validated(&self) -> bool {
        self.validation_status == ProposalValidationStatus::AcceptedByMemoryX
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmAllowedOperation {
    ProposeQueryContract,
    ProposeCandidateClaims,
    ProposeEntityLinks,
    RenderExplanation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmForbiddenOperation {
    VerifyFacts,
    HideConflicts,
    ChangeHardConstraints,
    InventSource,
    MarkComplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposals_are_not_validated_by_default() {
        let proposal = Proposal::new("MemoryX explains answers", "agent", "test-model", 42, 1.5);

        assert_eq!(proposal.confidence, 1.0);
        assert_eq!(
            proposal.validation_status,
            ProposalValidationStatus::Proposed
        );
        assert!(!proposal.is_validated());
    }
}
