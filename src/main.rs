use anyhow::{anyhow, Context, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::network::Network;
use bitcoin::secp256k1::Secp256k1;
use cln_plugin::{ConfiguredPlugin, Plugin};
use cln_rpc::model::requests::{
    AskreneageRequest, AskrenecreatelayerRequest, AskreneinformchannelInform,
    AskreneinformchannelRequest, AskreneupdatechannelRequest, GetinfoRequest, GetroutesRequest,
    SendpayRequest, SendpayRoute, WaitsendpayRequest,
};
use cln_rpc::model::responses::{GetroutesRoutes, GetroutesRoutesPath};
use cln_rpc::primitives::Amount;
use cln_rpc::ClnRpc;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};
use lightning_types::payment::PaymentSecret;
use lightning_types::routing::{RouteHint, RouteHintHop, RoutingFees};
use rand::rngs::SysRng;
use rand::TryRng;
use serde_json::{json, Value};
use std::path::Path;
use std::str::FromStr;

mod primitives;
use crate::primitives::{FromJson, PublicKey, SecretKey};

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

#[derive(Debug, Clone)]
struct LnRadar {
    pub currency: Currency,
    pub private_key: SecretKey,
    pub nodeid: PublicKey,
}

struct TestPayment {
    amount_msat: u64,
    fake_destination_priv: SecretKey,
    fake_destination_pubkey: PublicKey,
    payment_secret: PaymentSecret,
    payment_hash: sha256::Hash,
    min_final_cltv_expiry: u16,
    route_hint: RouteHintHop,
    ctx: Secp256k1<bitcoin::secp256k1::All>,
}

impl TestPayment {
    pub fn new(
        amount_msat: u64,
        fake_destination_priv: SecretKey,
        real_destination: PublicKey,
    ) -> Result<Self> {
        // Payment secret is random
        let mut payment_secret = [0u8; 32];
        SysRng
            .try_fill_bytes(&mut payment_secret)
            .map_err(|e| anyhow!("failed creating payment_secret: {e}"))?;
        let payment_secret = PaymentSecret(payment_secret);

        // Something we don't have a preimage for, and allows downstream nodes to recognize this as a
        // test payment.
        let mut payment_hash = [0xaa; 32];
        SysRng
            .try_fill_bytes(&mut payment_hash[16..])
            .map_err(|e| anyhow!("failed creating payment_hash: {e}"))?;
        let payment_hash = sha256::Hash::from_slice(&payment_hash[..])
            .map_err(|e| anyhow!("error while converting payment_hash type: {e}"))?;

        let route_hint = RouteHintHop {
            src_node_id: real_destination.into(),
            short_channel_id: (1 << 40) | (1 << 16) | 1,
            fees: RoutingFees {
                base_msat: 0,
                proportional_millionths: 0,
            },
            cltv_expiry_delta: 144,
            htlc_minimum_msat: None,
            htlc_maximum_msat: None,
        };

        let ctx = Secp256k1::new();
        let fake_destination_pubkey = PublicKey::from_secret_key(&ctx, &fake_destination_priv);

        Ok(Self {
            amount_msat,
            fake_destination_priv,
            fake_destination_pubkey,
            payment_secret,
            payment_hash,
            min_final_cltv_expiry: 18,
            route_hint,
            ctx,
        })
    }
    pub fn prev_destination(&self) -> PublicKey {
        self.route_hint.src_node_id.clone().into()
    }
    pub fn prev_delay(&self) -> u16 {
        self.route_hint.cltv_expiry_delta + self.min_final_cltv_expiry
    }
    pub fn prev_amount_msat(&self) -> u64 {
        let p: u64 = self.route_hint.fees.proportional_millionths.into();
        let b: u64 = self.route_hint.fees.base_msat.into();
        self.amount_msat + b + (p * self.amount_msat) / 1000000
    }
    pub fn final_hop(&self) -> SendpayRoute {
        SendpayRoute {
            amount_msat: Amount::from_msat(self.amount_msat),
            delay: self.min_final_cltv_expiry.into(),
            channel: self.route_hint.short_channel_id.into(),
            id: self.fake_destination_pubkey.clone().into(),
        }
    }
    pub fn get_invoice(&self, currency: Currency) -> Result<Bolt11Invoice> {
        // We add a routehint that tells the sender how to get to this non-existent node. The trick is
        // that it has to go through the real destination.
        let rhs = RouteHint(vec![self.route_hint.clone()]);
        let btc_sk: bitcoin::secp256k1::SecretKey = self.fake_destination_priv.clone().into();
        InvoiceBuilder::new(currency)
            .payment_hash(self.payment_hash)
            .amount_milli_satoshis(self.amount_msat)
            .description("Test invoice".into())
            .current_timestamp()
            .min_final_cltv_expiry_delta(self.min_final_cltv_expiry.into())
            .payment_secret(self.payment_secret)
            .private_route(rhs)
            .build_signed(|hash| self.ctx.sign_ecdsa_recoverable(hash, &btc_sk))
            .map_err(|e| anyhow!("{e}"))
    }
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
        match age_layer(
            p.clone(),
            json!({
                    "layer": LNRADAR_LAYER.to_string(),
                    "time_secs": LNRADAR_AGE_TIME}),
        )
        .await
        {
            Ok(_) => {
                log::info!("Aged layer");
            }
            Err(e) => {
                log::warn!("Failed to age layer: {e}");
            }
        }
    }
}

