pub const MIN_CONFIDENCE_DEFAULT: f32 = 0.50;
pub const MAX_FANOUT_DEFAULT: usize = 50;
pub const MAX_EDGES_DEFAULT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExplainTraversal {
    pub min_confidence: f32,
    pub max_fanout: usize,
    pub max_edges: usize,
}

impl Default for ExplainTraversal {
    fn default() -> Self {
        Self {
            min_confidence: MIN_CONFIDENCE_DEFAULT,
            max_fanout: MAX_FANOUT_DEFAULT,
            max_edges: MAX_EDGES_DEFAULT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrettyConfidenceTier {
    Edit,
    Move,
    Related,
    Hidden,
    ForensicsOnly,
}

pub fn pretty_tier(confidence: f32, moved: bool, location_only: bool) -> PrettyConfidenceTier {
    if location_only {
        return PrettyConfidenceTier::ForensicsOnly;
    }
    if confidence >= 0.90 && !moved {
        PrettyConfidenceTier::Edit
    } else if confidence >= 0.85 && moved {
        PrettyConfidenceTier::Move
    } else if confidence >= MIN_CONFIDENCE_DEFAULT {
        PrettyConfidenceTier::Related
    } else {
        PrettyConfidenceTier::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_defaults_match_spec_values() {
        let defaults = ExplainTraversal::default();
        assert_eq!(defaults.min_confidence, 0.50);
        assert_eq!(defaults.max_fanout, 50);
        assert_eq!(defaults.max_edges, 500);
    }

    #[test]
    fn pretty_tiers_match_confidence_cutoffs() {
        assert_eq!(
            pretty_tier(0.95, false, false),
            PrettyConfidenceTier::Edit
        );
        assert_eq!(pretty_tier(0.85, true, false), PrettyConfidenceTier::Move);
        assert_eq!(
            pretty_tier(0.50, false, false),
            PrettyConfidenceTier::Related
        );
        assert_eq!(
            pretty_tier(0.49, false, false),
            PrettyConfidenceTier::Hidden
        );
        assert_eq!(
            pretty_tier(0.10, false, true),
            PrettyConfidenceTier::ForensicsOnly
        );
    }

    #[test]
    fn moved_vs_non_moved_classification() {
        assert_eq!(pretty_tier(0.85, true, false), PrettyConfidenceTier::Move);
        assert_eq!(
            pretty_tier(0.85, false, false),
            PrettyConfidenceTier::Related
        );
    }

    #[test]
    fn threshold_edges_are_classified_as_specified() {
        assert_eq!(pretty_tier(0.49, true, false), PrettyConfidenceTier::Hidden);
        assert_eq!(
            pretty_tier(0.50, true, false),
            PrettyConfidenceTier::Related
        );
        assert_eq!(
            pretty_tier(0.89, false, false),
            PrettyConfidenceTier::Related
        );
        assert_eq!(pretty_tier(0.90, true, false), PrettyConfidenceTier::Move);
        assert_eq!(
            pretty_tier(0.90, false, false),
            PrettyConfidenceTier::Edit
        );
    }

    #[test]
    fn location_only_always_forensics_even_for_high_confidence() {
        assert_eq!(
            pretty_tier(0.95, false, true),
            PrettyConfidenceTier::ForensicsOnly
        );
    }
}
