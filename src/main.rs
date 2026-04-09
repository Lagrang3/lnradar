use crate::primitives::{Amount, ShortChannelIdDir};
use anyhow::{anyhow, Context, Result};
use bitcoin::network::Network;
use cln_plugin::options::DefaultBooleanConfigOption;
use cln_rpc::model::requests::{
    AskreneageRequest, AskrenecreatelayerRequest, AskreneinformchannelInform,
    AskreneinformchannelRequest, AskrenelistlayersRequest, AskreneupdatechannelRequest,
    GetinfoRequest, GetroutesRequest, ListnodesRequest, SendpayRequest, SendpayRoute,
    WaitsendpayRequest,
};
use cln_rpc::model::responses::GetroutesRoutes;
use cln_rpc::ClnRpc;
use lightning_invoice::Currency;
use rand::seq::IndexedRandom;
use serde_json::{json, Value};
use std::collections::BinaryHeap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::pin;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

mod primitives;
use crate::primitives::{DisabledChannel, FromJson, PublicKey, SecretKey};

mod testpayment;
use crate::testpayment::TestPayment;

mod results;
use crate::results::ErrorCode;
use crate::results::{ProbeAttempt, ProbeResult, ProbeStatus, Route, RouteHop};

mod error;
use crate::error::Error;

mod util;
use crate::util::FromPlugin;

// The default age time of xpay layer set to 1 hour is too small.
// We remove knowledge older than 1 day.
const LNRADAR_LAYER: &str = "lnradar";
// Time in seconds after which we consider knowledge to be obsolete.
const LNRADAR_AGE_TIME_SECS: u64 = 86400;
// maximum number of concurrent probes
const LNRADAR_MAX_CONCURRENT_PROBES: usize = 5;
// timeout for probes
const LNRADAR_DEFAULT_TIMEOUT_SECS: u64 = 60;

static SEM_PROBES: Semaphore = Semaphore::const_new(LNRADAR_MAX_CONCURRENT_PROBES);

static IS_PAYMENT_LAYER_OPT: DefaultBooleanConfigOption =
    DefaultBooleanConfigOption::new_bool_with_default(
        "lnradar-payment-layer",
        false,
        "Use \"lnradar\" as a payment layer.",
    );

#[derive(Clone)]
struct LnRadar {
    pub currency: Currency,
    pub private_key: SecretKey,
    pub disabled: Arc<Mutex<BinaryHeap<DisabledChannel>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plugin = cln_plugin::Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(IS_PAYMENT_LAYER_OPT.clone())
        .rpcmethod(
            "testinvoice",
            "Command to generate a test invoice",
            json_testinvoice,
        )
        .rpcmethod(
            "testpayment",
            "Command to probe a payment path",
            json_testpayment,
        )
        .rpcmethod(
            "testpayment-loop",
            "Command to try different probe paths to a destination",
            json_testpayment_loop,
        )
        .rpcmethod(
            "testnetwork-loop",
            "Command to try different probe paths to random destinations",
            json_testnetwork_loop,
        )
        .dynamic()
        .configure()
        .await?
        .unwrap();

    let network = Network::from_str(plugin.configuration().network.as_str())?;

    // FIXME: can we get the nodeid at this stage without breaking the plugin with the rpc_command
    // hook?

    // The private key used for the final hop. It is well-known so the penultimate hop can
    // decode the onion.
    let private_key = SecretKey::from_byte_array([0xaa; 32])?;

