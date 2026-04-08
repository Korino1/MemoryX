//! Gap Templates for MemoryX SKF-1.1
//!
//! This module implements structured gap generation following SKF-1.1 Section 4.2.
//! Gaps represent missing knowledge that needs to be retrieved.
//!
//! # Gap Templates
//!
//! Each intent has specific gap templates that define what knowledge is needed.
//! Templates include:
//! - Pattern matching rules
//! - Navigation hints for retrieval
//! - Priority levels
//! - Stop conditions

#![allow(dead_code)]

use crate::store::api::Gap;
use crate::store::{
    ClaimPattern, EdgeType, GapKind, Intent, NavHint, NodeNum, ObjTag, PatternRef, StopCond, SymId,
    TrustLevel,
};

/// Gap template for structured generation
#[derive(Debug, Clone)]
pub struct GapTemplate {
    /// Template kind
    pub kind: GapKind,
    /// Default priority (0-255)
    pub priority: u8,
    /// Pattern template with placeholders
    pub pattern_template: ClaimPatternTemplate,
    /// Navigation hints
    pub nav_template: NavHintTemplate,
    /// Stop conditions
    pub stop_template: StopCondTemplate,
}

/// Claim pattern template with placeholders
#[derive(Debug, Clone)]
pub struct ClaimPatternTemplate {
    /// Subject pattern
    pub subj: PatternRefTemplate,
    /// Predicate pattern
    pub pred: PatternRefTemplate,
    /// Object tag filter
    pub obj_tag: Option<ObjTag>,
    /// Object pattern
    pub obj: PatternRefTemplate,
    /// Qualifiers mask
    pub qualifiers_mask: u32,
}

impl ClaimPatternTemplate {
    /// Instantiate with concrete values
    pub fn instantiate(&self, entity: Option<NodeNum>) -> ClaimPattern {
        ClaimPattern {
            subj: self.subj.instantiate(entity),
            pred: self.pred.instantiate(entity),
            obj_tag: self.obj_tag,
            obj: self.obj.instantiate(entity),
            qualifiers_mask: self.qualifiers_mask,
        }
    }
}

/// Pattern reference template
#[derive(Debug, Clone)]
pub enum PatternRefTemplate {
    /// Match any
    Any,
    /// Match specific symbol
    Symbol(SymId),
    /// Match specific node
    Node(NodeNum),
    /// Match entity parameter
    Entity,
    /// Match specific constant
    Const(crate::vm::ConstValue),
}

impl PatternRefTemplate {
    /// Instantiate with concrete entity
    pub fn instantiate(&self, entity: Option<NodeNum>) -> PatternRef {
        match self {
            PatternRefTemplate::Any => PatternRef::Any,
            PatternRefTemplate::Symbol(s) => PatternRef::Sym(*s),
            PatternRefTemplate::Node(n) => PatternRef::Node(*n),
            PatternRefTemplate::Entity => entity.map(PatternRef::Node).unwrap_or(PatternRef::Any),
            PatternRefTemplate::Const(v) => {
                // Convert ConstValue to appropriate PatternRef
                match v {
                    crate::vm::ConstValue::I64(i) => PatternRef::Range { min: *i, max: *i },
                    _ => PatternRef::Any,
                }
            }
        }
    }
}

/// Navigation hint template
#[derive(Debug, Clone, Default)]
pub struct NavHintTemplate {
    /// Seed nodes to start from
    pub seed_nodes: Vec<NodeNum>,
    /// Edge types to traverse
    pub edge_types: Vec<EdgeType>,
    /// Max traversal depth
    pub max_depth: u8,
    /// Fanout limit
    pub fanout_limit: u16,
}

impl NavHintTemplate {
    /// Instantiate with concrete values
    pub fn instantiate(&self, entity: Option<NodeNum>) -> NavHint {
        let seed_nodes = if self.seed_nodes.is_empty() {
            entity.map_or_else(Vec::new, |node| vec![node])
        } else {
            self.seed_nodes.clone()
        };

        NavHint {
            seed_nodes,
            edge_types: self.edge_types.clone(),
            max_depth: self.max_depth,
            fanout_limit: self.fanout_limit,
        }
    }
}

