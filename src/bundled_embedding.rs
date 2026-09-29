use std::io::Read;
use std::sync::OnceLock;

use model2vec_rs::model::StaticModel;
use rayon::prelude::*;

use crate::embedding::EmbeddingProvider;
use crate::errors::EmbeddingError;

const MAGIC: &[u8; 8] = b"WNKEMB01";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 32;
const MAX_UNPACKED_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MODEL_BYTES: &[u8] =
    include_bytes!("../assets/models/bundled-embedding-v1.bin.zst");

static MODEL: OnceLock<Result<BundledModel, String>> = OnceLock::new();

pub struct BundledProvider;

pub(crate) struct BundledModel {
    inner: StaticModel,
}

impl BundledProvider {
    fn model() -> Result<&'static BundledModel, EmbeddingError> {
        MODEL
            .get_or_init(|| decode_model(MODEL_BYTES).map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|message| EmbeddingError::ProviderUnavailable(message.clone()))
    }
}

impl EmbeddingProvider for BundledProvider {
    fn name(&self) -> &str {
        "bundled"
    }

    fn dim(&self) -> usize {
        256
    }

    fn embed_batch(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let model = Self::model()?;
        Ok(chunks
            .par_iter()
            .map(|chunk| model.inner.encode_single(chunk))
            .collect())
    }
}

fn unavailable(message: impl Into<String>) -> EmbeddingError {
    EmbeddingError::ProviderUnavailable(format!(
        "invalid bundled embedding artifact: {}",
        message.into()
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, EmbeddingError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| unavailable("truncated header"))?;
    Ok(u32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| unavailable("invalid integer field"))?,
    ))
}

fn checked_slice(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u8>, EmbeddingError> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| unavailable("section length overflow"))?;
    let section = bytes
        .get(*cursor..end)
        .ok_or_else(|| unavailable("truncated section"))?
        .to_vec();
    *cursor = end;
    Ok(section)
}

fn safetensors_bytes(
    rows: usize,
    dim: usize,
    scales: &[u8],
    packed: &[u8],
) -> Result<Vec<u8>, EmbeddingError> {
    let value_count = rows
        .checked_mul(dim)
        .ok_or_else(|| unavailable("embedding shape overflow"))?;
    let expected_packed = value_count.div_ceil(2);
    if packed.len() != expected_packed {
        return Err(unavailable("quantized weight length mismatch"));
    }
    if scales.len() != rows.saturating_mul(4) {
        return Err(unavailable("scale length mismatch"));
    }

    let mut quantized = Vec::with_capacity(value_count);
    for &byte in packed {
        for nibble in [byte & 0x0f, byte >> 4] {
            if quantized.len() == value_count {
                break;
            }
            quantized.push(if nibble & 0x08 == 0 {
                nibble
            } else {
                nibble | 0xf0
            });
        }
    }

    let weights_start = quantized.len();
    let weights_end = weights_start
        .checked_add(scales.len())
        .ok_or_else(|| unavailable("safetensors length overflow"))?;
    let mut header = serde_json::to_vec(&serde_json::json!({
        "embeddings": {
            "dtype": "I8",
            "shape": [rows, dim],
            "data_offsets": [0, weights_start]
        },
        "weights": {
            "dtype": "F32",
            "shape": [rows],
            "data_offsets": [weights_start, weights_end]
        }
    }))
    .map_err(|error| unavailable(error.to_string()))?;
    while header.len() % 8 != 0 {
        header.push(b' ');
    }

    let header_len =
        u64::try_from(header.len()).map_err(|_| unavailable("safetensors header too large"))?;
    let mut output = Vec::with_capacity(8 + header.len() + quantized.len() + scales.len());
    output.extend_from_slice(&header_len.to_le_bytes());
    output.extend_from_slice(&header);
    output.extend_from_slice(&quantized);
    output.extend_from_slice(scales);
    Ok(output)
}

