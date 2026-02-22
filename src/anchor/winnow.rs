use std::collections::HashSet;

pub const DEFAULT_K_GRAM: usize = 5;
pub const DEFAULT_WINDOW: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpanAnchor {
    pub fingerprint: String,
}

pub fn fingerprint_text(text: &str) -> SpanAnchor {
    let tokens = tokenize(text);
    let features = winnowed_features(&tokens, DEFAULT_K_GRAM, DEFAULT_WINDOW);
    let fingerprint = if features.is_empty() {
        format!("fallback:{}", hash_str(text))
    } else {
        let joined = features
            .iter()
            .map(|h| format!("{h:016x}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("winnow:{}", joined)
    };

    SpanAnchor { fingerprint }
}

pub fn fingerprint_similarity(left: &str, right: &str) -> Option<f32> {
    let left_features = parse_fingerprint(left)?;
    let right_features = parse_fingerprint(right)?;

    if left_features.is_empty() || right_features.is_empty() {
        return None;
    }

    let intersection = left_features.intersection(&right_features).count() as f32;
    let union = left_features.union(&right_features).count() as f32;
    if union == 0.0 {
        None
    } else {
        Some(intersection / union)
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
            continue;
        }

        if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        if !ch.is_whitespace() {
            tokens.push(ch.to_string());
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn winnowed_features(tokens: &[String], k: usize, window: usize) -> Vec<u64> {
    if tokens.is_empty() || k == 0 || window == 0 {
        return Vec::new();
    }

    let kgrams = kgram_hashes(tokens, k);
    if kgrams.is_empty() {
        return Vec::new();
    }

    if kgrams.len() <= window {
        let mut uniq = Vec::new();
        let mut seen = HashSet::new();
        for hash in kgrams {
            if seen.insert(hash) {
                uniq.push(hash);
            }
        }
        return uniq;
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for start in 0..=(kgrams.len() - window) {
        let window_slice = &kgrams[start..start + window];
        let mut min_pos = 0usize;
        let mut min_value = window_slice[0];

        for (idx, value) in window_slice.iter().enumerate().skip(1) {
            if *value <= min_value {
                min_value = *value;
                min_pos = idx;
            }
        }

        let selected = window_slice[min_pos];
        if seen.insert(selected) {
            out.push(selected);
        }
    }

    out
}

fn kgram_hashes(tokens: &[String], k: usize) -> Vec<u64> {
    if tokens.len() < k {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(tokens.len() - k + 1);
    for start in 0..=(tokens.len() - k) {
        let kgram = tokens[start..start + k].join("\x1f");
        out.push(hash_str(&kgram));
    }
    out
}

fn hash_str(input: &str) -> u64 {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let bytes: [u8; 8] = digest[0..8]
        .try_into()
        .expect("sha256 digest has at least 8 bytes");
    u64::from_be_bytes(bytes)
}

fn parse_fingerprint(raw: &str) -> Option<HashSet<u64>> {
    if let Some(rest) = raw.strip_prefix("winnow:") {
        let values = rest
            .split(',')
            .filter(|value| !value.is_empty())
            .map(|value| u64::from_str_radix(value, 16).ok())
            .collect::<Option<Vec<_>>>()?;
        return Some(values.into_iter().collect());
    }

    if let Some(rest) = raw.strip_prefix("fallback:") {
        return Some([rest.parse::<u64>().ok()?].into_iter().collect());
    }

    if raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        let legacy = u64::from_str_radix(&raw[..16], 16).ok()?;
        return Some([legacy].into_iter().collect());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{fingerprint_similarity, fingerprint_text};

    #[test]
    fn fingerprints_are_stable_for_same_input() {
        let a = fingerprint_text("fn main() {}\n");
        let b = fingerprint_text("fn main() {}\n");
        assert_eq!(a, b);
    }

    #[test]
    fn similarity_survives_small_neighboring_edits() {
        let left = fingerprint_text("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        let right = fingerprint_text(
            "fn add(a: i32, b: i32) -> i32 {\n    // small comment\n    a + b\n}\n",
        );

        let score = fingerprint_similarity(&left.fingerprint, &right.fingerprint)
            .expect("similarity should compute");
        assert!(score > 0.0);
    }
}