/// Stop condition template
#[derive(Debug, Clone, Default)]
pub struct StopCondTemplate {
    /// Max nodes to retrieve
    pub max_nodes: u32,
    /// Max I/O bytes
    pub max_io_bytes: u64,
    /// Min trust threshold
    pub min_trust: TrustLevel,
    /// Max conflicts allowed
    pub max_conflicts: u32,
}

impl StopCondTemplate {
    /// Instantiate with defaults
    pub fn instantiate(&self) -> StopCond {
        StopCond {
            max_nodes: self.max_nodes,
            max_io_bytes: self.max_io_bytes,
            min_trust: self.min_trust,
            max_conflicts: self.max_conflicts,
        }
    }
}

/// Gap generator with templates
pub struct GapGenerator;

impl GapGenerator {
    /// Generate gaps for a specific intent
    ///
    /// # Arguments
    ///
    /// * `intent` - The query intent
    /// * `entities` - Entity node numbers involved
    ///
    /// # Returns
    ///
    /// Vector of gaps with priorities assigned
    pub fn generate(intent: Intent, entities: &[NodeNum]) -> Vec<Gap> {
        let templates = Self::get_templates(intent);
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        for template in templates {
            // Create a gap for each entity
            for entity in entities {
                let pattern = template.pattern_template.instantiate(Some(*entity));
                let nav = template.nav_template.instantiate(Some(*entity));
                let stop = template.stop_template.instantiate();

                let mut gap = Gap::new(gap_id, template.kind, pattern);
                gap.set_priority(template.priority);
                gap.set_nav(nav);
                gap.set_stop(stop);

                gaps.push(gap);
                gap_id += 1;
            }

            // If no entities, still create gaps with Any patterns
            if entities.is_empty() {
                let pattern = template.pattern_template.instantiate(None);
                let nav = template.nav_template.instantiate(None);
                let stop = template.stop_template.instantiate();

                let mut gap = Gap::new(gap_id, template.kind, pattern);
                gap.set_priority(template.priority);
                gap.set_nav(nav);
                gap.set_stop(stop);

                gaps.push(gap);
                gap_id += 1;
            }
        }

        gaps
    }

    /// Get templates for a specific intent
    fn get_templates(intent: Intent) -> Vec<GapTemplate> {
        match intent {
            Intent::LOOKUP => Self::lookup_templates(),
            Intent::DEFINE => Self::define_templates(),
            Intent::EXPLAIN => Self::explain_templates(),
            Intent::COMPARE => Self::compare_templates(),
            Intent::DERIVE => Self::derive_templates(),
            Intent::VERIFY => Self::verify_templates(),
            Intent::PLAN => Self::plan_templates(),
        }
    }

