//! modbit-effects — the protected-effect receipt ledger (Phase 5, docs/23
//! § Effect ledger; canonical owner crate per docs/81). Each receipt
//! hash-links to the previous one and binds the approval, capability,
//! call and result digests — tamper, delete and reorder are detectable.
//!
//! `Chain` is the pure logic (promoted from the checkpoint prototype to
//! the canonical owner); `Ledger` persists the chain as an append-only
//! JSONL file so receipts survive Core restarts. The ledger is written
//! on every PROTECTED effect (writes and external calls) — read-only
//! effects are not receipts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// One immutable receipt in the protected-effect chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectReceipt {
    pub receipt_id: String,
    pub prev_receipt_hash: String,
    /// Bound digests: approval, capability, call, result.
    pub approval_digest: String,
    pub capability_digest: String,
    pub call_digest: String,
    pub result_digest: String,
    /// Chain hash = H(prev + all bound digests).
    pub chain_hash: String,
}

fn chain_hash(prev: &str, approval: &str, capability: &str, call: &str, result: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev.as_bytes());
    hasher.update(approval.as_bytes());
    hasher.update(capability.as_bytes());
    hasher.update(call.as_bytes());
    hasher.update(result.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The in-memory chain (pure logic; used by the persistent ledger and by
/// tests of the hash discipline).
#[derive(Default)]
pub struct Chain {
    receipts: Vec<EffectReceipt>,
}

impl Chain {
    /// Appends a receipt bound to approval/capability/call/result digests.
    pub fn append(
        &mut self,
        approval_digest: &str,
        capability_digest: &str,
        call_digest: &str,
        result_digest: &str,
    ) -> EffectReceipt {
        let prev = self
            .receipts
            .last()
            .map(|r| r.chain_hash.clone())
            .unwrap_or_else(|| "GENESIS".to_string());
        let chain_hash = chain_hash(
            &prev,
            approval_digest,
            capability_digest,
            call_digest,
            result_digest,
        );
        let receipt = EffectReceipt {
            receipt_id: format!("receipt-{}", &chain_hash[..12]),
            prev_receipt_hash: prev,
            approval_digest: approval_digest.to_string(),
            capability_digest: capability_digest.to_string(),
            call_digest: call_digest.to_string(),
            result_digest: result_digest.to_string(),
            chain_hash,
        };
        self.receipts.push(receipt.clone());
        receipt
    }

    /// Verifies the full chain: hash links intact, order intact. Any
    /// tamper, delete or reorder breaks a link.
    pub fn verify(&self) -> Result<(), String> {
        let mut prev = "GENESIS".to_string();
        for (index, receipt) in self.receipts.iter().enumerate() {
            if receipt.prev_receipt_hash != prev {
                return Err(format!(
                    "chain broken at receipt {index}: link hash mismatch (tamper/delete/reorder)"
                ));
            }
            let expected = chain_hash(
                &receipt.prev_receipt_hash,
                &receipt.approval_digest,
                &receipt.capability_digest,
                &receipt.call_digest,
                &receipt.result_digest,
            );
            if receipt.chain_hash != expected {
                return Err(format!("receipt {index}: chain hash mismatch (tampered)"));
            }
            prev = receipt.chain_hash.clone();
        }
        Ok(())
    }

    pub fn receipts(&self) -> &[EffectReceipt] {
        &self.receipts
    }

    pub fn last_hash(&self) -> String {
        self.receipts
            .last()
            .map(|r| r.chain_hash.clone())
            .unwrap_or_else(|| "GENESIS".to_string())
    }
}

/// The append-only JSONL ledger: one receipt per line, durable across
/// restarts. Loads the existing chain (if any) so the hash continues.
pub struct Ledger {
    path: PathBuf,
    chain: Chain,
}

impl Ledger {
    /// Opens (or creates) the ledger at `path`, loading prior receipts.
    /// A pre-existing ledger must verify before it is extended.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut chain = Chain::default();
        if path.exists() {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(|e| e.to_string())?;
                if line.trim().is_empty() {
                    continue;
                }
                let receipt: EffectReceipt =
                    serde_json::from_str(&line).map_err(|e| format!("corrupt receipt line: {e}"))?;
                chain.receipts.push(receipt);
            }
            chain.verify()?;
        }
        Ok(Ledger { path: path.to_path_buf(), chain })
    }

    /// Appends one receipt and flushes it to disk before returning.
    pub fn append(
        &mut self,
        approval_digest: &str,
        capability_digest: &str,
        call_digest: &str,
        result_digest: &str,
    ) -> Result<EffectReceipt, String> {
        let receipt = self
            .chain
            .append(approval_digest, capability_digest, call_digest, result_digest);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        let mut line = serde_json::to_string(&receipt).map_err(|e| e.to_string())?;
        line.push('\n');
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        Ok(receipt)
    }

    /// Verifies the ENTIRE on-disk ledger (re-read from disk so a tamper
    /// in the file itself is caught, not just in-memory drift).
    pub fn verify_on_disk(&self) -> Result<(), String> {
        Self::verify_file(&self.path)
    }

    /// Verifies any ledger file.
    pub fn verify_file(path: &Path) -> Result<(), String> {
        let ledger = Self::open(path)?;
        ledger.chain.verify()
    }

    pub fn receipts(&self) -> &[EffectReceipt] {
        self.chain.receipts()
    }

    pub fn last_hash(&self) -> String {
        self.chain.last_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fx-ledger-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("effects.jsonl")
    }

    /// The hash discipline: links are intact, order matters, any tamper
    /// (value change, deletion, reorder) breaks verification.
    #[test]
    fn chain_detects_tamper_delete_and_reorder() {
        let mut chain = Chain::default();
        let r1 = chain.append("a1", "cap1", "call1", "res1");
        let r2 = chain.append("a2", "cap2", "call2", "res2");
        let r3 = chain.append("a3", "cap3", "call3", "res3");
        assert_eq!(r1.prev_receipt_hash, "GENESIS");
        assert_eq!(r2.prev_receipt_hash, r1.chain_hash);
        assert_eq!(r3.prev_receipt_hash, r2.chain_hash);
        chain.verify().unwrap();

        // TAMPER a bound digest in the middle receipt.
        let mut c = Chain::default();
        let mut tampered = chain.receipts().to_vec();
        tampered[1].call_digest = "evil".into();
        c.receipts = tampered;
        assert!(c.verify().is_err(), "value tamper detected");

        // DELETE the middle receipt.
        let c = Chain { receipts: vec![chain.receipts()[0].clone(), chain.receipts()[2].clone()] };
        assert!(c.verify().is_err(), "deletion detected");

        // REORDER two receipts.
        let c = Chain { receipts: vec![chain.receipts()[1].clone(), chain.receipts()[0].clone()] };
        assert!(c.verify().is_err(), "reorder detected");
    }

    /// The durable ledger: appends flush to disk, a reopened ledger
    /// continues the SAME hash chain, and an on-disk tamper is caught.
    #[test]
    fn ledger_is_durable_and_detects_on_disk_tamper() {
        let path = scratch("durable");
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.append("a", "cap", "call-1", "res-1").unwrap();
        let last2 = ledger.append("b", "cap", "call-2", "res-2").unwrap();
        assert!(ledger.verify_on_disk().is_ok());

        // Reopen: the chain continues from the last on-disk hash.
        let mut reopened = Ledger::open(&path).unwrap();
        assert_eq!(reopened.last_hash(), last2.chain_hash);
        let third = reopened.append("c", "cap", "call-3", "res-3").unwrap();
        assert_eq!(third.prev_receipt_hash, last2.chain_hash);
        assert!(reopened.verify_on_disk().is_ok());

        // TAMPER the file in place: a value edit breaks verification.
        let raw = std::fs::read_to_string(&path).unwrap();
        let tampered = raw.replace("call-2", "call-EVIL");
        std::fs::write(&path, tampered).unwrap();
        assert!(
            Ledger::verify_file(&path).is_err(),
            "on-disk tamper detected"
        );
        let _ = std::fs::remove_file(&path);
    }
}