pub(crate) fn decode_model(compressed: &[u8]) -> Result<BundledModel, EmbeddingError> {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|error| unavailable(error.to_string()))?;
    let mut unpacked = Vec::new();
    decoder
        .take(MAX_UNPACKED_BYTES + 1)
        .read_to_end(&mut unpacked)
        .map_err(|error| unavailable(error.to_string()))?;
    if unpacked.len() as u64 > MAX_UNPACKED_BYTES {
        return Err(unavailable("unpacked artifact exceeds safety limit"));
    }
    if unpacked.get(..8) != Some(MAGIC) {
        return Err(unavailable("bad magic"));
    }
    let version = read_u32(&unpacked, 8)?;
    if version != VERSION {
        return Err(unavailable(format!("unsupported version {version}")));
    }
    let rows = read_u32(&unpacked, 12)? as usize;
    let dim = read_u32(&unpacked, 16)? as usize;
    let config_len = read_u32(&unpacked, 20)? as usize;
    let tokenizer_len = read_u32(&unpacked, 24)? as usize;
    let weights_len = read_u32(&unpacked, 28)? as usize;
    if rows == 0 || dim == 0 || dim > 4096 {
        return Err(unavailable("invalid model shape"));
    }

    let mut cursor = HEADER_BYTES;
    let config = checked_slice(&unpacked, &mut cursor, config_len)?;
    let tokenizer = checked_slice(&unpacked, &mut cursor, tokenizer_len)?;
    let scale_len = rows
        .checked_mul(4)
        .ok_or_else(|| unavailable("scale length overflow"))?;
    let scales = checked_slice(&unpacked, &mut cursor, scale_len)?;
    let weights = checked_slice(&unpacked, &mut cursor, weights_len)?;
    if cursor != unpacked.len() {
        return Err(unavailable("trailing bytes"));
    }

    let safetensors = safetensors_bytes(rows, dim, &scales, &weights)?;
    let inner = StaticModel::from_bytes(tokenizer, safetensors, config, Some(true))
        .map_err(|error| unavailable(error.to_string()))?;
    Ok(BundledModel { inner })
}

#[cfg(test)]
mod tests {
    use crate::embedding::EmbeddingProvider;

    use super::{MODEL_BYTES, decode_model};

    #[test]
    fn bundled_artifact_decodes_with_expected_shape() {
        let model = decode_model(MODEL_BYTES).expect("checked-in artifact should decode");
        let embedding = model.inner.encode_single("authentication");
        assert_eq!(embedding.len(), 256);
        assert!(embedding.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn malformed_artifacts_fail_without_panicking() {
        for bytes in [
            &[][..],
            &MODEL_BYTES[..MODEL_BYTES.len() / 2],
            b"not a zstd frame",
        ] {
            assert!(decode_model(bytes).is_err());
        }
    }

    #[test]
    fn oversized_artifact_is_rejected_before_parsing() {
        let oversized = vec![0_u8; super::MAX_UNPACKED_BYTES as usize + 1];
        let compressed = zstd::stream::encode_all(oversized.as_slice(), 1).unwrap();
        let error = decode_model(&compressed).err().expect("oversized artifact");
        assert!(error.to_string().contains("exceeds safety limit"));
    }

    #[test]
    fn unknown_artifact_version_is_rejected() {
        let mut unpacked = zstd::stream::decode_all(MODEL_BYTES).unwrap();
        unpacked[8..12].copy_from_slice(&99_u32.to_le_bytes());
        let compressed = zstd::stream::encode_all(unpacked.as_slice(), 1).unwrap();
        let error = decode_model(&compressed).err().expect("unknown version");
        assert!(error.to_string().contains("unsupported version 99"));
    }

    #[test]
    fn bundled_embedding_matches_pack_time_reference() {
        let provider = super::BundledProvider;
        let embedding = provider
            .embed_single("File: src/auth.rs\n---\nfn authenticate_user(token: &str) -> Session")
            .unwrap();
        let expected = [
            -0.13708343,
            0.029942058,
            0.07863623,
            -0.021658354,
            -0.040669404,
            0.085410886,
            0.0480718,
            -0.030761322,
            -0.08652144,
            0.05469608,
            -0.11585529,
            -0.10945082,
        ];
        for (actual, expected) in embedding.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-5, "{actual} != {expected}");
        }
    }

    #[test]
    fn batch_embedding_is_finite_normalized_and_ordered() {
        let provider = super::BundledProvider;
        assert!(provider.embed_batch(&[]).unwrap().is_empty());
        let texts = vec![
            "authentication session token".to_string(),
            "quicksort pivot partition".to_string(),
            "matrix determinant inverse".to_string(),
        ];
        let first = provider.embed_batch(&texts).unwrap();
        let second = provider.embed_batch(&texts).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), texts.len());
        for vector in &first {
            assert_eq!(vector.len(), 256);
            assert!(vector.iter().all(|value| value.is_finite()));
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5);
        }
        assert_eq!(
            first[0],
            provider.embed_single(&texts[0]).expect("single embedding")
        );
    }

    #[test]
    fn authentication_query_prefers_authentication_code() {
        let provider = super::BundledProvider;
        let query = provider.embed_single("authentication").unwrap();
        let candidates = provider
            .embed_batch(&[
                "fn authenticate_user(token: &str) -> Session".to_string(),
                "fn quicksort(values: &mut [i32])".to_string(),
                "fn determinant(matrix: Matrix) -> f64".to_string(),
            ])
            .unwrap();
        let scores: Vec<f32> = candidates
            .iter()
            .map(|candidate| {
                query
                    .iter()
                    .zip(candidate)
                    .map(|(left, right)| left * right)
                    .sum()
            })
            .collect();
        assert!(scores[0] > scores[1], "{scores:?}");
        assert!(scores[0] > scores[2], "{scores:?}");
    }
}
