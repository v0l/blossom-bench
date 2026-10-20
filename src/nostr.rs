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

const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn bech32_polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk = 1u32;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x1ffffff) << 5) ^ *v as u32;
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

pub fn npub(pubkey: &[u8; 32]) -> String {
    let mut data: Vec<u8> = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in pubkey {
        acc = (acc << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            data.push(((acc >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        data.push(((acc << (5 - bits)) & 31) as u8);
    }
    let hrp = b"npub";
    let mut values: Vec<u8> = hrp.iter().map(|c| c >> 5).collect();
    values.push(0);
    values.extend(hrp.iter().map(|c| c & 31));
    values.extend(&data);
    values.extend([0u8; 6]);
    let polymod = bech32_polymod(&values) ^ 1;
    let checksum = (0..6).map(|i| ((polymod >> (5 * (5 - i))) & 31) as u8);
    let body: String = data.iter().copied().chain(checksum).map(|v| BECH32_CHARSET[v as usize] as char).collect();
    format!("npub1{body}")
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

#[cfg(test)]
mod npub_tests {
    use super::*;

    #[test]
    fn npub_matches_known_vector() {
        let hex = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
        let mut key = [0u8; 32];
        key.copy_from_slice(&hex::decode(hex).unwrap());
        assert_eq!(npub(&key), "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6");
    }
}