async fn age_layer(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let time_secs = args["time_secs"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field time_secs"))?;
    let layer = args["layer"]
        .as_str()
        .ok_or(anyhow!("Missing mandatory field layer"))?;
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
            layer: LNRADAR_LAYER.to_string(),
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
) -> Result<()> {
    let mut rpc = ClnRpc::from_plugin(&p)
        .await
        .map_err(|e| anyhow!("failed to fetch an rpc channel from plugin: {e}"))?;
    if sendpay_path.len() <= erring_index || getroutes_path.len() <= erring_index {
        return Err(anyhow!("erring_index is out of bounds"));
    }
    let askrene_req = AskreneinformchannelRequest {
        layer: LNRADAR_LAYER.to_string(),
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
        layer: LNRADAR_LAYER.to_string(),
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
    let getroutes = rpc
        .call_typed(&getroutes_req)
        .await
        .map_err(|e| anyhow!("getroutes failed: {e}"))?;

    // FIXME: maybe it would be better to keep byte types and then convert to specific library
    // types when needed
    let payment_hash =
        cln_rpc::primitives::Sha256::from_bytes_ref(test_payment.payment_hash.as_byte_array());
    let payment_secret =
        cln_rpc::primitives::Secret::try_from(test_payment.payment_secret.0.to_vec())
            .map_err(|e| anyhow!("invalid payment secret: {e}"))?;
    let partid = 0;
    let groupid = 1;
    let mut sendpay_route = convert_routes(&getroutes.routes[0])
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
        .clone()
        .data
        .ok_or(anyhow!("data is not present in waitsendpay error"))?;
    let failcode = data["failcode"]
        .as_u64()
        .ok_or(anyhow!("can't read failcode from waitsendpay response"))?;
    let erring_index: usize = data["erring_index"]
        .as_u64()
        .ok_or(anyhow!("can't read erring_index from waitsendpay response"))?
        .try_into()
        .context("failed to convert erring_index into usize")?;

    let getroutes_route = getroutes.routes[0].path.clone();
    match failcode {
        val if val == ErrorCode::UnknownNextPeer as u64 => {
            log::info!("Probe success");
        }
        val if val == ErrorCode::IncorrectOrUnknownPaymentDetails as u64 => {
            log::info!("Probe success, a node that runs testpay");
        }
        val if val == ErrorCode::TemporaryChannelFailure as u64 => {
            log::info!("Probe failed, possibly liquidity constraints");
            match knowledge_bad_channel(p.clone(), erring_index, &sendpay_route, &getroutes_route)
                .await
            {
                Err(e) => {
                    log::warn!("failed to update knowledge for failed channel: {e}");
                }
                _ => {}
            }
        }
        val if val == ErrorCode::FeeInsufficient as u64 => {
            log::info!("Probe failed, fee_insufficient");
            // We could have taken the raw message from waitsendpay to update the channel fees, but
            // this is simpler.
            match knowledge_disable_channel(p.clone(), erring_index, &getroutes_route).await {
                Err(e) => {
                    log::warn!("failed to disable bad channel: {e}");
                }
                _ => {}
            }
        }
        _ => {
            log::warn!("Unrecognized error code: {failcode}");
        }
    };

    match knowledge_good_channels(p.clone(), erring_index, &sendpay_route, &getroutes_route).await {
        Err(e) => {
            log::warn!("failed to update knowledge for good channels: {e}");
        }
        _ => {}
    }

    Ok(json!({"getroutes": getroutes, "sendpay": sendpay, "waitsendpay": waitsendpay}))
}
