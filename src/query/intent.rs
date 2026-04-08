//! Intent Classifier for MemoryX SKF-1.1
//!
//! This module implements query intent classification following SKF-1.1 Section 4.1.
//! It converts natural language or structured queries into Intent types.
//!
//! # Classification Rules
//!
//! The classifier uses deterministic pattern matching (no ML) to ensure:
//! - Predictable behavior
//! - No external dependencies
//! - Fast execution (< 1ms)
//!
//! # Intent Types
//!
//! - LOOKUP: Find specific information about entities
//! - DEFINE: Get definitions of terms/concepts
//! - EXPLAIN: Understand causes and mechanisms
//! - COMPARE: Contrast multiple entities
//! - DERIVE: Infer new conclusions from facts
//! - VERIFY: Check if a claim is supported
//! - PLAN: Generate action sequences

#![allow(dead_code)]

use crate::store::Intent;

/// Classification result with confidence and reasoning
#[derive(Debug, Clone, PartialEq)]
pub struct IntentClassification {
    /// The classified intent
    pub intent: Intent,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f64,
    /// Reason for classification
    pub reason: String,
    /// Extracted entities
    pub entities: Vec<String>,
    /// Secondary intents (if ambiguous)
    pub alternatives: Vec<(Intent, f64)>,
}

/// Query classifier for SKF-1.1 intents
pub struct IntentClassifier;

impl IntentClassifier {
    /// Classify a query string into an Intent
    ///
    /// # Algorithm
    ///
    /// 1. Normalize the query (lowercase, trim)
    /// 2. Score each intent based on keyword patterns
    /// 3. Return the highest scoring intent with confidence
    ///
    /// # Examples
    ///
    /// ```
    /// use memoryx::query::IntentClassifier;
    /// use memoryx::store::Intent;
    ///
    /// let result = IntentClassifier::classify("what is the definition of rust");
    /// assert_eq!(result.intent, Intent::DEFINE);
    ///
    /// let result = IntentClassifier::classify("compare rust and cpp");
    /// assert_eq!(result.intent, Intent::COMPARE);
    /// ```
    pub fn classify(query: &str) -> IntentClassification {
        let normalized = Self::normalize(query);

        // Score each intent
        let scores = [
            (Intent::LOOKUP, Self::score_lookup(&normalized)),
            (Intent::DEFINE, Self::score_define(&normalized)),
            (Intent::EXPLAIN, Self::score_explain(&normalized)),
            (Intent::COMPARE, Self::score_compare(&normalized)),
            (Intent::DERIVE, Self::score_derive(&normalized)),
            (Intent::VERIFY, Self::score_verify(&normalized)),
            (Intent::PLAN, Self::score_plan(&normalized)),
        ];

        // Find best match
        let (best_intent, best_score) = scores
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .copied()
            .unwrap_or((Intent::LOOKUP, 0.0));

        // Calculate confidence
        let total_score: f64 = scores.iter().map(|(_, s)| s).sum();
        let confidence = if total_score > 0.0 {
            best_score / total_score
        } else {
            0.5
        };

        // Build alternatives (other intents with > 20% of best score)
        let alternatives: Vec<(Intent, f64)> = scores
            .iter()
            .filter(|(intent, score)| *intent != best_intent && *score > best_score * 0.2)
            .map(|(i, s)| (*i, *s))
            .collect();

        // Extract entities (simple noun phrase extraction)
        let entities = Self::extract_entities(&normalized);

        IntentClassification {
            intent: best_intent,
            confidence: confidence.min(1.0),
            reason: Self::reason(best_intent, &normalized),
            entities,
            alternatives,
        }
    }

    /// Normalize query for classification
    fn normalize(query: &str) -> String {
        query
            .to_lowercase()
            .replace("?", "")
            .replace("!", "")
            .replace(".", "")
            .replace(",", "")
            .trim()
            .to_string()
    }

