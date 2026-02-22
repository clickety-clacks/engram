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
