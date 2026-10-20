use std::io::Read;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use secp256k1::{Keypair, SecretKey, schnorr};
use sha2::{Digest, Sha256};

pub struct Signer {
    keypair: Keypair,
    pub pubkey_hex: String,
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

impl Signer {
    pub fn fixed() -> Result<Self> {
        Self::from_bytes(Sha256::digest(b"blossom-bench signing key").into())
    }

    pub fn new() -> Result<Self> {
        Self::from_bytes(random_bytes::<32>()?)
    }

    fn from_bytes(bytes: [u8; 32]) -> Result<Self> {
        let sk = SecretKey::from_secret_bytes(bytes)?;
        let keypair = Keypair::from_secret_key(&sk);
        let (xonly, _) = keypair.x_only_public_key();
        Ok(Self {
            keypair,
            pubkey_hex: hex::encode(xonly.to_byte_array()),
        })
    }

    pub fn upload_token(&self, sha256_hex: &str, server_host: &str, encoding: &str, bound: Option<&str>) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + if bound.is_some() { 25 } else { 0 };
        let tags = match bound {
            Some(base) => serde_json::json!([
                ["t", "upload"],
                ["x", sha256_hex],
                ["expiration", (now + 100).to_string()],
                ["server", base],
                ["u", format!("{base}/upload")],
                ["method", "PUT"],
            ]),
            None => serde_json::json!([
                ["t", "upload"],
                ["x", sha256_hex],
                ["expiration", (now + 3600).to_string()],
                ["server", server_host],
            ]),
        };
        let preimage = serde_json::json!([0, self.pubkey_hex, now, 24242, tags, "Upload"]);
        let id: [u8; 32] = Sha256::digest(serde_json::to_vec(&preimage)?).into();
        let sig = schnorr::sign_no_aux_rand(&id, &self.keypair);
        let event = serde_json::json!({
            "id": hex::encode(id),
            "pubkey": self.pubkey_hex,
            "created_at": now,
            "kind": 24242,
            "tags": tags,
            "content": "Upload",
            "sig": hex::encode(sig.to_byte_array()),
        });
        let raw = serde_json::to_vec(&event)?;
        let encoded = match encoding {
            "url" => URL_SAFE_NO_PAD.encode(raw),
            _ => STANDARD.encode(raw),
        };
        Ok(format!("Nostr {encoded}"))
    }
}