    /// Score LOOKUP intent (0.0 - 1.0)
    fn score_lookup(query: &str) -> f64 {
        let patterns = &[
            "what is",
            "what are",
            "who is",
            "who are",
            "where is",
            "where are",
            "when did",
            "when was",
            "how many",
            "how much",
            "find",
            "get",
            "lookup",
            "look up",
            "search for",
            "tell me about",
            "information about",
            "details about",
            "facts about",
        ];

        Self::match_patterns(query, patterns, 0.8)
    }

    /// Score DEFINE intent (0.0 - 1.0)
    fn score_define(query: &str) -> f64 {
        let patterns = &[
            "define",
            "definition of",
            "what does mean",
            "meaning of",
            "explain the term",
            "what is the meaning",
            "describe",
            "description of",
            "what defines",
        ];

        let score = Self::match_patterns(query, patterns, 1.0);

        // Boost for explicit "definition" keyword
        if query.contains("definition") {
            score + 0.3
        } else {
            score
        }
    }

    /// Score EXPLAIN intent (0.0 - 1.0)
    fn score_explain(query: &str) -> f64 {
        let patterns = &[
            "explain",
            "why does",
            "why is",
            "why are",
            "how does",
            "how is",
            "how are",
            "what causes",
            "what makes",
            "reason for",
            "reasons for",
            "cause of",
            "causes of",
            "mechanism of",
            "process of",
            "how it works",
            "why did",
        ];

        Self::match_patterns(query, patterns, 1.0)
    }

    /// Score COMPARE intent (0.0 - 1.0)
    fn score_compare(query: &str) -> f64 {
        let patterns = &[
            "compare",
            "comparison",
            "difference between",
            "differences between",
            "versus",
            "vs",
            "or",
            "better than",
            "worse than",
            "similarities",
            "contrast",
            "distinction",
            "how do differ",
        ];

        let score = Self::match_patterns(query, patterns, 1.0);

        // Boost if contains "and" (likely comparing two things)
        if query.contains(" and ") {
            score + 0.2
        } else {
            score
        }
    }

    /// Score DERIVE intent (0.0 - 1.0)
    fn score_derive(query: &str) -> f64 {
        let patterns = &[
            "derive",
            "calculate",
            "compute",
            "solve",
            "what is the result",
            "what would be",
            "if then",
            "consequence of",
            "implication",
            "infer",
            "deduce",
            "conclude",
            "predict",
            "forecast",
            "estimate",
        ];

        Self::match_patterns(query, patterns, 1.0)
    }

    /// Score VERIFY intent (0.0 - 1.0)
    fn score_verify(query: &str) -> f64 {
        let patterns = &[
            "verify",
            "confirm",
            "check if",
            "is it true",
            "is that true",
            "validate",
            "prove",
            "evidence for",
            "support for",
            "is correct",
            "is accurate",
            "is valid",
            "truth of",
            "certainty of",
            "is there proof",
        ];

        Self::match_patterns(query, patterns, 1.0)
    }

    /// Score PLAN intent (0.0 - 1.0)
    fn score_plan(query: &str) -> f64 {
        let patterns = &[
            "plan",
            "how to",
            "steps to",
            "guide to",
            "procedure for",
            "process for",
            "method for",
            "way to",
            "approach to",
            "strategy for",
            "roadmap",
            "blueprint",
            "instructions",
            "what should i do",
            "recommendations",
        ];

        Self::match_patterns(query, patterns, 1.0)
    }

    /// Match patterns and return normalized score
    fn match_patterns(query: &str, patterns: &[&str], max_score: f64) -> f64 {
        let matches: usize = patterns.iter().filter(|p| query.contains(*p)).count();

        let score = (matches as f64 / patterns.len() as f64).sqrt() * max_score;
        score.min(max_score)
    }

