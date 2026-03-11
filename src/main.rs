use anyhow::{anyhow, Context, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::network::Network;
use bitcoin::secp256k1::{Secp256k1, SecretKey};
use cln_plugin::{ConfiguredPlugin, Plugin};
use cln_rpc::model::requests::{GetinfoRequest, GetroutesRequest};
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
struct PublicKey(cln_rpc::primitives::PublicKey);

impl Into<cln_rpc::primitives::PublicKey> for PublicKey {
    fn into(self) -> cln_rpc::primitives::PublicKey {
        self.0
    }
}

impl Into<bitcoin::secp256k1::PublicKey> for PublicKey {
    fn into(self) -> bitcoin::secp256k1::PublicKey {
        bitcoin::secp256k1::PublicKey::from_slice(&self.0.serialize()).expect("invalid key")
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
    destination: PublicKey,
    payment_secret: PaymentSecret,
    payment_hash: sha256::Hash,
}

impl TestPayment {
    pub fn new(amount_msat: u64, destination: PublicKey) -> Result<Self> {
        // Payment secret is random
        let mut payment_secret = [0u8; 32];
        SysRng.try_fill_bytes(&mut payment_secret)?;
        let payment_secret = PaymentSecret(payment_secret);

        // Something we don't have a preimage for, and allows downstream nodes to recognize this as a
        // test payment.
        let mut payment_hash = [0xaa; 32];
        SysRng.try_fill_bytes(&mut payment_hash[16..])?;
        let payment_hash = sha256::Hash::from_slice(&payment_hash[..])?;

        Ok(Self {
            amount_msat,
            destination,
            payment_secret,
            payment_hash,
        })
    }
    pub fn get_invoice(
        &self,
        currency: Currency,
        private_key: &SecretKey,
    ) -> Result<Bolt11Invoice> {
        // We add a routehint that tells the sender how to get to this non-existent node. The trick is
        // that it has to go through the real destination.
        let rh = RouteHintHop {
            src_node_id: self.destination.clone().into(),
            short_channel_id: (1 << 40) | (1 << 16) | 1,
            fees: RoutingFees {
                base_msat: 0,
                proportional_millionths: 0,
            },
            cltv_expiry_delta: 9,
            htlc_minimum_msat: None,
            htlc_maximum_msat: None,
        };
        let rhs = RouteHint(vec![rh]);

        InvoiceBuilder::new(currency)
            .payment_hash(self.payment_hash)
            .amount_milli_satoshis(self.amount_msat)
            .description("Test invoice".into())
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .payment_secret(self.payment_secret)
            .private_route(rhs)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, private_key))
            .map_err(|e| anyhow!("{e}"))
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
        let pk = cln_rpc::primitives::PublicKey::from_slice(&pk[..])
            .context("failed converting hex to PublicKey")?;
        Ok(PublicKey(pk))
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
    let private_key = SecretKey::from_slice(&[0xaa; 32][..])?;

    let mut rpc = ClnRpc::from_plugin(&plugin).await?;
    let getinfo = rpc.call_typed(&GetinfoRequest {}).await?;
    let nodeid = PublicKey(getinfo.id);

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

    let test_payment = TestPayment::new(amount_msat, destination)?;
    let invoice = test_payment.get_invoice(lnradar.currency.clone(), &lnradar.private_key)?;

    let response = json!({"bolt11": invoice.to_string()});
    Ok(response)
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

    let test_payment = TestPayment::new(amount_msat, destination)?;

    let mut rpc = ClnRpc::from_plugin(&p).await?;

    let getroutes_req = GetroutesRequest {
        source: lnradar.nodeid.clone().into(),
        destination: test_payment.destination.clone().into(),
        amount_msat: Amount::from_msat(test_payment.amount_msat),
        layers: vec![
            "auto.no_mpp_support".to_string(),
            "auto.localchans".to_string(),
            "auto.sourcefree".to_string(),
            "xpay".to_string(), // use xpay knowledge
        ],
        // FIXME: how much in fees is acceptable here?
        maxfee_msat: Amount::from_msat(test_payment.amount_msat),
        maxdelay: None,
        maxparts: Some(1),
        final_cltv: Some(18),
    };
    let getroutes = rpc.call_typed(&getroutes_req).await?;
    // FIXME: create onion
    // FIXME: call injectpaymentonion and wait response
    // FIXME: process response
    // FIXME: feed results back to askrene
    Ok(json!({"getroutes": getroutes}))
}
