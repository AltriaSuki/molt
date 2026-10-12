//! Offline mean-pooled word embeddings from a user-provided, hash-pinned model.
//! No downloader, subprocess, API key or executable model format is involved.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{ensure, Context};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_MODEL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NOTES: usize = 10_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileModel {
    format: String,
    dimensions: usize,
    vectors: BTreeMap<String, Vec<f32>>,
}

#[derive(Debug)]
pub struct Model {
    hash: String,
    dimensions: usize,
    vectors: BTreeMap<String, Vec<f32>>,
    pub weight: f64,
    pub min_similarity: f64,
}

impl Model {
    pub fn load(path: &Path, expected_sha256: &str, weight: f64, min_similarity: f64) -> anyhow::Result<Self> {
        ensure!(cfg!(feature = "semantic"), "rebuild molt-memory with the semantic feature to enable local embeddings");
        ensure!(
            expected_sha256.len() == 64 && expected_sha256.bytes().all(|c| c.is_ascii_hexdigit()),
            "model needs a pinned SHA-256"
        );
        ensure!(weight.is_finite() && (0.0..=1.0).contains(&weight), "semantic weight must be 0 to 1");
        ensure!(
            min_similarity.is_finite() && (0.0..=1.0).contains(&min_similarity),
            "minimum similarity must be 0 to 1"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.take(MAX_MODEL_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_MODEL_BYTES, "embedding model exceeds 64 MiB");
        let hash = hex::encode(Sha256::digest(&bytes));
        ensure!(
            hash.eq_ignore_ascii_case(expected_sha256),
            "embedding model SHA-256 does not match; refusing to load it"
        );
        let file: FileModel = serde_json::from_slice(&bytes).context("embedding model JSON")?;
        ensure!(file.format == "mean-word-vectors-v1", "unsupported embedding model format");
        ensure!(
            (1..=1024).contains(&file.dimensions) && !file.vectors.is_empty() && file.vectors.len() <= 200_000,
            "invalid embedding dimensions or vocabulary size"
        );
        for (word, vector) in &file.vectors {
            ensure!(
                !word.is_empty() && word.len() <= 256 && word.to_lowercase() == *word,
                "vocabulary must use lowercase words"
            );
            ensure!(
                vector.len() == file.dimensions && vector.iter().all(|v| v.is_finite()),
                "invalid word vector for {word}"
            );
        }
        Ok(Self { hash, dimensions: file.dimensions, vectors: file.vectors, weight, min_similarity })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let mut vector = vec![0.0f64; self.dimensions];
        for word in text.split(|c: char| !c.is_alphanumeric() && c != '_').take(1024) {
            if let Some(v) = self.vectors.get(&word.to_lowercase()) {
                for (a, b) in vector.iter_mut().zip(v) {
                    *a += f64::from(*b);
                }
            }
        }
        let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
        (norm.is_finite() && norm > 0.0).then(|| vector.iter().map(|v| (v / norm) as f32).collect())
    }

    /// Cache is derived data, keyed by model and text hashes. Corrections and
    /// retractions are filtered from live notes before any vector is used.
    pub(crate) fn cached(&self, conn: &Connection, id: &str, text: &str) -> rusqlite::Result<Option<Vec<f32>>> {
        let text_hash = hex::encode(Sha256::digest(text.as_bytes()));
        let old: Option<String> = conn
            .query_row(
                "SELECT vector FROM note_vectors WHERE note = ?1 AND model = ?2 AND text_hash = ?3",
                params![id, self.hash, text_hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            if let Ok(v) = serde_json::from_str::<Option<Vec<f32>>>(&old) {
                if v.as_ref().is_none_or(|v| v.len() == self.dimensions && v.iter().all(|v| v.is_finite())) {
                    return Ok(v);
                }
            }
        }
        let v = self.embed(text);
        conn.execute("INSERT INTO note_vectors VALUES (?1, ?2, ?3, ?4) ON CONFLICT(note, model) DO UPDATE SET text_hash=excluded.text_hash, vector=excluded.vector", params![id, self.hash, text_hash, serde_json::to_string(&v).expect("finite vectors serialize")])?;
        Ok(v)
    }

    pub(crate) fn sync(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS note_vectors (note TEXT NOT NULL, model TEXT NOT NULL, text_hash TEXT NOT NULL, vector TEXT NOT NULL, PRIMARY KEY(note, model));")?;
        let mut stmt = conn.prepare("SELECT id, text FROM notes WHERE forgotten_ms IS NULL ORDER BY id LIMIT 10001")?;
        let notes: Vec<(String, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        if notes.len() > MAX_NOTES {
            return Err(rusqlite::Error::InvalidParameterName(
                "semantic recall supports at most 10000 live notes; narrow the database or disable it".into(),
            ));
        }
        for (id, text) in notes {
            self.cached(conn, &id, &text)?;
        }
        Ok(())
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(a, b)| f64::from(*a) * f64::from(*b)).sum::<f64>().clamp(-1.0, 1.0)
}

#[cfg(all(test, feature = "semantic"))]
mod tests {
    use super::*;
    #[test]
    fn synonyms_are_recalled_with_scores_and_snapshots_survive_forgetting() {
        use crate::notes::{self, NewNote};
        use molt_api::memory::{NoteKind, Provenance, RecallRequest};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.json");
        let bytes = br#"{"format":"mean-word-vectors-v1","dimensions":2,"vectors":{"automobile":[1,0],"car":[1,0],"database":[0,1]}}"#;
        std::fs::write(&path, bytes).unwrap();
        let hash = hex::encode(Sha256::digest(bytes));
        let mut db = crate::Db::in_memory().unwrap();
        let note = NewNote {
            kind: NoteKind::Fact,
            text: "Use the automobile guide.".into(),
            workspace: Some(dir.path().to_str().unwrap().into()),
            confidence: 0.8,
            provenance: Provenance {
                trace: "source".into(),
                events: vec![],
                service: "cli".into(),
                version: "user".into(),
            },
        };
        let (note, _) = notes::remember(&db, note, 1).unwrap();
        let req = RecallRequest {
            query: "car".into(),
            workspace: note.workspace.clone(),
            capture: Some("context".into()),
            ..Default::default()
        };
        assert!(crate::review::recall(&db, &req, Some(("before", "c0")), 2).unwrap().is_empty());
        db.enable_semantic(Model::load(&path, &hash, 0.0, 0.4).unwrap()).unwrap();
        assert!(crate::review::recall(&db, &req, Some(("zero", "zero")), 2).unwrap().is_empty());
        db.enable_semantic(Model::load(&path, &hash, 0.5, 0.4).unwrap()).unwrap();
        let found = crate::review::recall(&db, &req, Some(("after", "c1")), 3).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].note.id, note.id);
        assert!(found[0].reason.similarity.unwrap() > 0.99);
        assert_eq!(found[0].reason.model_sha256.as_deref(), Some(hash.as_str()));
        assert_eq!(found[0].reason.relevance, Some(0.0));
        let other = RecallRequest { workspace: Some("/another".into()), capture: None, ..req.clone() };
        assert!(crate::recall(&db, &other).unwrap().is_empty());
        let before = crate::recall_snapshots(&db, req.workspace.as_deref().unwrap(), "after").unwrap();
        notes::forget(&db, &note.id, "no longer true", "forget", 4).unwrap();
        assert!(crate::review::recall(&db, &req, Some(("next", "c2")), 5).unwrap().is_empty());
        assert_eq!(crate::recall_snapshots(&db, req.workspace.as_deref().unwrap(), "after").unwrap(), before);
    }

    #[test]
    fn pinned_models_retrieve_synonyms_and_reject_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.json");
        let bytes = br#"{"format":"mean-word-vectors-v1","dimensions":2,"vectors":{"automobile":[1,0],"car":[1,0],"database":[0,1]}}"#;
        std::fs::write(&path, bytes).unwrap();
        let hash = hex::encode(Sha256::digest(bytes));
        let model = Model::load(&path, &hash, 0.5, 0.4).unwrap();
        assert!(cosine(&model.embed("car").unwrap(), &model.embed("automobile").unwrap()) > 0.99);
        assert_eq!(model.embed("unknown"), None);
        std::fs::write(&path, b"{}").unwrap();
        assert!(Model::load(&path, &hash, 0.5, 0.4).unwrap_err().to_string().contains("SHA-256"));
    }
}
