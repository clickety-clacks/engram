pub mod winnow;

pub use winnow::{SpanAnchor, fingerprint_similarity, fingerprint_text};

pub fn fingerprint_anchor_hashes(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![fingerprint_text(text).fingerprint]
    }
}
