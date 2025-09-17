use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use appattest_rs::{assertion::Assertion, attestation::Attestation};
use base64::{engine::general_purpose, Engine as _};
use ciborium::de::from_reader;
use constant_time_eq::constant_time_eq;
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

#[derive(Clone)]
pub struct AppAttestService {
    app_id: Arc<str>,
    challenge_ttl: Duration,
}

pub struct AttestationVerification {
    pub public_key: Vec<u8>,
    pub receipt: Vec<u8>,
}

pub struct AssertionVerification {
    pub counter: u32,
}

#[derive(Debug, Deserialize)]
struct RawAssertion {
    #[serde(rename = "authenticatorData")]
    authenticator_data: Vec<u8>,
    #[serde(rename = "signature")]
    _signature: Vec<u8>,
}

impl AppAttestService {
    pub fn new(app_id: String, challenge_ttl_secs: u64) -> Self {
        let ttl = Duration::seconds(challenge_ttl_secs as i64);
        Self {
            app_id: Arc::from(app_id),
            challenge_ttl: ttl,
        }
    }

    pub fn generate_challenge(&self) -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn issue_challenge(&self) -> (String, OffsetDateTime) {
        let challenge = self.generate_challenge();
        let expires_at = OffsetDateTime::now_utc() + self.challenge_ttl;
        (challenge, expires_at)
    }

    pub fn verify_attestation(
        &self,
        attestation_b64: &str,
        challenge: &str,
        key_id: &str,
    ) -> Result<AttestationVerification> {
        let attestation = Attestation::from_base64(attestation_b64)
            .context("failed to decode attestation payload")?;

        let (public_key, receipt) = attestation
            .verify(challenge, &self.app_id, key_id)
            .map_err(|e| anyhow!("app attest attestation validation failed: {e}"))?;

        Ok(AttestationVerification {
            public_key,
            receipt,
        })
    }

    pub fn validate_challenge(
        &self,
        expected: Option<&str>,
        expires_at: Option<OffsetDateTime>,
        provided: &str,
    ) -> Result<()> {
        if let Some(expiry) = expires_at {
            if OffsetDateTime::now_utc() > expiry {
                bail!("app attest challenge expired");
            }
        }

        if let Some(expected_challenge) = expected {
            if !constant_time_eq(expected_challenge.as_bytes(), provided.as_bytes()) {
                bail!("app attest challenge mismatch");
            }
        }

        Ok(())
    }

    pub fn verify_assertion(
        &self,
        assertion_b64: &str,
        client_data_b64: &str,
        public_key: &[u8],
        previous_counter: u32,
        stored_challenge: Option<&str>,
        presented_challenge: &str,
    ) -> Result<AssertionVerification> {
        let client_data = general_purpose::STANDARD
            .decode(client_data_b64)
            .context("invalid clientData base64")?;

        let assertion =
            Assertion::from_base64(assertion_b64).context("failed to decode assertion payload")?;

        let challenge_for_validation = stored_challenge.unwrap_or(presented_challenge);

        assertion
            .verify(
                client_data,
                &self.app_id,
                public_key.to_vec(),
                previous_counter,
                challenge_for_validation,
            )
            .map_err(|e| anyhow!("app attest assertion validation failed: {e}"))?;

        let counter = Self::parse_counter(assertion_b64)?;

        if counter <= previous_counter {
            bail!("app attest counter did not advance");
        }

        Ok(AssertionVerification { counter })
    }

    fn parse_counter(assertion_b64: &str) -> Result<u32> {
        let bytes = general_purpose::STANDARD
            .decode(assertion_b64)
            .context("invalid assertion base64")?;

        let raw: RawAssertion =
            from_reader(bytes.as_slice()).context("failed to decode assertion CBOR")?;

        if raw.authenticator_data.len() < 37 {
            bail!("authenticator data too short");
        }

        let counter_bytes: [u8; 4] = raw.authenticator_data[33..37]
            .try_into()
            .map_err(|_| anyhow!("invalid counter slice"))?;

        Ok(u32::from_be_bytes(counter_bytes))
    }
}
