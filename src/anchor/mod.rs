use std::collections::HashSet;

pub mod winnow;

pub use winnow::{SpanAnchor, expand_winnow_anchor, fingerprint_similarity, fingerprint_text};

const WINDOW_LINES: usize = 24;
const WINDOW_OVERLAP_LINES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintedWindow {
    pub ordinal: u32,
    pub anchor: String,
    pub features: Vec<String>,
}

pub fn fingerprint_windows(text: &str) -> Vec<FingerprintedWindow> {
    line_windows(text, WINDOW_LINES, WINDOW_OVERLAP_LINES)
        .into_iter()
        .enumerate()
        .filter_map(|(ordinal, window)| {
            let anchor = fingerprint_text(&window).fingerprint;
            if anchor.is_empty() {
                return None;
            }
            let mut seen = HashSet::new();
            let features = expand_winnow_anchor(&anchor)
                .into_iter()
                .filter(|feature| seen.insert(feature.clone()))
                .collect();
            Some(FingerprintedWindow {
                ordinal: ordinal as u32,
                anchor,
                features,
            })
        })
        .collect()
}

pub fn fingerprint_anchor_hashes(text: &str) -> Vec<String> {
    fingerprint_windows(text)
        .into_iter()
        .map(|window| window.anchor)
        .collect()
}

pub fn fingerprint_window_hashes(text: &str) -> Vec<String> {
    fingerprint_anchor_hashes(text)
}

/// Return the ordered unique winnow features in `text`.
pub fn fingerprint_token_hashes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for window in fingerprint_windows(text) {
        for feature in window.features {
            if seen.insert(feature.clone()) {
                out.push(feature);
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
    use super::{fingerprint_anchor_hashes, fingerprint_window_hashes, fingerprint_windows};

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

    #[test]
    fn fingerprinted_windows_preserve_equal_physical_windows_and_ordinals() {
        let block = (1..=24)
            .map(|line| format!("fn repeated_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();
        let text = format!("{block}{block}");

        let windows = fingerprint_windows(&text);

        assert_eq!(windows.len(), 3);
        assert_eq!(
            windows
                .iter()
                .map(|window| window.ordinal)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(windows[0].anchor, windows[2].anchor);
        assert_eq!(windows[0].features, windows[2].features);
    }
}
