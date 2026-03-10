use anyhow::{anyhow, Context, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::network::Network;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use cln_plugin;
use hex;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};
use lightning_types::payment::PaymentSecret;
use lightning_types::routing::{RouteHint, RouteHintHop, RoutingFees};
use rand::rngs::SysRng;
use rand::TryRng;
use serde_json::{json, Value};
use std::str::FromStr;

#[derive(Debug, Clone)]
struct LnRadar {
    pub currency: Currency,
    pub private_key: SecretKey,
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
            src_node_id: self.destination,
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
        PublicKey::from_slice(&pk[..]).context("failed converting hex to PublicKey")
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
        .dynamic()
        .configure()
        .await?
        .unwrap();

    let network = Network::from_str(plugin.configuration().network.as_str())?;

    // The private key used for the final hop. It is well-known so the penultimate hop can
    // decode the onion.
    let private_key = SecretKey::from_slice(&[0xaa; 32][..])?;

    let state = LnRadar {
        currency: network.into(),
        private_key: private_key,
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
