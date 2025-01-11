use sha2::{Sha256, Digest};
use crate::scalar::{get_curve_order, Scalar, get_field_size};
use crate::field::FieldElement;
use crate::secp256k1::Point;
use std::ops::Sub;
use num_bigint::BigUint;
use tfhe::prelude::*;
use tfhe::{generate_keys, set_server_key, ConfigBuilder, FheUint32, FheUint64, ClientKey, FheBool, CompressedServerKey};

/// BIP-340 tag constants for domain separation
const AUX_TAG: &[u8] = b"BIP0340/aux";
const NONCE_TAG: &[u8] = b"BIP0340/nonce";
const CHALLENGE_TAG: &[u8] = b"BIP0340/challenge";

/// Represents a Schnorr signature according to BIP-340
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub r_x: FieldElement,  // x-coordinate of R
    pub s: Scalar,         // scalar s
}

impl Signature {
    /// Serializes the signature to bytes according to BIP-340: R.x || s
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut sig = Vec::with_capacity(64);
        sig.extend_from_slice(&bytes_from_int(&self.r_x.value()));
        sig.extend_from_slice(&bytes_from_int(self.s.value()));
        sig
    }
}

/// The Schnorr signature scheme implementation following BIP-340
pub struct Schnorr {
    private_key: Scalar,
}

impl Schnorr {
    /// Creates a new Schnorr instance with the given private key.
    pub fn new(private_key: Scalar) -> Self {
        Self { private_key }
    }

