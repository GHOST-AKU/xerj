//! The exact chunk + embed + pool recipe `semantic_text` uses with the
//! built-in lexical embedder.
//!
//! It lives here, not in the engine, so the stored-section codec can rebuild
//! the derived vectors from the text instead of persisting them in every
//! stored `_source` (#1122, #2433). Ingest and regeneration both call these
//! functions, so a regenerated vector is bit-identical to the ingested one.
//! The codec only drops a vector after re-deriving it and comparing bits.
//! Changing anything here changes what old stripped segments rehydrate to,
//! so bump `stored_codec::derived_vectors::MARKER_LEXICAL_V1` if you do.

use crate::chunker::TextChunker;
use crate::local::local_embed;

/// Default chunk size (characters) for auto-embed-on-ingest chunking.
/// `EmbeddingConfig` has no per-field chunk parameter yet, so this is a
/// sensible fixed default (roughly a paragraph).
pub const SEMANTIC_CHUNK_SIZE: usize = 512;
/// Default overlap (characters) between consecutive chunks.
pub const SEMANTIC_CHUNK_OVERLAP: usize = 64;

/// The chunker used by the auto-embed-on-ingest path.
pub fn semantic_chunker() -> TextChunker {
    TextChunker::new(SEMANTIC_CHUNK_SIZE, SEMANTIC_CHUNK_OVERLAP)
}

/// L2-normalize each input vector, mean-pool across all of them, then
/// re-normalize the pooled result. Produces a unit-norm vector of the same
/// dimensionality as the inputs (empty input => empty vec). Vectors whose
/// length differs from the first are skipped.
pub fn mean_pool_normalize(vectors: &[Vec<f32>]) -> Vec<f32> {
    let dims = match vectors.first() {
        Some(v) => v.len(),
        None => return Vec::new(),
    };
    let mut acc = vec![0.0f32; dims];
    let mut used = 0usize;
    for v in vectors {
        if v.len() != dims {
            continue;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for (a, x) in acc.iter_mut().zip(v.iter()) {
                *a += x / norm;
            }
            used += 1;
        }
    }
    if used > 0 {
        let inv = 1.0 / used as f32;
        for a in acc.iter_mut() {
            *a *= inv;
        }
    }
    // Re-normalize the pooled vector to unit length.
    let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for a in acc.iter_mut() {
            *a /= norm;
        }
    }
    acc
}

/// Chunk `text` and embed each overlapping chunk with the built-in *local*
/// embedder, returning one unit-norm vector per passage. Short text
/// (<= chunk_size) produces a single chunk, so the result is
/// `vec![local_embed(text, dims)]`.
pub fn local_chunk_vectors(text: &str, dims: usize) -> Vec<Vec<f32>> {
    let chunks = semantic_chunker().chunk(text, None);
    if chunks.len() <= 1 {
        return vec![local_embed(text, dims)];
    }
    chunks.iter().map(|c| local_embed(&c.text, dims)).collect()
}

/// A pooled vector plus, for multi-passage text, the per-passage vectors.
pub type LexicalSemanticVectors = (Vec<f32>, Option<Vec<Vec<f32>>>);

/// The vectors ingest persists for one lexical `semantic_text` value: the
/// pooled vector, plus the per-passage vectors when there is more than one
/// passage. Mirrors `apply_semantic_embeddings_batch`: one passage is used
/// as-is, several are mean-pooled and re-normalized. `None` for `dims == 0`.
pub fn lexical_semantic_vectors(text: &str, dims: usize) -> Option<LexicalSemanticVectors> {
    if dims == 0 {
        return None;
    }
    let mut chunks: Vec<Vec<f32>> = local_chunk_vectors(text, dims)
        .into_iter()
        .filter(|v| !v.is_empty())
        .collect();
    match chunks.len() {
        0 => None,
        1 => chunks.pop().map(|pooled| (pooled, None)),
        _ => {
            let pooled = mean_pool_normalize(&chunks);
            Some((pooled, Some(chunks)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_unpooled_passage() {
        let (pooled, chunks) = lexical_semantic_vectors("a short passage", 16).unwrap();
        assert!(chunks.is_none());
        assert_eq!(pooled, local_embed("a short passage", 16));
    }

    #[test]
    fn long_text_pools_its_passages() {
        let text = "lorem ipsum dolor sit amet ".repeat(60);
        let (pooled, chunks) = lexical_semantic_vectors(&text, 32).unwrap();
        let chunks = chunks.expect("long text has several passages");
        assert!(chunks.len() > 1);
        assert_eq!(pooled, mean_pool_normalize(&chunks));
        assert!(lexical_semantic_vectors(&text, 0).is_none());
    }
}