    let state = LnRadar {
        currency: network.into(),
        private_key: private_key,
        disabled: Arc::new(Mutex::new(BinaryHeap::new())),
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

    match rpc
        .call_typed(&AskrenelistlayersRequest {
            layer: Some(LNRADAR_LAYER.to_string()),
        })
        .await
    {
        Err(_) => {
            // layer does not exist

            match rpc
                .call_typed(&AskrenecreatelayerRequest {
                    layer: LNRADAR_LAYER.to_string(),
                    persistent: Some(true),
                })
                .await
            {
                Ok(_) => {
                    log::info!("Created layer \"{LNRADAR_LAYER}\"");
                }
                Err(e) => {
                    log::warn!("Failed to create layer \"{LNRADAR_LAYER}\": {e}");
                }
            }
        }
        Ok(_) => {
            // layer already exists
        }
    }
    loop {
        // every minute apply aging to layers
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        match age_layer(p.clone(), LNRADAR_LAYER, LNRADAR_AGE_TIME_SECS).await {
            Ok(_) => {
                log::info!("Aged layer: {LNRADAR_LAYER}");
            }
            Err(e) => {
                log::warn!("Failed to age layer: {e}");
            }
        }

        match age_disabled(p.clone(), LNRADAR_LAYER, LNRADAR_AGE_TIME_SECS).await {
            Ok(n) => {
                log::info!("Re-enabled {n} channels.");
            }
            Err(e) => {
                log::warn!("Failed to re-enable channels on layer \"{LNRADAR_LAYER}\": {e}");
            }
        }
    }
}

async fn age_disabled(
    p: cln_plugin::Plugin<LnRadar>,
    layer: &str,
    age_time_secs: u64,
) -> Result<usize, cln_plugin::Error> {
    let time_delta = std::time::Duration::from_secs(age_time_secs);
    let cutoff = std::time::SystemTime::now()
        .checked_sub(time_delta)
        .ok_or(anyhow!("cutoff goes out of bounds"))?;
    let mut disabled_heap = p.state().disabled.lock().await;
    let mut n: usize = 0;
    while let Some(chan) = disabled_heap.peek() {
        if chan.time > cutoff {
            break;
        }

        let scidd = chan.scidd.clone();
        let scidd_str = serde_json::to_string(&scidd).unwrap_or_default();
        match reenable_channel(p.clone(), scidd.clone(), layer).await {
            Ok(_) => {
                n += 1;
                log::info!("re-enabed channel {scidd_str}");
                disabled_heap.pop();
            }
            Err(e) => {
                log::warn!("failed to re-enable channel {scidd_str}: {e}");
                break;
            }
        }
    }
    Ok(n)
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

async fn json_testinvoice(
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

fn convert_routes(r: &GetroutesRoutes) -> Result<(Vec<SendpayRoute>, Vec<RouteHop>)> {
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

    let path: Result<Vec<RouteHop>> = sp_route
        .iter()
        .zip(r.path.iter())
        .map(|(sp_hop, gr_hop)| -> Result<RouteHop> {
            let scidd = gr_hop
                .short_channel_id_dir
                .ok_or(anyhow!("hop {gr_hop:?} is missing a short_channel_id_dir"))?;
            Ok(RouteHop {
                short_channel_id_dir: scidd,
                next_nodeid: gr_hop.next_node_id.into(),
                amount: sp_hop.amount_msat,
            })
        })
        .collect();
    let path = path?;
    Ok((sp_route, path))
}

// We can't just use getroutes_path because the amounts are wrong.
// We can't just use sendpay_path because the channels are identified as short_channel_id instead
// of the needed short_channel_id_dir.
async fn knowledge_good_channels(
    p: cln_plugin::Plugin<LnRadar>,
    erring_index: usize,
    path: &Vec<RouteHop>,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    if path.len() < erring_index {
        return Err(anyhow!("erring_index is out of bounds"));
    }
    for hop in &path[..erring_index] {
        let askrene_req = AskreneinformchannelRequest {
            layer: layer.to_string(),
            amount_msat: Some(hop.amount),
            short_channel_id_dir: Some(hop.short_channel_id_dir),
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
    path: &Vec<RouteHop>,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    let hop = path
        .get(erring_index)
        .ok_or(anyhow!("erring_index is out of bounds"))?;
    let askrene_req = AskreneinformchannelRequest {
        layer: layer.to_string(),
        amount_msat: Some(hop.amount),
        short_channel_id_dir: Some(hop.short_channel_id_dir),
        inform: Some(AskreneinformchannelInform::CONSTRAINED),
    };
    let _ = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-inform-channel failed: {e}"))?;
    Ok(())
}

async fn disable_channel(
    p: cln_plugin::Plugin<LnRadar>,
    scidd: ShortChannelIdDir,
    layer: &str,
) -> Result<()> {
    let lnradar = p.state();
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    let askrene_req = AskreneupdatechannelRequest {
        layer: layer.to_string(),
        short_channel_id_dir: scidd.clone(),
        enabled: Some(false),
        cltv_expiry_delta: None,
        fee_base_msat: None,
        fee_proportional_millionths: None,
        htlc_minimum_msat: None,
        htlc_maximum_msat: None,
    };

    let mut disabled_heap = lnradar.disabled.lock().await;
    disabled_heap.push(DisabledChannel {
        scidd: scidd,
        time: std::time::SystemTime::now(),
    });

    let _ = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-update-channel failed: {e}"))?;
    log::debug!("Disabled channel {scidd}");
    Ok(())
}

async fn reenable_channel(
    p: cln_plugin::Plugin<LnRadar>,
    scidd: ShortChannelIdDir,
    layer: &str,
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    let askrene_req = AskreneupdatechannelRequest {
        layer: layer.to_string(),
        short_channel_id_dir: scidd.clone(),
        enabled: Some(true),
        cltv_expiry_delta: None,
        fee_base_msat: None,
        fee_proportional_millionths: None,
        htlc_minimum_msat: None,
        htlc_maximum_msat: None,
    };
    // FIXME: this is not enough, we should be able to remove entries.
    let _ = rpc
        .call_typed(&askrene_req)
        .await
        .map_err(|e| anyhow!("askrene-update-channel failed: {e}"))?;

    Ok(())
}

async fn knowledge_disable_channel(
    p: cln_plugin::Plugin<LnRadar>,
    erring_index: usize,
    path: &Vec<RouteHop>,
    layer: &str,
) -> Result<()> {
    let scidd = path
        .get(erring_index)
        .ok_or(anyhow!("erring_index is out of bounds"))?
        .short_channel_id_dir;

    disable_channel(p, scidd, layer).await
}

async fn send_probe(
    p: cln_plugin::Plugin<LnRadar>,
    test_payment: &TestPayment,
    groupid: u64,
) -> Result<ProbeAttempt, Error> {
    // Only a limited number of probes is allowed to run concurrently, due to the limited available
    // number of HTLC slots in local channels.
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin {e}"))?;

    let getinfo = rpc.call_typed(&GetinfoRequest {}).await.map_err(|e| {
        Error::other(format!(
            "Failed to call getinfo and the node id is needed: {e}"
        ))
    })?;
    let nodeid: PublicKey = getinfo.id.clone().into();

    let getroutes_req = GetroutesRequest {
        source: nodeid.into(),
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
    let getroutes = rpc
        .call_typed(&getroutes_req)
        .await
        .map_err(|e| Error::no_routes(e.to_string()))?;
    if getroutes.routes.len() != 1 {
        return Err(Error::other(format!(
            "Expecting getroutes to return exactly one route, got {} instead.",
            getroutes.routes.len()
        )));
    }

    // FIXME: maybe it would be better to keep byte types and then convert to specific library
    // types when needed
    let payment_hash = cln_rpc::primitives::Sha256::from_bytes_ref(&test_payment.payment_hash);
    let payment_secret =
        cln_rpc::primitives::Secret::try_from(test_payment.payment_secret.0.to_vec())
            .map_err(|e| anyhow!("invalid payment secret: {e}"))?;
    let partid = 0;
    let (mut sendpay_route, path) = convert_routes(&getroutes.routes[0])
        .map_err(|e| anyhow!("couldn't convert routes types: {e}"))?;
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
    let _sendpay = rpc
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
            return Err(Error::other(format!(
                "Unrecognized error code from waitsendpay: {waitsendpay}"
            )));
        }
    };

    Ok(ProbeAttempt {
        payment_hash: test_payment.payment_hash,
        destination: test_payment.prev_destination(),
        amount: Amount::from_msat(test_payment.amount_msat),
        route: Route {
            path,
            failcode,
            erring_index,
        },
    })
}

async fn update_knowledge(
    p: cln_plugin::Plugin<LnRadar>,
    results: &ProbeAttempt,
    layer: &str,
) -> Result<()> {
    match results.route.failcode {
        ErrorCode::TemporaryChannelFailure => {
            knowledge_bad_channel(
                p.clone(),
                results.route.erring_index,
                &results.route.path,
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
                results.route.erring_index,
                &results.route.path,
                layer,
            )
            .await
            .map_err(|e| anyhow!("failed to disable bad channel: {e}"))?;
        }
        _ => {}
    };

    knowledge_good_channels(
        p.clone(),
        results.route.erring_index,
        &results.route.path,
        layer,
    )
    .await
    .map_err(|e| anyhow!("failed to update knowledge for good channels: {e}"))?;
    Ok(())
}

async fn json_testpayment(
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

    match results.route.failcode {
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
    Ok(json!(results))
}

async fn probe_loop_timeout(
    p: cln_plugin::Plugin<LnRadar>,
    test_payment: &TestPayment,
    timeout: tokio::time::Duration,
) -> ProbeResult {
    let _probe = match SEM_PROBES.acquire().await {
        Ok(p) => p,
        Err(e) => {
            return ProbeResult {
                payment_hash: test_payment.payment_hash,
                destination: test_payment.prev_destination(),
                amount: Amount::from_msat(test_payment.amount_msat),
                routes: vec![],
                status: ProbeStatus::Failed,
                message: Some(format!("failed to acquire probe semaphore: {e}")),
            };
        }
    };

    let timer = tokio::time::sleep(timeout);
    pin!(timer);

    let mut groupid: u64 = 0;
    let nodeid_str = serde_json::to_string(&test_payment.prev_destination()).unwrap_or_default();
    let mut routes = vec![];
    let result: ProbeResult;

    loop {
        groupid += 1;

        let attempt = tokio::select! {
            sp = send_probe(p.clone(), &test_payment, groupid) => {
                sp
            },
            _ = &mut timer => {
                Err(Error::other(format!("time out while waiting for probe loop to finish")))
            }
        };

        let attempt = match attempt {
            Ok(r) => r,
            Err(e) => {
                result = ProbeResult {
                    payment_hash: test_payment.payment_hash,
                    destination: test_payment.prev_destination(),
                    amount: Amount::from_msat(test_payment.amount_msat),
                    routes: routes,
                    status: ProbeStatus::Failed,
                    message: Some(format!("{e}")),
                };
                break;
            }
        };
        routes.push(attempt.route.clone());
        match update_knowledge(p.clone(), &attempt, LNRADAR_LAYER).await {
            Ok(_) => {}
            Err(e) => {
                log::warn!("Failed to update knowledge: {e}");
            }
        };
        match attempt.route.failcode {
            ErrorCode::UnknownNextPeer | ErrorCode::IncorrectOrUnknownPaymentDetails => {
                log::info!("Probe success, nodeid={nodeid_str}");
                result = ProbeResult {
                    payment_hash: attempt.payment_hash,
                    destination: attempt.destination,
                    amount: attempt.amount,
                    routes: routes,
                    status: ProbeStatus::Success,
                    message: None,
                };
                break;
            }
            _ => {
                log::info!("Probe failed, nodeid={nodeid_str}");
                continue;
            }
        };
    }

    drop(_probe);
    result
}

async fn json_testpayment_loop(
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

    let r = probe_loop_timeout(
        p,
        &test_payment,
        tokio::time::Duration::from_secs(LNRADAR_DEFAULT_TIMEOUT_SECS),
    )
    .await;
    Ok(json!(r))
}

// FIXME: on success returns an array of ProbeResult
async fn json_testnetwork_loop(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let amount_msat = args["amount_msat"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field amount_msat"))?;
    let n = args["num_destinations"].as_u64().unwrap_or(10);

    log::trace!(
        "testpayment called with amount_msat: {} and num_destinations: {}",
        args["amount_msat"],
        args["num_destinations"]
    );

    let mut rpc = ClnRpc::from_plugin(&p).await?;

    let nodes: Vec<_> = rpc.call_typed(&ListnodesRequest { id: None }).await?.nodes;
    let mut rng: rand::rngs::StdRng = rand::make_rng();
    let nodes: Vec<_> = nodes
        .sample(&mut rng, n as usize)
        .map(|x| x.nodeid.clone())
        .collect();

    let mut set = JoinSet::new();
    let mut results = vec![];

    for n in nodes {
        let plugin = p.clone();
        let nodeid = n.clone();
        set.spawn(async move {
            let lnradar = plugin.state();
            let destination: PublicKey = nodeid.into();
            let test_payment = match TestPayment::new(
                amount_msat,
                lnradar.private_key.clone(),
                destination.clone(),
            ) {
                Ok(t) => t,
                Err(e) => {
                    return ProbeResult {
                        payment_hash: [0u8; 32],
                        destination: destination,
                        amount: Amount::from_msat(amount_msat),
                        routes: vec![],
                        status: ProbeStatus::Failed,
                        message: Some(format!("failed to create a test payment: {e}")),
                    };
                }
            };
            probe_loop_timeout(
                plugin,
                &test_payment,
                tokio::time::Duration::from_secs(LNRADAR_DEFAULT_TIMEOUT_SECS),
            )
            .await
        });
    }

    while let Some(res) = set.join_next().await {
        let res = res?;
        results.push(res);
    }

    Ok(json!(results))
}
