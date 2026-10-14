//! SRP-6a client as used by Apple's web sign-in (idmsa.apple.com).
//!
//! Matches pysrp with `rfc5054_enable()` + `no_username_in_x()`, which is what
//! pyicloud uses: 2048-bit RFC 5054 group, SHA-256, x = H(s | H(":" | P)) where P
//! is the PBKDF2-derived password key, k and u computed over padded values.

use num_bigint::BigUint;
use sha2::{Digest, Sha256};

const N_HEX: &str = "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050\
A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50\
E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B8\
55F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773B\
CA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748\
544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6\
AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB6\
94B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    a_pub: BigUint,
}

pub struct SrpProof {
    pub m1: Vec<u8>,
    pub m2: Vec<u8>,
}

impl SrpClient {
    pub fn new() -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid SRP group");
        let g = BigUint::from(2u32);
        let a = BigUint::from_bytes_be(&rand::random::<[u8; 32]>());
        let a_pub = g.modpow(&a, &n);
        Self { n, g, a, a_pub }
    }

    /// Public ephemeral value A, unpadded big-endian.
    pub fn public_a(&self) -> Vec<u8> {
        self.a_pub.to_bytes_be()
    }

    pub fn process_challenge(
        &self,
        username: &str,
        password_key: &[u8],
        salt: &[u8],
        b_bytes: &[u8],
    ) -> Option<SrpProof> {
        let width = self.n.to_bytes_be().len();
        let b = BigUint::from_bytes_be(b_bytes);
        if (&b % &self.n) == BigUint::ZERO {
            return None;
        }

        let k = hash_int(&[&self.n.to_bytes_be(), &pad(&self.g, width)]);
        let u = hash_int(&[&pad(&self.a_pub, width), &pad(&b, width)]);
        if u == BigUint::ZERO {
            return None;
        }

        // pysrp hashes the inner digest back through an integer, which drops
        // leading zero bytes; mirror that exactly.
        let inner = Sha256::new().chain_update(b":").chain_update(password_key).finalize();
        let inner = BigUint::from_bytes_be(&inner).to_bytes_be();
        let x = hash_int(&[salt, &inner]);

        let v = self.g.modpow(&x, &self.n);
        let kv = (&k * &v) % &self.n;
        let base = ((&b % &self.n) + &self.n - kv) % &self.n;
        let s = base.modpow(&(&self.a + &u * &x), &self.n);
        let key = Sha256::digest(s.to_bytes_be());

        let h_n = Sha256::digest(self.n.to_bytes_be());
        let h_g = Sha256::digest(pad(&self.g, width));
        let n_xor_g: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();

        let a_bytes = self.a_pub.to_bytes_be();
        let m1 = Sha256::new()
            .chain_update(&n_xor_g)
            .chain_update(Sha256::digest(username.as_bytes()))
            .chain_update(salt)
            .chain_update(&a_bytes)
            .chain_update(b.to_bytes_be())
            .chain_update(key)
            .finalize()
            .to_vec();
        let m2 = Sha256::new()
            .chain_update(&a_bytes)
            .chain_update(&m1)
            .chain_update(key)
            .finalize()
            .to_vec();
        Some(SrpProof { m1, m2 })
    }
}

/// Apple's password key: PBKDF2-HMAC-SHA256 over SHA-256(password).
/// For protocol `s2k_fo` the digest is hex-encoded before derivation.
pub fn derive_password_key(password: &str, salt: &[u8], iterations: u32, protocol: &str) -> Vec<u8> {
    let digest = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = if protocol == "s2k_fo" {
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes()
    } else {
        digest.to_vec()
    };
    let mut out = vec![0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, iterations, &mut out);
    out
}

fn pad(n: &BigUint, width: usize) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; width.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

fn hash_int(parts: &[&[u8]]) -> BigUint {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    BigUint::from_bytes_be(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_is_2048_bits() {
        let c = SrpClient::new();
        assert_eq!(c.n.bits(), 2048);
    }

    /// Play the server side (verifier v = g^x, B = kv + g^b) and check both
    /// sides derive the same session key, via the server-computed M1.
    #[test]
    fn agrees_with_simulated_server() {
        let client = SrpClient::new();
        let (n, g) = (client.n.clone(), client.g.clone());
        let width = n.to_bytes_be().len();
        let salt = b"0123456789abcdef";
        let key = derive_password_key("hunter2", salt, 1000, "s2k");

        let inner = Sha256::new().chain_update(b":").chain_update(&key).finalize();
        let inner = BigUint::from_bytes_be(&inner).to_bytes_be();
        let x = hash_int(&[salt, &inner]);
        let v = g.modpow(&x, &n);
        let k = hash_int(&[&n.to_bytes_be(), &pad(&g, width)]);
        let b = BigUint::from_bytes_be(&[7u8; 32]);
        let b_pub = (&k * &v + g.modpow(&b, &n)) % &n;

        let u = hash_int(&[&pad(&client.a_pub, width), &pad(&b_pub, width)]);
        let s = (&client.a_pub * v.modpow(&u, &n)).modpow(&b, &n);
        let server_key = Sha256::digest(s.to_bytes_be());
        let h_n = Sha256::digest(n.to_bytes_be());
        let h_g = Sha256::digest(pad(&g, width));
        let xor: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();
        let expected_m1 = Sha256::new()
            .chain_update(&xor)
            .chain_update(Sha256::digest(b"user@example.com"))
            .chain_update(salt)
            .chain_update(client.a_pub.to_bytes_be())
            .chain_update(b_pub.to_bytes_be())
            .chain_update(server_key)
            .finalize()
            .to_vec();

        let proof = client
            .process_challenge("user@example.com", &key, salt, &b_pub.to_bytes_be())
            .unwrap();
        assert_eq!(proof.m1, expected_m1);
    }

    #[test]
    fn s2k_fo_differs_from_s2k() {
        let a = derive_password_key("pw", b"salt", 1000, "s2k");
        let b = derive_password_key("pw", b"salt", 1000, "s2k_fo");
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
