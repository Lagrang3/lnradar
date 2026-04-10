use anyhow::Result;
use bitcoin::secp256k1::Secp256k1;
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::str::FromStr;

pub use cln_rpc::primitives::{Amount, ShortChannelIdDir};
pub type Sha256 = [u8; 32];

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

impl Display for PublicKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pk_hex = hex::encode(self.0.serialize());
        write!(f, "{pk_hex}")
    }
}

impl Serialize for PublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl FromStr for PublicKey {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // choose the format you expect; here: JSON
        let pk: [u8; 33] = hex::FromHex::from_hex(s)?;
        let pk = PublicKey::from_byte_array(pk)?;
        Ok(pk)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        PublicKey::from_str(&s)
            .map_err(|e| serde::de::Error::custom(format!("could not parse PublicKey: {e}")))
    }
}

// A struct to keep track of disabled channels, because askrene does not age disabled channels
// knowledge.
#[derive(Debug)]
pub struct DisabledChannel {
    pub scidd: cln_rpc::primitives::ShortChannelIdDir,
    pub time: std::time::SystemTime,
}

impl PartialEq for DisabledChannel {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.scidd == other.scidd
    }
}

impl Eq for DisabledChannel {}

impl Ord for DisabledChannel {
    fn cmp(&self, other: &Self) -> Ordering {
        match other.time.cmp(&self.time) {
            Ordering::Equal => self.scidd.cmp(&other.scidd),
            o => o,
        }
    }
}

impl PartialOrd for DisabledChannel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