    /// Gets the public key with BIP-340 y-coordinate conventions
    fn get_public_key(&self) -> (Point, Scalar) {
        let generator = Point::get_generator();
        let public_key = generator.scalar_mul(&self.private_key);

        // Ensure the public key has an even y-coordinate as per BIP-340
        if public_key.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            (
                Point::new(
                    public_key.x.clone(),
                    FieldElement::new(get_field_size() - public_key.y.value(), get_field_size()),
                    false
                ),
                Scalar::new(get_curve_order() - self.private_key.value())
            )
        } else {
            (public_key, self.private_key.clone())
        }
    }

    /// Signs a message using the Schnorr signature scheme according to BIP-340.
    pub fn sign(&self, message: &[u8], aux_rand: &[u8]) -> Signature {
        let (pubkey, d) = self.get_public_key();

        // Generate deterministic nonce k0 according to BIP-340
        let k0 = compute_nonce(d.value(), &pubkey, message, aux_rand);
        let generator = Point::get_generator();
        let r = generator.scalar_mul(&Scalar::new(k0.clone()));

        // Adjust k based on R's y-coordinate parity
        let k = if r.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            get_curve_order() - k0
        } else {
            k0
        };

        // Compute challenge e = hash(R || P || message)
        let e = compute_challenge(&r, &pubkey, message);

        // Compute s = (k + e * d) % n
        let s = (k + e * d.value()) % get_curve_order();

        Signature {
            r_x: r.x,
            s: Scalar::new(s),
        }
    }

    /// Signs a message using the Schnorr signature scheme according to BIP-340 with FHE.
    pub fn sign_fhe(&self, message: &[u8], aux_rand: &[u8], client_key: &ClientKey) -> Result<Signature, tfhe::Error> {
        let (pubkey, d) = self.get_public_key();

        // Generate deterministic nonce k0 according to BIP-340
        let k0 = compute_nonce(d.value(), &pubkey, message, aux_rand);
        let generator = Point::get_generator();
        let r = generator.scalar_mul(&Scalar::new(k0.clone()));

        // Adjust k based on R's y-coordinate parity
        let k = if r.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            get_curve_order() - k0
        } else {
            k0
        };

        // Compute challenge e = hash(R || P || message)
        let e = compute_challenge(&r, &pubkey, message);

        // Convert to u64 values with modular reduction
        let curve_order_u64 = (u64::MAX / 2) + 1;
        let d_bytes = (d.value() % BigUint::from(curve_order_u64)).to_bytes_be();
        let k_bytes = (k % BigUint::from(curve_order_u64)).to_bytes_be();
        let e_bytes = (e % BigUint::from(curve_order_u64)).to_bytes_be();

        let d_u64 = if d_bytes.len() >= 8 {
            u64::from_be_bytes(d_bytes[d_bytes.len()-8..].try_into().unwrap())
        } else {
            let mut bytes = [0u8; 8];
            bytes[8-d_bytes.len()..].copy_from_slice(&d_bytes);
            u64::from_be_bytes(bytes)
        };

        let k_u64 = if k_bytes.len() >= 8 {
            u64::from_be_bytes(k_bytes[k_bytes.len()-8..].try_into().unwrap())
        } else {
            let mut bytes = [0u8; 8];
            bytes[8-k_bytes.len()..].copy_from_slice(&k_bytes);
            u64::from_be_bytes(bytes)
        };

        let e_u64 = if e_bytes.len() >= 8 {
            u64::from_be_bytes(e_bytes[e_bytes.len()-8..].try_into().unwrap())
        } else {
            let mut bytes = [0u8; 8];
            bytes[8-e_bytes.len()..].copy_from_slice(&e_bytes);
            u64::from_be_bytes(bytes)
        };

        // Encrypt values
        let d_encrypted = FheUint64::encrypt(d_u64, client_key);
        let k_encrypted = FheUint64::encrypt(k_u64, client_key);
        let e_encrypted = FheUint64::encrypt(e_u64, client_key);

        // Compute s = (k + e * d) % n in encrypted form
        let ed_encrypted = e_encrypted * d_encrypted;
        let s_encrypted = k_encrypted + ed_encrypted;
        let s_u64: u64 = s_encrypted.decrypt(client_key);

        // Convert back to field element and reduce modulo n
        let s_bytes = s_u64.to_be_bytes();
        let s = BigUint::from_bytes_be(&s_bytes);
        let s = s % get_curve_order();

        Ok(Signature {
            r_x: r.x,
            s: Scalar::new(s),
        })
    }

    /// Verifies a Schnorr signature according to BIP-340.
    pub fn verify(message: &[u8], pubkey_bytes: &[u8], sig_bytes: &[u8]) -> bool {
        // Check input lengths
        if pubkey_bytes.len() != 32 || sig_bytes.len() != 64 {
            return false;
        }

        // Parse signature and public key
        let sig = Signature {
            r_x: FieldElement::new(BigUint::from_bytes_be(&sig_bytes[0..32]), get_field_size()),
            s: Scalar::new(BigUint::from_bytes_be(&sig_bytes[32..64])),
        };

        // Lift x coordinates to curve points
        let pubkey = FieldElement::new(BigUint::from_bytes_be(pubkey_bytes), get_field_size());
        let pubkey_point = lift_x(&pubkey);
        if pubkey_point.is_infinity {
            return false;
        }

        // Reconstruct R point from x-coordinate
        let r_point = {
            let x3 = &sig.r_x * &sig.r_x * &sig.r_x;
            let r_y_squared = x3 + FieldElement::new(BigUint::from(7u32), get_field_size());
            let mut r_y = r_y_squared.sqrt();
            if r_y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
                r_y = FieldElement::new(get_field_size() - r_y.value(), get_field_size());
            }
            Point::new(sig.r_x.clone(), r_y, false)
        };

        // Verify signature bounds
        if r_point.x.value() >= &get_curve_order() || sig.s.value() >= &get_curve_order() {
            return false;
        }

        // Compute sG and eP
        let generator = Point::get_generator();
        let s_g = generator.scalar_mul(&sig.s);
        let e = Scalar::new(compute_challenge(&r_point, &pubkey_point, message));
        let e_p = pubkey_point.scalar_mul(&e);

        // Verify R = sG - eP and has even y-coordinate
        let r_computed = s_g - e_p;
        !(r_computed.is_infinity
          || r_computed.y.value() % BigUint::from(2u32) == BigUint::from(1u32)
          || r_computed.x.value() != sig.r_x.value())
    }
}

/// Computes the tagged hash according to BIP-340 specification.
/// tagged_hash = SHA256(SHA256(tag) || SHA256(tag) || msg)
fn tagged_hash(tag: &[u8], msg: &[u8]) -> Vec<u8> {
    let tag_hash = Sha256::digest(tag);
    let mut hasher = Sha256::new();
    hasher.update(&tag_hash);
    hasher.update(&tag_hash);
    hasher.update(msg);
    hasher.finalize().to_vec()
}

/// Converts a BigUint to a 32-byte array in big-endian format
fn bytes_from_int(n: &BigUint) -> [u8; 32] {
    let mut bytes = n.to_bytes_be();
    let mut result = [0u8; 32];
    let start = 32 - bytes.len();
    result[start..].copy_from_slice(&bytes);
    result
}

/// Converts a Point to a 32-byte array by taking its x-coordinate
fn bytes_from_point(p: &Point) -> [u8; 32] {
    bytes_from_int(&p.x.value())
}

/// Computes the nonce according to BIP-340 specification
fn compute_nonce(d: &BigUint, pubkey: &Point, message: &[u8], aux_rand: &[u8]) -> BigUint {
    let t = xor_bytes(&bytes_from_int(d), &tagged_hash(AUX_TAG, aux_rand));
    let mut nonce_input = Vec::new();
    nonce_input.extend_from_slice(&t);
    nonce_input.extend_from_slice(&bytes_from_point(pubkey));
    nonce_input.extend_from_slice(message);
    BigUint::from_bytes_be(&tagged_hash(NONCE_TAG, &nonce_input)) % get_curve_order()
}

