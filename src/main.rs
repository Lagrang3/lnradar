use anyhow::{anyhow, Context, Result};
use bitcoin::hashes::Hash;
use bitcoin::network::Network;
use cln_plugin::{ConfiguredPlugin, Plugin};
use cln_rpc::model::requests::{
    AskreneageRequest, AskrenecreatelayerRequest, AskreneinformchannelInform,
    AskreneinformchannelRequest, AskreneupdatechannelRequest, GetinfoRequest, GetroutesRequest,
    SendpayRequest, SendpayRoute, WaitsendpayRequest,
};
use cln_rpc::model::responses::{GetroutesRoutes, GetroutesRoutesPath, SendpayResponse};
use cln_rpc::primitives::Amount;
use cln_rpc::{ClnRpc, RpcError};
use lightning_invoice::Currency;
use serde_json::{json, Value};
use std::path::Path;
use std::str::FromStr;

mod primitives;
use crate::primitives::{FromJson, PublicKey, SecretKey};

mod testpayment;
use crate::testpayment::TestPayment;

// The default age time of xpay layer set to 1 hour is too small.
// We remove knowledge older than 1 day.
const LNRADAR_LAYER: &str = "lnradar";
const LNRADAR_AGE_TIME: u64 = 86400;

#[repr(u64)]
enum ErrorCode {
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

#[derive(Debug, Clone)]
struct LnRadar {
    pub currency: Currency,
    pub private_key: SecretKey,
    pub nodeid: PublicKey,
}

trait FromPlugin<P> {
    async fn from_plugin(plugin: &P) -> Result<Self>
    where
        Self: Sized;
}

impl<S: Clone + Send> FromPlugin<Plugin<S>> for ClnRpc {
    async fn from_plugin(plugin: &Plugin<S>) -> Result<Self> {
        ClnRpc::new(
            Path::new(&plugin.configuration().lightning_dir).join(plugin.configuration().rpc_file),
        )
        .await
    }
}

impl<
        S: Clone + Send + Sync + 'static,
        I: tokio::io::AsyncRead + Send + Unpin + 'static,
        O: Send + tokio::io::AsyncWrite + Unpin + 'static,
    > FromPlugin<ConfiguredPlugin<S, I, O>> for ClnRpc
{
    async fn from_plugin(plugin: &ConfiguredPlugin<S, I, O>) -> Result<Self> {
        ClnRpc::new(
            Path::new(&plugin.configuration().lightning_dir).join(plugin.configuration().rpc_file),
        )
        .await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plugin = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .rpcmethod(
            "testinvoice",
            "Command to generate a test invoice",
            testinvoice,
        )
        .rpcmethod(
            "testpayment",
            "Command to probe a payment path",
            testpayment,
        )
        .rpcmethod(
            "testpayment-loop",
            "Command to try different probe paths to a destination",
            testpayment_loop,
        )
        .dynamic()
        .configure()
        .await?
        .unwrap();

    let network = Network::from_str(plugin.configuration().network.as_str())?;

    // The private key used for the final hop. It is well-known so the penultimate hop can
    // decode the onion.
    let private_key = SecretKey::from_byte_array([0xaa; 32])?;

    let mut rpc = ClnRpc::from_plugin(&plugin).await?;
    let getinfo = rpc.call_typed(&GetinfoRequest {}).await?;
    let nodeid: PublicKey = getinfo.id.clone().into();

    let state = LnRadar {
        currency: network.into(),
        private_key: private_key,
        nodeid: nodeid,
    };
    let plugin = plugin.start(state).await?;

    let p = plugin.clone();
    tokio::spawn(async move {
        care_of_layers(p).await;
    });

    plugin.join().await
}

async fn care_of_layers(p: cln_plugin::Plugin<LnRadar>) {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .expect("failed to fetch an rpc channel from plugin");
    let askrene_req = AskrenecreatelayerRequest {
        layer: LNRADAR_LAYER.to_string(),
        persistent: None,
    };
    match rpc.call_typed(&askrene_req).await {
        Ok(_) => {
            log::info!("Created layer {LNRADAR_LAYER}");
        }
        Err(e) => {
            log::warn!("Failed to create layer {LNRADAR_LAYER}: {e}");
        }
    }
    loop {
        // every minute apply aging to layers
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        match age_layer(p.clone(), LNRADAR_LAYER, LNRADAR_AGE_TIME).await {
            Ok(_) => {
                log::info!("Aged layer: {LNRADAR_LAYER}");
            }
            Err(e) => {
                log::warn!("Failed to age layer: {e}");
            }
        }
    }
}

async fn age_layer(
    p: cln_plugin::Plugin<LnRadar>,
    layer: &str,
    time_secs: u64,
) -> Result<Value, cln_plugin::Error> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("failed to get current unix time: {e}"))?;
    let cutoff = now.as_secs() - time_secs;
    let askrene_req = AskreneageRequest {
        layer: layer.to_string(),
        cutoff: cutoff,
    };
    let response = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-age failed: {e}"))?;
    Ok(json!(response))
}

