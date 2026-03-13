use anyhow::{anyhow, Result};
use bitcoin::secp256k1::Secp256k1;
use serde_json::Value;

// Incompatible versions of the bitcoin library for cln_rpc and lightning crates makes it
// impossible to interoperably use a single PublicKey struct here.
#[derive(Debug, Clone)]
pub struct PublicKey(bitcoin::secp256k1::PublicKey);

#[derive(Debug, Clone)]
pub struct SecretKey(bitcoin::secp256k1::SecretKey);

impl Into<cln_rpc::primitives::PublicKey> for PublicKey {
    fn into(self) -> cln_rpc::primitives::PublicKey {
        cln_rpc::primitives::PublicKey::from_slice(&self.0.serialize()).expect("invalid key")
    }
}

impl Into<bitcoin::secp256k1::PublicKey> for PublicKey {
    fn into(self) -> bitcoin::secp256k1::PublicKey {
        self.0
    }
}

impl From<cln_rpc::primitives::PublicKey> for PublicKey {
    fn from(pk: cln_rpc::primitives::PublicKey) -> PublicKey {
        let pk = bitcoin::secp256k1::PublicKey::from_slice(&pk.serialize()).expect("invalid key");
        PublicKey(pk)
    }
}
impl From<bitcoin::secp256k1::PublicKey> for PublicKey {
    fn from(pk: bitcoin::secp256k1::PublicKey) -> PublicKey {
        PublicKey(pk)
    }
}

impl PublicKey {
    pub fn from_secret_key(ctx: &Secp256k1<bitcoin::secp256k1::All>, privk: &SecretKey) -> Self {
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&ctx, &privk.0);
        PublicKey(pk)
    }
    pub fn from_byte_array(s: [u8; 33]) -> Result<Self> {
        // FIXME: from_slice is deprecated in newer versions of secp256k1
        let k = bitcoin::secp256k1::PublicKey::from_slice(&s[..])?;
        Ok(PublicKey(k))
    }
}

impl SecretKey {
    pub fn from_byte_array(s: [u8; 32]) -> Result<Self> {
        // FIXME: from_slice is deprecated in newer versions of secp256k1
        let k = bitcoin::secp256k1::SecretKey::from_slice(&s[..])?;
        Ok(SecretKey(k))
    }
}

impl Into<bitcoin::secp256k1::SecretKey> for SecretKey {
    fn into(self) -> bitcoin::secp256k1::SecretKey {
        self.0
    }
}

pub trait FromJson {
    fn from_value(value: &Value) -> Result<Self>
    where
        Self: Sized;
}

impl FromJson for PublicKey {
    fn from_value(value: &Value) -> Result<Self> {
        let pk = value
            .as_str()
            .ok_or(anyhow!("failed to read field as str"))?;
        let pk: [u8; 33] = hex::FromHex::from_hex(pk)
            .map_err(|e| anyhow!("failed converting string to hex: {e}"))?;
        PublicKey::from_byte_array(pk)
            .map_err(|e| anyhow!("failed converting hex to PublicKey: {e}"))
    }
}
