//! Le calcul SRP-6a qu'utilise Apple pour vérifier le mot de passe sans jamais l'envoyer.
//!
//! Reproduit exactement la bibliothèque Python `srp` (mode `rfc5054_enable()` +
//! `no_username_in_x()`, SHA-256, groupe 2048 bits) qu'utilise icloudpd.

use num_bigint::BigUint;
use sha2::{Digest, Sha256};

/// Groupe 2048 bits de la RFC 5054.
const N_HEX: &[u8] = b"AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";

fn n() -> BigUint {
    BigUint::parse_bytes(N_HEX, 16).unwrap()
}

fn g() -> BigUint {
    BigUint::from(2u32)
}

/// Comme `long_to_bytes` en Python : gros-boutiste, sans zéros en tête.
fn bytes(x: &BigUint) -> Vec<u8> {
    x.to_bytes_be()
}

fn pad(data: &[u8], width: usize) -> Vec<u8> {
    let mut out = vec![0u8; width.saturating_sub(data.len())];
    out.extend_from_slice(data);
    out
}

fn sha(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

fn sha_int(parts: &[&[u8]]) -> BigUint {
    BigUint::from_bytes_be(&sha(parts))
}

/// La clé dérivée du mot de passe (protocoles `s2k` et `s2k_fo` d'Apple).
pub fn password_key(password: &str, protocol: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    let digest = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = if protocol == "s2k_fo" {
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes()
    } else {
        digest.to_vec()
    };
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, iterations, &mut key);
    key.to_vec()
}

pub struct Client {
    account: String,
    a: BigUint,
    big_a: BigUint,
}

pub struct Proof {
    pub m1: Vec<u8>,
    pub m2: Vec<u8>,
}

impl Client {
    pub fn new(account: &str) -> Client {
        let mut secret = [0u8; 256];
        getrandom::fill(&mut secret).expect("générateur aléatoire indisponible");
        secret[0] |= 0x80;
        Client::with_secret(account, &secret)
    }

    pub fn with_secret(account: &str, secret: &[u8]) -> Client {
        let a = BigUint::from_bytes_be(secret);
        let big_a = g().modpow(&a, &n());
        Client { account: account.to_string(), a, big_a }
    }

    /// La valeur publique `A` à envoyer à Apple.
    pub fn public(&self) -> Vec<u8> {
        bytes(&self.big_a)
    }

    /// Calcule les preuves M1 et M2 à partir du sel et de `B` renvoyés par Apple.
    pub fn prove(&self, key: &[u8], salt: &[u8], server_b: &[u8]) -> Option<Proof> {
        let (n, g) = (n(), g());
        let width = bytes(&n).len();
        let b = BigUint::from_bytes_be(server_b);
        if (&b % &n) == BigUint::ZERO {
            return None;
        }
        let k = sha_int(&[&bytes(&n), &pad(&bytes(&g), width)]);
        let u = sha_int(&[&pad(&bytes(&self.big_a), width), &pad(&bytes(&b), width)]);
        if u == BigUint::ZERO {
            return None;
        }
        // x = H(sel | H(":" | clé)) : Apple n'inclut pas l'identifiant dans x.
        let x = sha_int(&[salt, &sha(&[b":", key])]);
        let v = g.modpow(&x, &n);
        let kv = (&k * &v) % &n;
        let base = ((&b + &n) - kv) % &n;
        let s = base.modpow(&(&self.a + &u * &x), &n);
        let session_key = sha(&[&bytes(&s)]);

        let hn = sha(&[&bytes(&n)]);
        let hg = sha(&[&pad(&bytes(&g), width)]);
        let hn_xor_hg: Vec<u8> = hn.iter().zip(&hg).map(|(a, b)| a ^ b).collect();
        let m1 = sha(&[
            &hn_xor_hg,
            &sha(&[self.account.as_bytes()]),
            salt,
            &bytes(&self.big_a),
            &bytes(&b),
            &session_key,
        ]);
        let m2 = sha(&[&bytes(&self.big_a), &m1, &session_key]);
        Some(Proof { m1, m2 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Vecteurs produits par la bibliothèque Python `srp` (celle d'icloudpd), serveur compris :
    /// `user|mot de passe|protocole|itérations|sel|a|A|B|M1|M2|clé`, en base64.
    #[test]
    fn identique_a_pysrp() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).unwrap();
        for line in include_str!("srp_vectors.txt").lines() {
            let f: Vec<&str> = line.split('|').collect();
            let (user, pwd, proto, iterations) = (f[0], f[1], f[2], f[3].parse().unwrap());
            let (salt, a, big_a, big_b, m1, m2, key) = (b64(f[4]), b64(f[5]), b64(f[6]), b64(f[7]), b64(f[8]), b64(f[9]), b64(f[10]));
            assert_eq!(password_key(pwd, proto, &salt, iterations), key, "clé {user}");
            let client = Client::with_secret(user, &a);
            assert_eq!(client.public(), big_a, "A {user}");
            let proof = client.prove(&key, &salt, &big_b).unwrap();
            assert_eq!(proof.m1, m1, "M1 {user}");
            assert_eq!(proof.m2, m2, "M2 {user}");
        }
    }
}
