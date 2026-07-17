//! ES256 access-token signing. The keypair is generated on first boot if
//! none exists and persisted in Postgres (not a Kubernetes Secret) --
//! avoids any chicken-and-egg RBAC/Secret-writing problem, and matches how
//! everything else in this stack treats Postgres as the source of truth.
//! `registry_db::insert_signing_key_if_absent`'s `ON CONFLICT DO NOTHING`
//! against a singleton row means two replicas racing to generate a key on
//! first boot still converge on one winner.

use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::Serialize;
use sqlx::PgPool;

pub struct Signer {
    encoding_key: EncodingKey,
    pub jwk: Jwk,
    kid: String,
}

impl Signer {
    pub async fn load_or_create(pool: &PgPool) -> anyhow::Result<Self> {
        let der = match registry_db::get_signing_key(pool).await? {
            Some(der) => der,
            None => {
                let rng = SystemRandom::new();
                let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow::anyhow!("failed to generate signing key"))?;
                registry_db::insert_signing_key_if_absent(pool, pkcs8.as_ref()).await?
            }
        };
        Self::from_der(der)
    }

    fn from_der(der: Vec<u8>) -> anyhow::Result<Self> {
        let encoding_key = EncodingKey::from_ec_der(&der);
        // A stable identifier for this key, derived from the key material
        // itself (not random) -- so `kid` survives a restart with no extra
        // bookkeeping, and would change automatically if the key were ever
        // rotated to different material.
        let kid = {
            use ring::digest::{digest, SHA256};
            let hash = digest(&SHA256, &der);
            data_encoding_hex(&hash.as_ref()[..8])
        };

        let mut jwk = Jwk::from_encoding_key(&encoding_key, Algorithm::ES256)?;
        jwk.common.key_id = Some(kid.clone());

        Ok(Self { encoding_key, jwk, kid })
    }

    pub fn sign<T: Serialize>(&self, claims: &T) -> anyhow::Result<String> {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        Ok(jsonwebtoken::encode(&header, claims, &self.encoding_key)?)
    }

    pub fn jwks_json(&self) -> serde_json::Value {
        serde_json::to_value(JwkSet { keys: vec![self.jwk.clone()] }).expect("JwkSet always serializes")
    }
}

fn data_encoding_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
