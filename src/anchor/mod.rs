use std::collections::HashSet;

pub mod winnow;

pub use winnow::{SpanAnchor, expand_winnow_anchor, fingerprint_similarity, fingerprint_text};

const WINDOW_LINES: usize = 24;
const WINDOW_OVERLAP_LINES: usize = 12;

pub fn fingerprint_anchor_hashes(text: &str) -> Vec<String> {
    collect_window_anchors(text, |window| {
        let fingerprint = fingerprint_text(window).fingerprint;
        if fingerprint.is_empty() {
            Vec::new()
        } else {
            vec![fingerprint]
        }
    })
}

pub fn fingerprint_window_hashes(text: &str) -> Vec<String> {
    fingerprint_anchor_hashes(text)
}

/// Return individual winnow hash tokens for `text`, using the same overlapping
/// window strategy as [`fingerprint_anchor_hashes`] but expanding each window
/// fingerprint into one entry per hash token.
///
/// Suitable for storing as individual `evidence` rows so that each token can
/// be looked up via an exact-equality index scan.
pub fn fingerprint_token_hashes(text: &str) -> Vec<String> {
    collect_window_anchors(text, |window| {
        let fingerprint = fingerprint_text(window).fingerprint;
        if fingerprint.is_empty() {
            Vec::new()
        } else {
            expand_winnow_anchor(&fingerprint)
        }
    })
}

fn collect_window_anchors<F>(text: &str, anchors_for_window: F) -> Vec<String>
where
    F: Fn(&str) -> Vec<String>,
{
    if text.is_empty() {
        return Vec::new();
    }

    let windows = line_windows(text, WINDOW_LINES, WINDOW_OVERLAP_LINES);
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for window in windows {
        for anchor in anchors_for_window(&window) {
            if !anchor.is_empty() && seen.insert(anchor.clone()) {
                out.push(anchor);
            }
        }
    }

    out
}

fn line_windows(text: &str, window_lines: usize, overlap_lines: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if window_lines == 0 {
        return vec![text.to_string()];
    }

    let lines = split_lines_preserving_terminators(text);
    if lines.is_empty() || lines.len() <= window_lines {
        return vec![text.to_string()];
    }

    let step = window_lines.saturating_sub(overlap_lines).max(1);
    let mut out = Vec::new();
    let mut start = 0usize;

    loop {
        let end = (start + window_lines).min(lines.len());
        out.push(lines[start..end].concat());
        if end == lines.len() {
            break;
        }
        start = start.saturating_add(step);
    }

    out
}

fn split_lines_preserving_terminators(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(ToOwned::to_owned).collect();
    if lines.is_empty() {
        lines.push(text.to_string());
    } else {
        let trailing_newline_bytes: usize = lines.iter().map(String::len).sum();
        if trailing_newline_bytes < text.len() {
            lines.push(text[trailing_newline_bytes..].to_string());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{fingerprint_anchor_hashes, fingerprint_window_hashes};

    #[test]
    fn short_text_emits_window_anchor() {
        let anchors = fingerprint_anchor_hashes("fn main() {\n    println!(\"hi\");\n}\n");
        assert!(!anchors.is_empty());
        assert!(anchors.iter().all(|anchor| anchor.starts_with("winnow:")));
    }

    #[test]
    fn long_text_emits_overlapping_window_anchors() {
        let text = (1..=72)
            .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();

        let anchors = fingerprint_anchor_hashes(&text);
        assert!(anchors.len() >= 3, "anchors={anchors:?}");
    }

    #[test]
    fn window_hashes_preserve_legacy_full_fingerprints() {
        let text = (1..=72)
            .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();

        let window_hashes = fingerprint_window_hashes(&text);
        assert!(window_hashes.len() >= 3, "hashes={window_hashes:?}");
        assert!(window_hashes.iter().all(|anchor| anchor.contains(',')));
    }

    #[test]
    fn large_file_produces_window_scale_anchor_count() {
        let text = (1..=1914)
            .map(|line| format!("fn line_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();

        let anchors = fingerprint_anchor_hashes(&text);
        assert!(
            (100..=200).contains(&anchors.len()),
            "expected window-scale anchor count, got {}",
            anchors.len()
        );
    }
}
