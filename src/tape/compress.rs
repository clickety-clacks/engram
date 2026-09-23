#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionFormat {
    Zstd,
}

pub fn compress_jsonl(input: &str) -> std::io::Result<Vec<u8>> {
    zstd::stream::encode_all(input.as_bytes(), 0)
}

pub fn decompress_jsonl(input: &[u8]) -> std::io::Result<String> {
    let decompressed = zstd::stream::decode_all(input)?;
    String::from_utf8(decompressed)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

pub fn decompress_jsonl_with_limit(input: &[u8], max_bytes: u64) -> std::io::Result<String> {
    use std::io::Read;

    let decoder = zstd::stream::read::Decoder::new(input)?;
    let mut decompressed = Vec::new();
    decoder
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut decompressed)?;
    if decompressed.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("decompressed tape exceeds {max_bytes} byte limit"),
        ));
    }
    String::from_utf8(decompressed)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{compress_jsonl, decompress_jsonl_with_limit};

    #[test]
    fn bounded_decompression_accepts_at_limit_and_rejects_over_limit() {
        let compressed = compress_jsonl("abc").expect("compress");
        assert_eq!(decompress_jsonl_with_limit(&compressed, 3).unwrap(), "abc");
        assert!(decompress_jsonl_with_limit(&compressed, 2).is_err());
    }
}
