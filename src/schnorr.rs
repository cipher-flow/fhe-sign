use sha2::{Sha256, Digest};
use rand::Rng;
use crate::scalar::{get_curve_order, Scalar, get_field_size};
use crate::field::FieldElement;
use crate::secp256k1::Point;
use std::ops::Sub;
use std::str::FromStr;
use num_bigint::BigUint;
use hex;

/// BIP-340 tag constants
const AUX_TAG: &[u8] = b"BIP0340/aux";
const NONCE_TAG: &[u8] = b"BIP0340/nonce";
const CHALLENGE_TAG: &[u8] = b"BIP0340/challenge";

/// Computes the tagged hash according to BIP340 specification.
/// The tag is used to prevent cross-protocol attacks by making the hash domain-specific.
/// tagged_hash = SHA256(SHA256(tag) || SHA256(tag) || msg)
fn tagged_hash(tag: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut tag_hasher = Sha256::new();
    tag_hasher.update(tag);
    let tag_hash = tag_hasher.finalize();

    let mut hasher = Sha256::new();
    hasher.update(&tag_hash);
    hasher.update(&tag_hash);
    hasher.update(msg);
    hasher.finalize().to_vec()
}

/// Converts a BigUint to a 32-byte array in big-endian format with zero padding
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

/// Converts a byte slice to a BigUint in big-endian format
fn int_from_bytes(b: &[u8]) -> BigUint {
    BigUint::from_bytes_be(b)
}

/// Performs XOR operation on two 32-byte slices
fn xor_bytes(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut result = [0u8; 32];
    for i in 0..32 {
        result[i] = a[i] ^ b[i];
    }
    result
}

/// Computes the nonce according to BIP340 specification
/// k0 = int_from_bytes(tagged_hash("BIP0340/nonce", t || bytes_from_point(P) || msg)) % n
/// where t = xor_bytes(bytes_from_int(d), tagged_hash("BIP0340/aux", aux_rand))
pub fn compute_nonce(d: &BigUint, pubkey: &Point, message: &[u8], aux_rand: &[u8]) -> BigUint {
    // Compute aux_tag_hash and XOR with private key bytes
    let aux_tag_hash = tagged_hash(AUX_TAG, aux_rand);
    let t = xor_bytes(&bytes_from_int(d), &aux_tag_hash);

    // Concatenate t || pubkey.x || message
    let mut nonce_input = Vec::new();
    nonce_input.extend_from_slice(&t);
    nonce_input.extend_from_slice(&bytes_from_point(pubkey));
    nonce_input.extend_from_slice(message);

    // Compute k0 = tagged_hash % n
    int_from_bytes(&tagged_hash(NONCE_TAG, &nonce_input)) % get_curve_order()
}

/// Computes the challenge according to BIP340 specification
/// e = int_from_bytes(tagged_hash("BIP0340/challenge", R.x || P.x || msg)) % n
pub fn compute_challenge(r: &Point, pubkey: &Point, message: &[u8]) -> BigUint {
    let mut challenge_input = Vec::new();
    challenge_input.extend_from_slice(&bytes_from_point(r));
    challenge_input.extend_from_slice(&bytes_from_point(pubkey));
    challenge_input.extend_from_slice(message);

    int_from_bytes(&tagged_hash(CHALLENGE_TAG, &challenge_input)) % get_curve_order()
}

pub fn lift_x(x: &FieldElement) -> Point {
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

/// Represents a Schnorr signature according to BIP-340
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub r_x: FieldElement,  // x-coordinate of R
    pub s: Scalar,         // scalar s
}

impl Signature {
    /// Converts the signature to bytes according to BIP-340: R.x || s
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut sig = Vec::new();
        sig.extend_from_slice(&bytes_from_int(&self.r_x.value()));
        sig.extend_from_slice(&bytes_from_int(self.s.value()));
        sig
    }
}

/// The Schnorr signature scheme implementation
pub struct Schnorr {
    private_key: Scalar,
    public_key: Point,
}