async fn testinvoice(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let lnradar = p.state();

    // FIXME: implement positional arguments and amount_msat from string conversion
    let amount_msat = args["amount_msat"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field amount_msat"))?;
    let destination = PublicKey::from_value(&args["destination"])
        .map_err(|e| anyhow!("failed to get mandatory field destination: {e}"))?;

    let test_payment = TestPayment::new(amount_msat, lnradar.private_key.clone(), destination)
        .map_err(|e| anyhow!("failed to create test_payment: {e}"))?;
    let invoice = test_payment
        .get_invoice(lnradar.currency.clone())
        .map_err(|e| anyhow!("failed to create invoice: {e}"))?;

    let response = json!({"bolt11": invoice.to_string()});
    Ok(response)
}

fn convert_routes(r: &GetroutesRoutes) -> Result<Vec<SendpayRoute>> {
    let sp_route: Result<Vec<SendpayRoute>> = r
        .path
        .iter()
        .map(|hop| -> Result<SendpayRoute> {
            let short_channel_id = hop
                .short_channel_id_dir
                .ok_or(anyhow!("hop in {r:?} is missing a short_channel_id_dir"))?
                .short_channel_id;
            Ok(SendpayRoute {
                amount_msat: hop.amount_msat,
                delay: hop.delay,
                channel: short_channel_id,
                id: hop.next_node_id,
            })
        })
        .collect();

    // Since there is an offset in getroutes hops we need an extra step to fix that
    let mut sp_route = sp_route?;
    let n = sp_route.len();
    for i in 1..n {
        sp_route[i - 1].amount_msat = sp_route[i].amount_msat;
        sp_route[i - 1].delay = sp_route[i].delay;
    }
    sp_route[n - 1].amount_msat = r.amount_msat;
    sp_route[n - 1].delay = r
        .final_cltv
        .ok_or(anyhow!("routes {r:?} is missing the final_cltv"))?;
    Ok(sp_route)
}

// We can't just use getroutes_path because the amounts are wrong.
// We can't just use sendpay_path because the channels are identified as short_channel_id instead
// of the needed short_channel_id_dir.
async fn knowledge_good_channels(
    p: cln_plugin::Plugin<LnRadar>,
    erring_index: usize,
    sendpay_path: &Vec<SendpayRoute>,
    getroutes_path: &Vec<GetroutesRoutesPath>,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    if sendpay_path.len() < erring_index || getroutes_path.len() < erring_index {
        return Err(anyhow!("erring_index is out of bounds"));
    }
    for (sp_hop, gr_hop) in std::iter::zip(
        &sendpay_path[..erring_index],
        &getroutes_path[..erring_index],
    ) {
        let askrene_req = AskreneinformchannelRequest {
            layer: layer.to_string(),
            amount_msat: Some(sp_hop.amount_msat),
            short_channel_id_dir: gr_hop.short_channel_id_dir,
            inform: Some(AskreneinformchannelInform::UNCONSTRAINED),
        };
        let _ = rpc
            .call_typed(&askrene_req)
            .await
            .map_err(|e| anyhow!("askrene-inform-channel failed: {e}"))?;
    }
    Ok(())
}

async fn knowledge_bad_channel(
    p: cln_plugin::Plugin<LnRadar>,
    erring_index: usize,
    sendpay_path: &Vec<SendpayRoute>,
    getroutes_path: &Vec<GetroutesRoutesPath>,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    if sendpay_path.len() <= erring_index || getroutes_path.len() <= erring_index {
        return Err(anyhow!("erring_index is out of bounds"));
    }
    let askrene_req = AskreneinformchannelRequest {
        layer: layer.to_string(),
        amount_msat: Some(sendpay_path[erring_index].amount_msat),
        short_channel_id_dir: getroutes_path[erring_index].short_channel_id_dir,
        inform: Some(AskreneinformchannelInform::CONSTRAINED),
    };
    let _ = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-inform-channel failed: {e}"))?;
    Ok(())
}

async fn knowledge_disable_channel(
    p: cln_plugin::Plugin<LnRadar>,
    erring_index: usize,
    getroutes_path: &Vec<GetroutesRoutesPath>,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    if getroutes_path.len() <= erring_index {
        return Err(anyhow!("erring_index is out of bounds"));
    }
    let scidd = getroutes_path[erring_index]
        .short_channel_id_dir
        .ok_or(anyhow!(
            "missing short_channel_id_dir in hop {:?}",
            getroutes_path[erring_index]
        ))?;
    let askrene_req = AskreneupdatechannelRequest {
        layer: layer.to_string(),
        short_channel_id_dir: scidd,
        enabled: Some(false),
        cltv_expiry_delta: None,
        fee_base_msat: None,
        fee_proportional_millionths: None,
        htlc_minimum_msat: None,
        htlc_maximum_msat: None,
    };
    let _ = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-update-channel failed: {e}"))?;
    Ok(())
}

struct ProbeResult {
    getroutes_path: Vec<GetroutesRoutesPath>,
    sendpay_route: Vec<SendpayRoute>,
    sendpay: SendpayResponse,
    waitsendpay: RpcError,
    failcode: ErrorCode,
    erring_index: usize,
}

async fn send_probe(
    p: cln_plugin::Plugin<LnRadar>,
    test_payment: &TestPayment,
    groupid: u64,
) -> Result<ProbeResult> {
    let lnradar = p.state();
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin {e}"))?;

    let getroutes_req = GetroutesRequest {
        source: lnradar.nodeid.clone().into(),
        destination: test_payment.prev_destination().into(),
        amount_msat: Amount::from_msat(test_payment.prev_amount_msat()),
        layers: vec![
            "auto.no_mpp_support".to_string(),
            "auto.localchans".to_string(),
            "auto.sourcefree".to_string(),
            "xpay".to_string(),        // use xpay knowledge
            LNRADAR_LAYER.to_string(), // also bring our own layer
        ],
        // FIXME: how much in fees is acceptable here?
        maxfee_msat: Amount::from_msat(test_payment.prev_amount_msat()),
        maxdelay: None,
        maxparts: Some(1),
        final_cltv: Some(test_payment.prev_delay().into()),
    };
    let mut getroutes = rpc
        .call_typed(&getroutes_req)
        .await
        .map_err(|e| anyhow!("getroutes failed: {e}"))?;
    if getroutes.routes.len() != 1 {
        return Err(anyhow!(
            "Expecting getroutes to return exactly one route, got {} instead.",
            getroutes.routes.len()
        ));
    }

    // FIXME: maybe it would be better to keep byte types and then convert to specific library
    // types when needed
    let payment_hash =
        cln_rpc::primitives::Sha256::from_bytes_ref(test_payment.payment_hash.as_byte_array());
    let payment_secret =
        cln_rpc::primitives::Secret::try_from(test_payment.payment_secret.0.to_vec())
            .map_err(|e| anyhow!("invalid payment secret: {e}"))?;
    let partid = 0;
    let mut sendpay_route = convert_routes(&getroutes.routes[0])
        .map_err(|e| anyhow!("couldn't convert routes types: {e}"))?;
    let getroutes_path = getroutes.routes.remove(0).path;
    sendpay_route.push(test_payment.final_hop());

    let sendpay_req = SendpayRequest {
        payment_hash: *payment_hash,
        payment_secret: Some(payment_secret),
        route: sendpay_route.clone(),
        amount_msat: Some(Amount::from_msat(test_payment.amount_msat)),
        partid: Some(partid),
        groupid: Some(groupid),
        bolt11: None,
        description: None,
        label: None,
        localinvreqid: None,
        payment_metadata: None,
    };
    let sendpay = rpc
        .call_typed(&sendpay_req)
        .await
        .map_err(|e| anyhow!("sendpay failed: {e}"))?;

    // waitsendpay payment_hash [timeout] [partid groupid]
    let waitsendpay_req = WaitsendpayRequest {
        payment_hash: *payment_hash,
        partid: Some(partid),
        groupid: Some(groupid),
        timeout: Some(60),
    };
    let waitsendpay = rpc
        .call_typed(&waitsendpay_req)
        .await
        .err()
        .ok_or(anyhow!("unexpected waitsendpay success"))?;

    let data = waitsendpay
        .data
        .clone()
        .ok_or(anyhow!("data is not present in waitsendpay error"))?;
    let raw_failcode = data["failcode"]
        .as_u64()
        .ok_or(anyhow!("can't read failcode from waitsendpay response"))?;
    let erring_index: usize = data["erring_index"]
        .as_u64()
        .ok_or(anyhow!("can't read erring_index from waitsendpay response"))?
        .try_into()
        .context("failed to convert erring_index into usize")?;

    let failcode = match ErrorCode::from_u64(raw_failcode) {
        Some(e) => e,
        None => {
            return Err(anyhow!(
                "Unrecognized error code from waitsendpay: {waitsendpay}"
            ));
        }
    };

    Ok(ProbeResult {
        getroutes_path,
        sendpay_route,
        sendpay,
        waitsendpay,
        failcode,
        erring_index,
    })
}

async fn update_knowledge(
    p: cln_plugin::Plugin<LnRadar>,
    results: &ProbeResult,
    layer: &str,
) -> Result<()> {
    match results.failcode {
        ErrorCode::TemporaryChannelFailure => {
            knowledge_bad_channel(
                p.clone(),
                results.erring_index,
                &results.sendpay_route,
                &results.getroutes_path,
                layer,
            )
            .await
            .map_err(|e| anyhow!("failed to update knowledge fro failed channel: {e}"))?;
        }
        ErrorCode::FeeInsufficient => {
            // We could have taken the raw message from waitsendpay to update the channel fees, but
            // this is simpler.
            knowledge_disable_channel(
                p.clone(),
                results.erring_index,
                &results.getroutes_path,
                layer,
            )
            .await
            .map_err(|e| anyhow!("failed to disable bad channel: {e}"))?;
        }
        _ => {}
    };

    knowledge_good_channels(
        p.clone(),
        results.erring_index,
        &results.sendpay_route,
        &results.getroutes_path,
        layer,
    )
    .await
    .map_err(|e| anyhow!("failed to update knowledge for good channels: {e}"))?;
    Ok(())
}

async fn testpayment(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let lnradar = p.state();

    // we use map_err instead of context because the plugin error message only contains the context
    // and not the details of the error
    let amount_msat = args["amount_msat"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field amount_msat"))?;
    let destination = PublicKey::from_value(&args["destination"])
        .map_err(|e| anyhow!("failed to get mandatory field destination: {e}"))?;

    log::trace!(
        "testpayment called with amount_msat: {} and destination: {}",
        args["amount_msat"],
        args["destination"]
    );

    let test_payment = TestPayment::new(amount_msat, lnradar.private_key.clone(), destination)
        .map_err(|e| anyhow!("failed to create a test payment: {e}"))?;

    let results = send_probe(p.clone(), &test_payment, 1).await?;

    match update_knowledge(p.clone(), &results, LNRADAR_LAYER).await {
        Ok(_) => {}
        Err(e) => {
            log::warn!("Failed to update knowledge: {e}");
        }
    };

    match results.failcode {
        ErrorCode::UnknownNextPeer => {
            log::info!("Probe success");
        }
        ErrorCode::IncorrectOrUnknownPaymentDetails => {
            log::info!("Probe success, a node that runs testpay");
        }
        _ => {
            log::info!("Probe failed");
        }
    };
    Ok(
        json!({"getroutes": results.getroutes_path, "sendpay": results.sendpay, "waitsendpay": results.waitsendpay}),
    )
}

async fn probe_loop(
    p: cln_plugin::Plugin<LnRadar>,
    test_payment: &TestPayment,
) -> Result<ProbeResult> {
    let mut groupid: u64 = 0;
    loop {
        groupid += 1;
        let results = send_probe(p.clone(), &test_payment, groupid).await?;
        match update_knowledge(p.clone(), &results, LNRADAR_LAYER).await {
            Ok(_) => {}
            Err(e) => {
                log::warn!("Failed to update knowledge: {e}");
            }
        };
        match results.failcode {
            ErrorCode::UnknownNextPeer | ErrorCode::IncorrectOrUnknownPaymentDetails => {
                return Ok(results);
            }
            _ => {
                log::info!("Probe failed");
                continue;
            }
        };
    }
}

async fn testpayment_loop(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let lnradar = p.state();

    // we use map_err instead of context because the plugin error message only contains the context
    // and not the details of the error
    let amount_msat = args["amount_msat"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field amount_msat"))?;
    let destination = PublicKey::from_value(&args["destination"])
        .map_err(|e| anyhow!("failed to get mandatory field destination: {e}"))?;

    log::trace!(
        "testpayment called with amount_msat: {} and destination: {}",
        args["amount_msat"],
        args["destination"]
    );

    let test_payment = TestPayment::new(amount_msat, lnradar.private_key.clone(), destination)
        .map_err(|e| anyhow!("failed to create a test payment: {e}"))?;

    let results = tokio::select! {
        r = probe_loop(p, &test_payment) => {
            r?
        },
        _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
            return Err(anyhow!("time out while waiting for probe loop to finish"));
        }
    };
    Ok(json!({
        "getroutes": results.getroutes_path,
        "sendpay": results.sendpay,
        "waitsendpay": results.waitsendpay}))
}
