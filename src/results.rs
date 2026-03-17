use cln_rpc::model::requests::SendpayRoute;
use cln_rpc::model::responses::{GetroutesRoutesPath, SendpayResponse};
use cln_rpc::RpcError;
use serde::ser::SerializeStruct;
use serde::Serialize;

// FIXME: extend to more error codes
#[repr(u64)]
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
}

pub struct ProbeResult {
    pub getroutes_path: Vec<GetroutesRoutesPath>,
    pub sendpay_route: Vec<SendpayRoute>,
    pub sendpay: SendpayResponse,
    pub waitsendpay: RpcError,
    pub failcode: ErrorCode,
    pub erring_index: usize,
}

impl Serialize for ProbeResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("ProbeResult", 3)?;
        state.serialize_field("getroutes", &self.getroutes_path)?;
        state.serialize_field("sendpay", &self.sendpay)?;
        state.serialize_field("waitsendpay", &self.waitsendpay)?;
        state.end()
    }
}
