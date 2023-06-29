use beserial::{Deserialize, Serialize};
use nimiq_keys::SecureGenerate;
use wasm_bindgen::prelude::*;

use super::{bls_public_key::BLSPublicKey, bls_secret_key::BLSSecretKey};

/// A BLS keypair
/// It is used by validators to vote during Tendermint rounds.
/// This is just a wrapper around our internal BLS structs
#[wasm_bindgen]
pub struct BLSKeyPair {
    inner: nimiq_bls::KeyPair,
}

#[wasm_bindgen]
impl BLSKeyPair {
    /// Generates a new keypair from secure randomness.
    pub fn generate() -> BLSKeyPair {
        let key_pair = nimiq_bls::KeyPair::generate_default_csprng();
        BLSKeyPair::from_native(key_pair)
    }

    /// Derives a keypair from an existing private key.
    pub fn derive(private_key: &BLSSecretKey) -> BLSKeyPair {
        let key_pair = nimiq_bls::KeyPair::from(private_key.native_ref().clone());
        BLSKeyPair::from_native(key_pair)
    }

    /// Deserializes a keypair from a byte array.
    pub fn unserialize(bytes: &[u8]) -> Result<BLSKeyPair, JsError> {
        let key_pair = nimiq_bls::KeyPair::deserialize(&mut &*bytes)?;
        Ok(BLSKeyPair::from_native(key_pair))
    }

    #[wasm_bindgen(constructor)]
    pub fn new(secret_key: &BLSSecretKey, public_key: &BLSPublicKey) -> BLSKeyPair {
        let key_pair = nimiq_bls::KeyPair {
            secret_key: secret_key.native_ref().clone(),
            public_key: *public_key.native_ref(),
        };
        BLSKeyPair::from_native(key_pair)
    }

    /// Serializes to a byte array.
    pub fn serialize(&self) -> Vec<u8> {
        let mut vec = self.inner.serialize_to_vec();
        vec.push(0);
        vec
    }

    /// Gets the keypair's secret key.
    #[wasm_bindgen(getter, js_name = secretKey)]
    pub fn secret_key(&self) -> BLSSecretKey {
        BLSSecretKey::from_native(self.inner.secret_key.clone())
    }

    /// Gets the keypair's public key.
    #[wasm_bindgen(getter, js_name = publicKey)]
    pub fn public_key(&self) -> BLSPublicKey {
        BLSPublicKey::from_native(self.inner.public_key)
    }

    /// Formats the keypair into a hex string.
    #[wasm_bindgen(js_name = toHex)]
    pub fn to_hex(&self) -> String {
        hex::encode(self.serialize())
    }
}

impl BLSKeyPair {
    pub fn from_native(key_pair: nimiq_bls::KeyPair) -> BLSKeyPair {
        BLSKeyPair { inner: key_pair }
    }

    pub fn native_ref(&self) -> &nimiq_bls::KeyPair {
        &self.inner
    }
}