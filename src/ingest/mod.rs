//! Safe automatic ingestion pipeline.
//!
//! This module extracts proposals only. Extracted claims are never marked as
//! verified facts by this pipeline.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractorIdentity {
    pub extractor: String,
    pub model_or_tool: String,
    pub version: String,
}

impl Default for ExtractorIdentity {
    fn default() -> Self {
        Self {
            extractor: "memoryx-deterministic-text-extractor".to_owned(),
            model_or_tool: "rule_based".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedClaimStatus {
    ExtractedUnverified,
    Hypothesis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub source: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateClaim {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub status: ExtractedClaimStatus,
    pub confidence: f32,
    pub source_span: SourceSpan,
    pub extractor: ExtractorIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityMention {
    pub label: String,
    pub source_span: SourceSpan,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedRelation {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
    pub source_span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestExtractionPlan {
    pub source: String,
    pub extractor: ExtractorIdentity,
    pub segments: Vec<SourceSpan>,
    pub candidate_claims: Vec<CandidateClaim>,
    pub entity_mentions: Vec<EntityMention>,
    pub suggested_relations: Vec<SuggestedRelation>,
    pub confirmation_required: bool,
    pub confirmation_hint: String,
}

pub struct IngestExtractor;

impl IngestExtractor {
    pub fn dry_run_extract(
        source: impl Into<String>,
        document: &str,
        extractor: ExtractorIdentity,
    ) -> IngestExtractionPlan {
        let source = source.into();
        let segments = segment_document(&source, document);
        let entity_mentions = extract_entities(&segments);
        let candidate_claims = extract_claims(&segments, &extractor);
        let suggested_relations = candidate_claims
            .iter()
            .map(|claim| SuggestedRelation {
                subject: claim.subject.clone(),
                predicate: claim.predicate.clone(),
                object: claim.object.clone(),
                confidence: claim.confidence,
                source_span: claim.source_span.clone(),
            })
            .collect();

        IngestExtractionPlan {
            source,
            extractor,
            segments,
            candidate_claims,
            entity_mentions,
            suggested_relations,
            confirmation_required: true,
            confirmation_hint:
                "Review candidate_claims and confirm through authoring APIs/MCP before writing facts."
                    .to_owned(),
        }
    }
}

fn segment_document(source: &str, document: &str) -> Vec<SourceSpan> {
    let mut segments = Vec::new();
    let mut start = 0usize;

    for part in document.split_terminator(['.', '\n']) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            start = start.saturating_add(part.len() + 1);
            continue;
        }

        let local_start = document[start..]
            .find(trimmed)
            .map(|offset| start + offset)
            .unwrap_or(start);
        let end = local_start + trimmed.len();
        segments.push(SourceSpan {
            source: source.to_owned(),
            start_byte: local_start,
            end_byte: end,
            text: trimmed.to_owned(),
        });
        start = end.saturating_add(1);
    }

    if segments.is_empty() && !document.trim().is_empty() {
        let trimmed = document.trim();
        let local_start = document.find(trimmed).unwrap_or(0);
        segments.push(SourceSpan {
            source: source.to_owned(),
            start_byte: local_start,
            end_byte: local_start + trimmed.len(),
            text: trimmed.to_owned(),
        });
    }

    segments
}

fn extract_entities(segments: &[SourceSpan]) -> Vec<EntityMention> {
    let mut entities = Vec::new();
    for segment in segments {
        for token in segment.text.split_whitespace() {
            let clean = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
            if clean.len() >= 2 && clean.chars().next().is_some_and(char::is_uppercase) {
                entities.push(EntityMention {
                    label: clean.to_owned(),
                    source_span: segment.clone(),
                    confidence: 0.55,
                });
            }
        }
    }
    entities
}

fn extract_claims(segments: &[SourceSpan], extractor: &ExtractorIdentity) -> Vec<CandidateClaim> {
    segments
        .iter()
        .filter_map(|segment| extract_claim_from_segment(segment, extractor))
        .collect()
}

fn extract_claim_from_segment(
    segment: &SourceSpan,
    extractor: &ExtractorIdentity,
) -> Option<CandidateClaim> {
    let words: Vec<_> = segment.text.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }

    let subject = words.first()?.trim_matches(punctuation).to_owned();
    let object = words.last()?.trim_matches(punctuation).to_owned();
    if subject.is_empty() || object.is_empty() || subject.eq_ignore_ascii_case(&object) {
        return None;
    }

    let predicate = words
        .iter()
        .skip(1)
        .take(words.len().saturating_sub(2).min(4))
        .map(|word| word.trim_matches(punctuation))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if predicate.is_empty() {
        return None;
    }

    Some(CandidateClaim {
        subject,
        predicate,
        object,
        status: ExtractedClaimStatus::ExtractedUnverified,
        confidence: 0.45,
        source_span: segment.clone(),
        extractor: extractor.clone(),
    })
}

fn punctuation(c: char) -> bool {
    !c.is_alphanumeric() && c != '_' && c != '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_extraction_never_verifies_claims() {
        let plan = IngestExtractor::dry_run_extract(
            "doc.txt",
            "MemoryX stores knowledge atoms. Codex uses MemoryX.",
            ExtractorIdentity::default(),
        );

        assert!(plan.confirmation_required);
        assert!(!plan.candidate_claims.is_empty());
        assert!(
            plan.candidate_claims
                .iter()
                .all(|claim| claim.status == ExtractedClaimStatus::ExtractedUnverified)
        );
        assert!(!plan.entity_mentions.is_empty());
    }
}