    /// Simple entity extraction
    fn extract_entities(query: &str) -> Vec<String> {
        // Simple approach: capitalize words that aren't stop words
        let stop_words: &[&str] = &[
            "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has",
            "had", "do", "does", "did", "will", "would", "could", "should", "may", "might", "must",
            "shall", "can", "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with",
            "at", "by", "from", "as", "into", "through", "during", "before", "after", "above",
            "below", "between", "and", "but", "or", "yet", "so", "if", "because", "although",
            "though", "while", "where", "when", "that", "which", "who", "whom", "whose", "what",
            "this", "these", "those", "i", "you", "he", "she", "it", "we", "they", "me", "him",
            "her", "us", "them",
        ];

        query
            .split_whitespace()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .filter(|w| !stop_words.contains(w))
            .map(|w| w.to_string())
            .collect()
    }

    /// Generate classification reason
    fn reason(intent: Intent, query: &str) -> String {
        match intent {
            Intent::LOOKUP => format!("Query '{}' seeks factual information", query),
            Intent::DEFINE => format!("Query '{}' requests a definition", query),
            Intent::EXPLAIN => format!("Query '{}' asks for explanation/cause", query),
            Intent::COMPARE => format!("Query '{}' compares entities", query),
            Intent::DERIVE => format!("Query '{}' seeks derivation/inference", query),
            Intent::VERIFY => format!("Query '{}' requests verification", query),
            Intent::PLAN => format!("Query '{}' seeks a plan/procedure", query),
        }
    }
}

/// Structured query for programmatic use
#[derive(Debug, Clone)]
pub struct StructuredQuery {
    /// Explicit intent override
    pub intent: Option<Intent>,
    /// Entity identifiers
    pub entity_ids: Vec<String>,
    /// Time constraints
    pub time_range: Option<(u64, u64)>,
    /// Domain filter
    pub domain: Option<String>,
    /// Minimum trust level
    pub min_trust: Option<u8>,
}

impl StructuredQuery {
    /// Convert structured query to Intent
    pub fn to_intent(&self) -> Intent {
        self.intent.unwrap_or(if self.entity_ids.len() >= 2 {
            Intent::COMPARE
        } else {
            Intent::LOOKUP
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_lookup() {
        let queries = vec![
            "what is rust",
            "who is the president",
            "how many people",
            "find information about",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
        }
    }

    #[test]
    fn test_classify_define() {
        let queries = vec![
            "define rust programming",
            "what is the definition of blockchain",
            "meaning of life",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::DEFINE));
        }
    }

    #[test]
    fn test_classify_explain() {
        let queries = vec![
            "why is the sky blue",
            "explain rust ownership",
            "what causes climate change",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::EXPLAIN));
        }
    }

    #[test]
    fn test_classify_compare() {
        let queries = vec![
            "compare rust and cpp",
            "difference between java and python",
            "rust vs go",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::COMPARE));
        }
    }

    #[test]
    fn test_classify_verify() {
        let queries = vec![
            "is it true that earth is flat",
            "verify this claim",
            "confirm that rust is safe",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::VERIFY));
        }
    }

    #[test]
    fn test_classify_plan() {
        let queries = vec![
            "how to build a house",
            "steps to deploy kubernetes",
            "guide to learn rust",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::PLAN));
        }
    }

    #[test]
    fn test_classify_derive() {
        let queries = vec![
            "calculate the result",
            "predict the outcome",
            "infer the conclusion",
        ];

        for query in queries {
            let result = IntentClassifier::classify(query);
            println!(
                "'{}' -> {:?} (conf: {:.2})",
                query, result.intent, result.confidence
            );
            assert!(matches!(result.intent, Intent::DERIVE));
        }
    }

    #[test]
    fn test_entity_extraction() {
        let result = IntentClassifier::classify("what is rust programming language");
        println!("Entities: {:?}", result.entities);
        assert!(!result.entities.is_empty());
    }

    #[test]
    fn test_confidence_range() {
        let result = IntentClassifier::classify("what is rust");
        assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    }
}