    /// Templates for LOOKUP intent
    fn lookup_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need facts about entity
            GapTemplate {
                kind: GapKind::NEED_FACT,
                priority: 200,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DEFINES, EdgeType::IMPLIES, EdgeType::SUPPORTS],
                    max_depth: 2,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 100,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 50,
                    max_conflicts: 10,
                },
            },
            // Secondary: Need evidence
            GapTemplate {
                kind: GapKind::NEED_EVIDENCE,
                priority: 150,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DERIVED_FROM, EdgeType::SUPPORTS],
                    max_depth: 3,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 512 * 1024,
                    min_trust: 70,
                    max_conflicts: 5,
                },
            },
        ]
    }

    /// Templates for DEFINE intent
    fn define_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need definition
            GapTemplate {
                kind: GapKind::NEED_DEFINITION,
                priority: 255, // Highest priority
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Symbol(1), // "defines" predicate
                    obj_tag: Some(ObjTag::SYM),
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DEFINES, EdgeType::REFINES, EdgeType::SAME_AS],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 10,
                    max_io_bytes: 256 * 1024,
                    min_trust: 80,
                    max_conflicts: 2,
                },
            },
            // Constraints: Applicability conditions
            GapTemplate {
                kind: GapKind::NEED_CONSTRAINTS,
                priority: 180,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0xFF, // Qualifiers matter
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DEPENDS_ON, EdgeType::PREVENTS],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 20,
                    max_io_bytes: 256 * 1024,
                    min_trust: 60,
                    max_conflicts: 5,
                },
            },
        ]
    }

    /// Templates for EXPLAIN intent
    fn explain_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need causal chain
            GapTemplate {
                kind: GapKind::NEED_CAUSAL_CHAIN,
                priority: 255,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(3), // "causes" predicate
                    obj_tag: Some(ObjTag::REF),
                    obj: PatternRefTemplate::Entity,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::CAUSES, EdgeType::ENABLES, EdgeType::IMPLIES],
                    max_depth: 3,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 60,
                    max_conflicts: 10,
                },
            },
            // Constraints: When does it apply?
            GapTemplate {
                kind: GapKind::NEED_CONSTRAINTS,
                priority: 180,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Symbol(4), // "applies_when"
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0xFF,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DEPENDS_ON, EdgeType::PREVENTS],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 20,
                    max_io_bytes: 256 * 1024,
                    min_trust: 60,
                    max_conflicts: 5,
                },
            },
        ]
    }

    /// Templates for COMPARE intent
    fn compare_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Definitions of both entities
            GapTemplate {
                kind: GapKind::NEED_DEFINITION,
                priority: 220,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Symbol(1),
                    obj_tag: Some(ObjTag::SYM),
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::DEFINES, EdgeType::REFINES],
                    max_depth: 1,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 10,
                    max_io_bytes: 256 * 1024,
                    min_trust: 70,
                    max_conflicts: 2,
                },
            },
            // Facts about each entity
            GapTemplate {
                kind: GapKind::NEED_FACT,
                priority: 200,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::IMPLIES, EdgeType::SUPPORTS],
                    max_depth: 2,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 512 * 1024,
                    min_trust: 60,
                    max_conflicts: 10,
                },
            },
            // Comparison axes
            GapTemplate {
                kind: GapKind::NEED_COMPARISON_AXIS,
                priority: 180,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(5), // "comparable_on"
                    obj_tag: Some(ObjTag::SYM),
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::SAME_AS, EdgeType::GENERALIZES],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 20,
                    max_io_bytes: 256 * 1024,
                    min_trust: 50,
                    max_conflicts: 5,
                },
            },
        ]
    }

    /// Templates for DERIVE intent
    fn derive_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need causal chain for derivation
            GapTemplate {
                kind: GapKind::NEED_CAUSAL_CHAIN,
                priority: 255,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(6), // "derives"
                    obj_tag: None,
                    obj: PatternRefTemplate::Entity,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::IMPLIES, EdgeType::DERIVED_FROM],
                    max_depth: 3,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 75,
                    max_conflicts: 10,
                },
            },
            // Facts to derive from
            GapTemplate {
                kind: GapKind::NEED_FACT,
                priority: 220,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::SUPPORTS, EdgeType::CAUSES],
                    max_depth: 2,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 100,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 60,
                    max_conflicts: 15,
                },
            },
        ]
    }

    /// Templates for VERIFY intent
    fn verify_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need direct evidence
            GapTemplate {
                kind: GapKind::NEED_EVIDENCE,
                priority: 255,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Entity,
                    pred: PatternRefTemplate::Any,
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::SUPPORTS, EdgeType::DERIVED_FROM],
                    max_depth: 2,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 100,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 80,
                    max_conflicts: 5,
                },
            },
            // Counter-evidence
            GapTemplate {
                kind: GapKind::NEED_COUNTEREXAMPLE,
                priority: 220,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(8), // "contradicts"
                    obj_tag: None,
                    obj: PatternRefTemplate::Entity,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::CONTRADICTS],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 512 * 1024,
                    min_trust: 70,
                    max_conflicts: 5,
                },
            },
        ]
    }

    /// Templates for PLAN intent
    fn plan_templates() -> Vec<GapTemplate> {
        vec![
            // Primary: Need procedure
            GapTemplate {
                kind: GapKind::NEED_PROCEDURE,
                priority: 255,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(9), // "procedure_for"
                    obj_tag: None,
                    obj: PatternRefTemplate::Entity,
                    qualifiers_mask: 0,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::STEP_OF, EdgeType::INPUT_OF, EdgeType::OUTPUT_OF],
                    max_depth: 3,
                    fanout_limit: 128,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 50,
                    max_io_bytes: 1024 * 1024,
                    min_trust: 70,
                    max_conflicts: 10,
                },
            },
            // Constraints
            GapTemplate {
                kind: GapKind::NEED_CONSTRAINTS,
                priority: 180,
                pattern_template: ClaimPatternTemplate {
                    subj: PatternRefTemplate::Any,
                    pred: PatternRefTemplate::Symbol(4),
                    obj_tag: None,
                    obj: PatternRefTemplate::Any,
                    qualifiers_mask: 0xFF,
                },
                nav_template: NavHintTemplate {
                    seed_nodes: vec![],
                    edge_types: vec![EdgeType::PREVENTS, EdgeType::DEPENDS_ON],
                    max_depth: 2,
                    fanout_limit: 64,
                },
                stop_template: StopCondTemplate {
                    max_nodes: 20,
                    max_io_bytes: 256 * 1024,
                    min_trust: 60,
                    max_conflicts: 5,
                },
            },
        ]
    }
}

