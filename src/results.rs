use crate::primitives::{Amount, PublicKey, Sha256, ShortChannelIdDir};
use serde::ser::SerializeStruct;
use serde::Serialize;

// FIXME: extend to more error codes
#[repr(u64)]
#[derive(Clone)]
pub enum ErrorCode {
    UnknownNextPeer = 0x400a,
    IncorrectOrUnknownPaymentDetails = 0x400f,
    TemporaryChannelFailure = 0x1007,
    FeeInsufficient = 0x100c,
}

impl ErrorCode {
    pub fn from_u64(n: u64) -> Option<Self> {
        match n {
            n if n == ErrorCode::UnknownNextPeer as u64 => Some(ErrorCode::UnknownNextPeer),
            n if n == ErrorCode::IncorrectOrUnknownPaymentDetails as u64 => {
                Some(ErrorCode::IncorrectOrUnknownPaymentDetails)
            }
            n if n == ErrorCode::TemporaryChannelFailure as u64 => {
                Some(ErrorCode::TemporaryChannelFailure)
            }
            n if n == ErrorCode::FeeInsufficient as u64 => Some(ErrorCode::FeeInsufficient),
            _ => None,
        }
    }

    pub fn to_string(&self) -> String {
        let s = match self {
            ErrorCode::UnknownNextPeer => "UNKNOWN_NEXT_PEER",
            ErrorCode::IncorrectOrUnknownPaymentDetails => "INCORRECT_OR_UNKNOWN_PAYMENT_DETAILS",
            ErrorCode::TemporaryChannelFailure => "TEMPORARY_CHANNEL_FAILURE",
            ErrorCode::FeeInsufficient => "FEE_INSUFFICIENT",
        };
        s.to_string()
    }
}

#[derive(Clone)]
pub struct RouteHop {
    pub short_channel_id_dir: ShortChannelIdDir,
    pub next_nodeid: PublicKey,
    pub amount: Amount,
}

#[derive(Clone)]
pub struct Route {
    pub path: Vec<RouteHop>,
    pub failcode: ErrorCode,
    pub erring_index: usize,
}

pub struct ProbeAttempt {
    pub payment_hash: Sha256,
    pub destination: PublicKey,
    pub amount: Amount,
    pub route: Route,
}

pub enum ProbeStatus {
    Success,
    Failed,
}

pub struct ProbeResult {
    pub payment_hash: Sha256,
    pub destination: PublicKey,
    pub amount: Amount,
    pub routes: Vec<Route>,
    pub status: ProbeStatus,
    pub message: Option<String>,
}

impl ProbeResult {
    pub fn pretty_status(&self) -> String {
        let s = serde_json::to_string(&self.status).unwrap_or_default();
        match &self.message {
            Some(m) => {
                format!("{s} ({m})")
            }
            None => {
                format!("{s}")
            }
        }
    }
}

impl Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let x: u64 = self.clone() as u64;
        serializer.serialize_u64(x)
    }
}

impl Serialize for RouteHop {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("RouteHop", 3)?;
        state.serialize_field("short_channel_id_dir", &self.short_channel_id_dir)?;
        state.serialize_field("next_nodeid", &self.next_nodeid)?;
        state.serialize_field("amount_msat", &self.amount)?;
        state.end()
    }
}

impl Serialize for Route {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("Route", 4)?;
        state.serialize_field("failcode", &self.failcode)?;
        state.serialize_field("failcodename", &self.failcode.to_string())?;
        state.serialize_field("erring_index", &self.erring_index)?;
        state.serialize_field("path", &self.path)?;
        state.end()
    }
}

impl Serialize for ProbeAttempt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ProbeAttempt", 4)?;
        state.serialize_field("payment_hash", &hex::encode(&self.payment_hash))?;
        state.serialize_field("destination", &self.destination)?;
        state.serialize_field("amount_msat", &self.amount)?;
        state.serialize_field("route", &self.route)?;
        state.end()
    }
}

impl Serialize for ProbeStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match *self {
            ProbeStatus::Success => serializer.serialize_unit_variant("ProbeStatus", 0, "Success"),
            ProbeStatus::Failed => serializer.serialize_unit_variant("ProbeStatus", 1, "Failed"),
        }
    }
}

impl Serialize for ProbeResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ProbeAttempt", 6)?;
        state.serialize_field("payment_hash", &hex::encode(&self.payment_hash))?;
        state.serialize_field("destination", &self.destination)?;
        state.serialize_field("amount_msat", &self.amount)?;
        state.serialize_field("routes", &self.routes)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("message", &self.message)?;
        state.end()
    }
}
