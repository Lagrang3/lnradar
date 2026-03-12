use anyhow::{anyhow, Context, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::network::Network;
use bitcoin::secp256k1::Secp256k1;
use cln_plugin::{ConfiguredPlugin, Plugin};
use cln_rpc::model::requests::{
    GetinfoRequest, GetroutesRequest, SendpayRequest, SendpayRoute, WaitsendpayRequest,
};
use cln_rpc::model::responses::GetroutesRoutes;
use cln_rpc::primitives::Amount;
use cln_rpc::ClnRpc;
use hex;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};
use lightning_types::payment::PaymentSecret;
use lightning_types::routing::{RouteHint, RouteHintHop, RoutingFees};
use rand::rngs::SysRng;
use rand::TryRng;
use serde_json::{json, Value};
use std::path::Path;
use std::str::FromStr;

// Incompatible versions of the bitcoin library for cln_rpc and lightning crates makes it
// impossible to interoperably use a single PublicKey struct here.
#[derive(Debug, Clone)]
struct PublicKey(bitcoin::secp256k1::PublicKey);

#[derive(Debug, Clone)]
struct SecretKey(bitcoin::secp256k1::SecretKey);

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
    fn from_secret_key(ctx: &Secp256k1<bitcoin::secp256k1::All>, privk: &SecretKey) -> Self {
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&ctx, &privk.0);
        PublicKey(pk)
    }
    fn from_byte_array(s: [u8; 33]) -> Result<Self> {
        // FIXME: from_slice is deprecated in newer versions of secp256k1
        let k = bitcoin::secp256k1::PublicKey::from_slice(&s[..])?;
        Ok(PublicKey(k))
    }
}

impl SecretKey {
    fn from_byte_array(s: [u8; 32]) -> Result<Self> {
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

trait FromJson {
    fn from_value(value: &Value) -> Result<Self>
    where
        Self: Sized;
}

impl FromJson for PublicKey {
    fn from_value(value: &Value) -> Result<Self> {
        let pk = value.as_str().context("field is missing")?;
        let pk: [u8; 33] = hex::FromHex::from_hex(pk).context("failed converting string to hex")?;
        PublicKey::from_byte_array(pk).context("failed converting hex to PublicKey")
    }
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
        SysRng.try_fill_bytes(&mut payment_secret)?;
        let payment_secret = PaymentSecret(payment_secret);

        // Something we don't have a preimage for, and allows downstream nodes to recognize this as a
        // test payment.
        let mut payment_hash = [0xaa; 32];
        SysRng.try_fill_bytes(&mut payment_hash[16..])?;
        let payment_hash = sha256::Hash::from_slice(&payment_hash[..])?;

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
    plugin.join().await
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
        .context("failed to get mandatory field destination")?;

    let test_payment = TestPayment::new(amount_msat, lnradar.private_key.clone(), destination)?;
    let invoice = test_payment.get_invoice(lnradar.currency.clone())?;

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

async fn testpayment(
    p: cln_plugin::Plugin<LnRadar>,
    args: Value,
) -> Result<Value, cln_plugin::Error> {
    let lnradar = p.state();

    let amount_msat = args["amount_msat"]
        .as_u64()
        .ok_or(anyhow!("Missing mandatory field amount_msat"))?;
    let destination = PublicKey::from_value(&args["destination"])
        .context("failed to get mandatory field destination")?;

    let test_payment = TestPayment::new(amount_msat, lnradar.private_key.clone(), destination)?;

    let mut rpc = ClnRpc::from_plugin(&p).await?;

    let getroutes_req = GetroutesRequest {
        source: lnradar.nodeid.clone().into(),
        destination: test_payment.prev_destination().into(),
        amount_msat: Amount::from_msat(test_payment.prev_amount_msat()),
        layers: vec![
            "auto.no_mpp_support".to_string(),
            "auto.localchans".to_string(),
            "auto.sourcefree".to_string(),
            "xpay".to_string(), // use xpay knowledge
        ],
        // FIXME: how much in fees is acceptable here?
        maxfee_msat: Amount::from_msat(test_payment.prev_amount_msat()),
        maxdelay: None,
        maxparts: Some(1),
        final_cltv: Some(test_payment.prev_delay().into()),
    };
    let getroutes = rpc.call_typed(&getroutes_req).await?;

    // FIXME: maybe it would be better to keep byte types and then convert to specific library
    // types when needed
    let payment_hash =
        cln_rpc::primitives::Sha256::from_bytes_ref(test_payment.payment_hash.as_byte_array());
    let payment_secret =
        cln_rpc::primitives::Secret::try_from(test_payment.payment_secret.0.to_vec())?;
    let partid = 0;
    let groupid = 1;
    let mut sendpay_route = convert_routes(&getroutes.routes[0])?;
    sendpay_route.push(test_payment.final_hop());

    let sendpay_req = SendpayRequest {
        payment_hash: *payment_hash,
        payment_secret: Some(payment_secret),
        route: sendpay_route,
        amount_msat: Some(Amount::from_msat(test_payment.amount_msat)),
        partid: Some(partid),
        groupid: Some(groupid),
        bolt11: None,
        description: None,
        label: None,
        localinvreqid: None,
        payment_metadata: None,
    };
    let sendpay = rpc.call_typed(&sendpay_req).await?;

    // waitsendpay payment_hash [timeout] [partid groupid]
    let waitsendpay_req = WaitsendpayRequest {
        payment_hash: *payment_hash,
        partid: Some(partid),
        groupid: Some(groupid),
        timeout: Some(60),
    };
    let waitsendpay = rpc.call_typed(&waitsendpay_req).await;

    // FIXME: process response
    // FIXME: feed results back to askrene

    Ok(json!({"getroutes": getroutes, "sendpay": sendpay, "waitsendpay": waitsendpay}))
}