/// Extension methods for Gap
impl Gap {
    /// Set priority
    pub fn set_priority(&mut self, priority: u8) {
        self.priority = priority;
    }

    /// Set navigation hints
    pub fn set_nav(&mut self, nav: NavHint) {
        self.nav = nav;
    }

    /// Set stop conditions
    pub fn set_stop(&mut self, stop: StopCond) {
        self.stop = stop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_lookup_gaps() {
        let entities = vec![1, 2];
        let gaps = GapGenerator::generate(Intent::LOOKUP, &entities);

        assert!(!gaps.is_empty());
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_FACT));
        println!("Generated {} LOOKUP gaps", gaps.len());
    }

    #[test]
    fn test_generate_define_gaps() {
        let entities = vec![1];
        let gaps = GapGenerator::generate(Intent::DEFINE, &entities);

        assert!(!gaps.is_empty());
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_DEFINITION));
        assert!(gaps.iter().any(|g| g.priority > 200));
        println!("Generated {} DEFINE gaps", gaps.len());
    }

    #[test]
    fn test_generate_explain_gaps() {
        let entities = vec![1];
        let gaps = GapGenerator::generate(Intent::EXPLAIN, &entities);

        assert!(!gaps.is_empty());
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_CAUSAL_CHAIN));
        println!("Generated {} EXPLAIN gaps", gaps.len());
    }

    #[test]
    fn test_generate_compare_gaps() {
        let entities = vec![1, 2];
        let gaps = GapGenerator::generate(Intent::COMPARE, &entities);

        // Should generate gaps for both entities
        assert!(gaps.len() >= 4);
        println!("Generated {} COMPARE gaps", gaps.len());
    }

    #[test]
    fn test_gap_priorities() {
        let entities = vec![1];
        let gaps = GapGenerator::generate(Intent::DEFINE, &entities);

        // Should have high priority gaps
        let max_priority = gaps.iter().map(|g| g.priority).max().unwrap();
        assert!(max_priority >= 200);
    }

    #[test]
    fn test_empty_entities() {
        let gaps = GapGenerator::generate(Intent::LOOKUP, &[]);

        // Should still generate gaps with Any patterns
        assert!(!gaps.is_empty());
        println!("Generated {} gaps with no entities", gaps.len());
    }

    #[test]
    fn test_all_intents_have_templates() {
        let intents = vec![
            Intent::LOOKUP,
            Intent::DEFINE,
            Intent::EXPLAIN,
            Intent::COMPARE,
            Intent::DERIVE,
            Intent::VERIFY,
            Intent::PLAN,
        ];

        for intent in intents {
            let templates = GapGenerator::get_templates(intent);
            assert!(
                !templates.is_empty(),
                "Intent {:?} has no templates",
                intent
            );
        }
    }
}