/// Computes the challenge according to BIP-340 specification
fn compute_challenge(r: &Point, pubkey: &Point, message: &[u8]) -> BigUint {
    let mut challenge_input = Vec::new();
    challenge_input.extend_from_slice(&bytes_from_point(r));
    challenge_input.extend_from_slice(&bytes_from_point(pubkey));
    challenge_input.extend_from_slice(message);
    BigUint::from_bytes_be(&tagged_hash(CHALLENGE_TAG, &challenge_input)) % get_curve_order()
}

/// Performs XOR operation on two 32-byte slices
fn xor_bytes(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut result = [0u8; 32];
    for i in 0..32 {
        result[i] = a[i] ^ b[i];
    }
    result
}

/// Lifts an x-coordinate to a point on the curve with even y-coordinate
fn lift_x(x: &FieldElement) -> Point {
    if x.value() >= &get_curve_order() {
        return Point::infinity();
    }
    let y_squared = x.pow(&BigUint::from(3u32)) + FieldElement::new(BigUint::from(7u32), x.order().clone());
    let mut y = y_squared.sqrt();
    if y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
        y = FieldElement::new(x.order().clone() - y.value(), x.order().clone());
    }
    Point::new(x.clone(), y, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex;

    #[test]
    fn test_schnorr_fhe() {
        let config = ConfigBuilder::default().build();
        let (client_key, server_keys) = generate_keys(config);
        set_server_key(server_keys);

        // Test vector from BIP-340
        let seckey_bytes = hex::decode("0000000000000000000000000000000000000000000000000000000000000003").unwrap();
        let message = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let aux_rand = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let expected_sig = hex::decode("E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0").unwrap();

        let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
        let schnorr = Schnorr::new(seckey);
        let sig = schnorr.sign(&message, &aux_rand);
        let (pubkey, _) = schnorr.get_public_key();

        assert_eq!(sig.to_bytes(), expected_sig);
        assert!(Schnorr::verify(&message, &pubkey.x.value().to_bytes_be(), &expected_sig));

        let sig_fhe = schnorr.sign_fhe(&message, &aux_rand, &client_key);
        assert!(sig_fhe.is_ok());
        let sig_fhe = sig_fhe.unwrap();
        assert_eq!(sig_fhe.to_bytes(), expected_sig);
        assert!(Schnorr::verify(&message, &pubkey.x.value().to_bytes_be(), &expected_sig));
    }

    fn test_schnorr_bip340() {
        // Test vector from BIP-340
        let seckey_bytes = hex::decode("0000000000000000000000000000000000000000000000000000000000000003").unwrap();
        let message = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let aux_rand = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let expected_sig = hex::decode("E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0").unwrap();

        let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
        let schnorr = Schnorr::new(seckey);
        let sig = schnorr.sign(&message, &aux_rand);
        let (pubkey, _) = schnorr.get_public_key();

        assert_eq!(sig.to_bytes(), expected_sig);
        assert!(Schnorr::verify(&message, &pubkey.x.value().to_bytes_be(), &expected_sig));
    }

    #[test]
    fn test_schnorr_vectors() {
        let csv_content = include_str!("../tests/test_vectors.csv");
        let mut all_passed = true;

        for (i, line) in csv_content.lines().skip(1).enumerate() {
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() < 7 { continue; }

            let (index, seckey_hex, pubkey_hex, aux_rand_hex, msg_hex, sig_hex, result_str) =
                (fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6]);
            let expected_result = result_str == "TRUE";

            let pubkey_bytes = hex::decode(pubkey_hex).unwrap();
            let message = hex::decode(msg_hex).unwrap();
            let expected_sig = hex::decode(sig_hex).unwrap();

            if !seckey_hex.is_empty() {
                let seckey_bytes = hex::decode(seckey_hex).unwrap();
                let aux_rand = hex::decode(aux_rand_hex).unwrap();
                let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
                let schnorr = Schnorr::new(seckey.clone());

                let sig = schnorr.sign(&message, &aux_rand);
                if sig.to_bytes() != expected_sig {
                    println!("Failed signing test for vector #{}", index);
                    all_passed = false;
                    continue;
                }
            }

            let result = Schnorr::verify(&message, &pubkey_bytes, &expected_sig);
            if result != expected_result {
                println!("Failed verification test for vector #{}", index);
                all_passed = false;
                continue;
            }
        }

        assert!(all_passed, "Some test vectors failed");
    }
}