impl Schnorr {
    /// Creates a new Schnorr instance with the given private key.
    /// The public key is computed as P = private_key * G.
    pub fn new(private_key: Scalar) -> Self {
        let generator = Point::get_generator();
        let public_key = generator.scalar_mul(&private_key);

        // Ensure the public key has an even y-coordinate as per BIP-340
        let (pk, d) = if public_key.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            // If y is odd, negate both the point and private key
            (
                Point::new(
                    public_key.x.clone(),
                    FieldElement::new(get_field_size() - public_key.y.value(), get_field_size()),
                    false
                ),
                Scalar::new(get_curve_order() - private_key.value())
            )
        } else {
            (public_key, private_key)
        };

        Self { private_key: d, public_key: pk }
    }

    /// Signs a message using the Schnorr signature scheme according to BIP-340.
    pub fn sign(&self, message: &[u8], aux_rand: &[u8]) -> Signature {
        // Compute d = d0 if has_even_y(P) else n - d0
        let d = if self.public_key.y.value() % BigUint::from(2u32) == BigUint::from(0u32) {
            self.private_key.value().clone()
        } else {
            get_curve_order() - self.private_key.value()
        };

        // Generate deterministic nonce according to BIP340
        let k0 = compute_nonce(&d, &self.public_key, message, aux_rand);

        // Calculate R = k * G
        let generator = Point::get_generator();
        let r = generator.scalar_mul(&Scalar::new(k0.clone()));

        // k = n - k0 if not has_even_y(R) else k0
        let k = if r.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            get_curve_order() - k0
        } else {
            k0
        };

        // Calculate e = hash(R || P || message)
        let e = compute_challenge(&r, &self.public_key, message);

        // Calculate s = (k + e * d) % n
        let s = (k + e * d) % get_curve_order();

        Signature {
            r_x: r.x,
            s: Scalar::new(s),
        }
    }

    /// Verifies a Schnorr signature according to BIP-340.
    pub fn verify(message: &[u8], pubkey_bytes: &[u8], sig_bytes: &[u8]) -> bool {
        if pubkey_bytes.len() != 32 {
            return false;
        }
        if sig_bytes.len() != 64 {
            return false;
        }
        let sig = Signature {
            r_x: FieldElement::new(BigUint::from_bytes_be(&sig_bytes[0..32]), get_field_size()),
            s: Scalar::new(BigUint::from_bytes_be(&sig_bytes[32..64])),
        };
        // Reconstruct R point from x-coordinate
        let x3 = &sig.r_x * &sig.r_x * &sig.r_x;
        let seven = FieldElement::new(BigUint::from(7u32), get_field_size());
        let r_y_squared = x3 + seven;

        // Try both possible y values and use the even one
        let mut r_y = r_y_squared.sqrt();
        if r_y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            r_y = FieldElement::new(get_field_size() - r_y.value(), get_field_size());
        }
        // Calculate s * G
        let generator = Point::get_generator();
        let s_g = generator.scalar_mul(&sig.s);

        // Calculate e * P
        let pubkey = FieldElement::new(BigUint::from_bytes_be(&pubkey_bytes), get_field_size());
        let pubkey_point = lift_x(&pubkey);
        let r_point = Point::new(sig.r_x.clone(), r_y, false);
        let e_biguint = compute_challenge(&r_point, &pubkey_point, message);
        let e = Scalar::new(e_biguint);
        let e_p = pubkey_point.scalar_mul(&e);
        //     if (P is None) or (r >= p) or (s >= n): return False
        if pubkey_point.is_infinity || r_point.x.value() >= &get_curve_order() || sig.s.value() >= &get_curve_order() {
            return false;
        }
        // Check that s * G = R + e * P by comparing x-coordinates
        let r_computed = s_g - e_p;
        // if (R is None) or (not has_even_y(R)) or (x(R) != r)
        if r_computed.is_infinity || r_computed.y.value() % BigUint::from(2u32) == BigUint::from(1u32) || r_computed.x.value() != sig.r_x.value() {
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use rand_core::OsRng;
    use hex;

    #[test]
    fn test_schnorr_sign_computation() {
        // Test vector from the Python implementation
        let seckey_bytes = hex::decode("0000000000000000000000000000000000000000000000000000000000000003").unwrap();
        let pubkey_bytes = hex::decode("F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9").unwrap();
        let aux_rand = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let message = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let signature = hex::decode("E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0").unwrap();
        let aux_tag_hash = hex::decode("54f169cfc9e2e5727480441f90ba25c488f461c70b5ea5dcaaf7af69270aa514").unwrap();
        let nonce_tag_hash = hex::decode("1d2dc1652fee3ad08434469f9ad30536a5787feccfa308e8fb396c8030dd1c69").unwrap();
        let t_tag_hash = hex::decode("54f169cfc9e2e5727480441f90ba25c488f461c70b5ea5dcaaf7af69270aa517").unwrap();
        let k0_expected = BigUint::from_str("13197915491876976259551408334663313131887465579707439106007590226688883891305").unwrap();
        let k_expected = BigUint::from_str("102594173745439219164019576674024594720950098699367465276597572914829277603032").unwrap();
        let e_expected = BigUint::from_str("48720319366320448218248931228309816100339060958010591740548832691369990260430").unwrap();

        let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
        assert!(seckey.value() < &get_curve_order());

        let pubkey = Point::get_generator().scalar_mul(&seckey);
        assert_eq!(pubkey.x.value(), &BigUint::from_bytes_be(&pubkey_bytes));

        // d = d0 if has_even_y(P) else n - d0
        let d = if pubkey.y.value() % BigUint::from(2u32) == BigUint::from(0u32) {
            seckey.value().clone()
        } else {
            get_curve_order() - seckey.value().clone()
        };

        // aux tag hash
        // t = xor_bytes(bytes_from_int(d), aux_tag_hash)
        let t = xor_bytes(&bytes_from_int(&d), &aux_tag_hash[..]);
        let mut t_expected = [0u8; 32];
        t_expected.copy_from_slice(&t_tag_hash);
        assert_eq!(t, t_expected);

        // Create nonce input: t || bytes_from_point(P) || msg
        let mut nonce_input = Vec::new();
        nonce_input.extend_from_slice(&t);
        nonce_input.extend_from_slice(&bytes_from_point(&pubkey));
        nonce_input.extend_from_slice(&message);

        // nonce tag hash
        // k0 = int_from_bytes(tagged_hash("BIP0340/nonce", t + bytes_from_point(P) + msg)) % n
        let k0 = int_from_bytes(&tagged_hash(NONCE_TAG, &nonce_input)) % get_curve_order();
        assert_eq!(k0, k0_expected);
        // R = point_mul(G, k0)
        let r = Point::get_generator().scalar_mul(&Scalar::new(k0.clone()));
        println!("r: {:?}", r);
        // k = n - k0 if not has_even_y(R) else k0
        let k = if r.y.value() % BigUint::from(2u32) == BigUint::from(1u32) {
            get_curve_order() - k0.clone()
        } else {
            k0.clone()
        };
        assert_eq!(k, k_expected);
        // e = int_from_bytes(tagged_hash("BIP0340/challenge", bytes_from_point(R) + bytes_from_point(P) + msg)) % n
        let mut challenge_input = Vec::new();
        challenge_input.extend_from_slice(&bytes_from_point(&r));
        challenge_input.extend_from_slice(&bytes_from_point(&pubkey));
        challenge_input.extend_from_slice(&message);
        // challenge tag hash
        let e = int_from_bytes(&tagged_hash(CHALLENGE_TAG, &challenge_input)) % get_curve_order();
        assert_eq!(e, e_expected);
        // sig = bytes_from_point(R) + bytes_from_int((k + e * d) % n)
        let mut sig = Vec::new();
        sig.extend_from_slice(&bytes_from_point(&r));
        sig.extend_from_slice(&bytes_from_int(&((k + e * d) % get_curve_order())));
        assert_eq!(sig, signature);
    }

    #[test]
    fn test_schnorr_bip340() {
        // Test vector from BIP-340
        let seckey_bytes = hex::decode("0000000000000000000000000000000000000000000000000000000000000003").unwrap();
        let message = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let aux_rand = hex::decode("0000000000000000000000000000000000000000000000000000000000000000").unwrap();
        let expected_sig = hex::decode("E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0").unwrap();

        // Create signer
        let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
        let schnorr = Schnorr::new(seckey);

        // Compute nonce
        let k0 = compute_nonce(
            &schnorr.private_key.value(),
            &schnorr.public_key,
            &message,
            &aux_rand
        );
        assert_eq!(
            k0,
            BigUint::from_str("13197915491876976259551408334663313131887465579707439106007590226688883891305").unwrap()
        );

        // Sign message
        let sig = schnorr.sign(&message, &aux_rand);
        assert_eq!(sig.to_bytes(), expected_sig);

        // Verify signature
        assert!(Schnorr::verify(&message, &schnorr.public_key.x.value().to_bytes_be(), &expected_sig));
    }

    #[test]
    fn test_schnorr_vectors() {
        // Read test vectors from CSV file
        let csv_content = include_str!("../tests/test_vectors.csv");
        let mut all_passed = true;

        // Skip header line and process each test vector
        for (i, line) in csv_content.lines().skip(1).enumerate() {
            let fields: Vec<&str> = line.split(',').collect();
            if fields.len() < 7 { continue; }

            let (index, seckey_hex, pubkey_hex, aux_rand_hex, msg_hex, sig_hex, result_str) =
                (fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6]);
            let expected_result = result_str == "TRUE";

            println!("\nTest vector #{}", index);

            // Parse test vector
            let pubkey_bytes = hex::decode(pubkey_hex).unwrap();
            let message = hex::decode(msg_hex).unwrap();
            let expected_sig = hex::decode(sig_hex).unwrap();
            // pubkey = bytes.fromhex(pubkey_hex)
            // msg = bytes.fromhex(msg_hex)
            // sig = bytes.fromhex(sig_hex)
            // result = result_str == 'TRUE'
            if !seckey_hex.is_empty() {
                // Test key generation and signing
                let seckey_bytes = hex::decode(seckey_hex).unwrap();
                let aux_rand = hex::decode(aux_rand_hex).unwrap();
                let seckey = Scalar::new(BigUint::from_bytes_be(&seckey_bytes));
                let schnorr = Schnorr::new(seckey.clone());

                // Verify public key generation
                let pubkey_actual = schnorr.public_key.x.value();
                if pubkey_actual != &BigUint::from_bytes_be(&pubkey_bytes) {
                    println!("Failed key generation for test vector #{}", index);
                    println!("Expected pubkey: {}", hex::encode(pubkey_bytes).to_uppercase());
                    println!("Actual pubkey: {}", hex::encode(bytes_from_int(pubkey_actual)).to_uppercase());
                    all_passed = false;
                    continue;
                }
                println!("Passed key generation test");

                // Test signing
                let sig = schnorr.sign(&message, &aux_rand);
                if sig.to_bytes() != expected_sig {
                    println!("Failed signing test for vector #{}", index);
                    println!("Expected sig: {}", hex::encode(&expected_sig).to_uppercase());
                    println!("Actual sig: {}", hex::encode(&sig.to_bytes()).to_uppercase());
                    all_passed = false;
                    continue;
                }
                println!("Passed signing test");
            }

            // Test verification
            let result = Schnorr::verify(&message, &pubkey_bytes, &expected_sig);

            if result != expected_result {
                println!("Failed verification test for vector #{}", index);
                println!("Expected result: {}", expected_result);
                println!("Actual result: {}", result);
                if fields.len() > 7 {
                    println!("Comment: {}", fields[7]);
                }
                all_passed = false;
                continue;
            }
            println!("Passed verification test");
        }

        assert!(all_passed, "Some test vectors failed");
    }
}


