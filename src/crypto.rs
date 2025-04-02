use anyhow::Result;
use sha2::{Digest, Sha256};
use std::env;

// Provides utilities for hashing DIDs to protect privacy
pub struct CryptoUtils {
    pub server_secret: String,
}

impl CryptoUtils {
    // Create a new CryptoUtils instance, loading the server secret from environment
    pub fn new() -> Result<Self> {
        let server_secret = env::var("SERVER_ENCRYPTION_SECRET")
        .expect("SERVER_ENCRYPTION_SECRET environment variable must be set");
    
        Ok(Self { server_secret })
    }

    // Hash a DID with a user-specific salt
    pub fn hash_did(&self, did_to_hash: &str, user_did: &str) -> String {
        // Create a unique salt per user by combining the user's DID with the server secret
        let salt = format!("{}{}", user_did, self.server_secret);
        
        // Combine the DID to hash with the salt
        let to_hash = format!("{}{}", did_to_hash, salt);
        
        // Calculate the SHA-256 hash
        let mut hasher = Sha256::new();
        hasher.update(to_hash.as_bytes());
        let result = hasher.finalize();
        
        // Return as hex string
        format!("{:x}", result)
    }

    // Check if a DID matches a stored hash
    pub fn did_matches_hash(&self, did_to_check: &str, user_did: &str, stored_hash: &str) -> bool {
        let computed_hash = self.hash_did(did_to_check, user_did);
        computed_hash == stored_hash
    }

    // Batch hash multiple DIDs at once
    pub fn hash_dids_batch(&self, dids_to_hash: &[String], user_did: &str) -> Vec<String> {
        dids_to_hash.iter()
            .map(|did| self.hash_did(did, user_did))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_did_hashing() {
        let crypto = CryptoUtils::new().unwrap();
        
        // Test basic hashing
        let hash1 = crypto.hash_did("did:plc:test1", "did:plc:user1");
        let hash2 = crypto.hash_did("did:plc:test1", "did:plc:user1");
        let hash3 = crypto.hash_did("did:plc:test1", "did:plc:user2");
        
        // Same user hashing same DID should be consistent
        assert_eq!(hash1, hash2);
        
        // Different users hashing same DID should produce different hashes
        assert_ne!(hash1, hash3);
        
        // Test hash verification
        assert!(crypto.did_matches_hash("did:plc:test1", "did:plc:user1", &hash1));
        assert!(!crypto.did_matches_hash("did:plc:test2", "did:plc:user1", &hash1));
        
        // Test batch hashing
        let dids = vec!["did:plc:test1".to_string(), "did:plc:test2".to_string()];
        let hashes = crypto.hash_dids_batch(&dids, "did:plc:user1");
        
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes[0], hash1);
    }
}
