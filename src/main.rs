use anyhow::{anyhow, Result};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::network::Network;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use cln_plugin;
use hex;
use lightning_invoice::{Currency, InvoiceBuilder};
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
    let destination = args["destination"]
        .as_str()
        .ok_or(anyhow!("Missing mandatory field destination"))?;

    let destination: [u8; 33] = hex::FromHex::from_hex(destination)?;
    let destination = PublicKey::from_slice(&destination[..])?;

    // Payment secret is random
    let mut payment_secret = [0u8; 32];
    SysRng.try_fill_bytes(&mut payment_secret)?;
    let payment_secret = PaymentSecret(payment_secret);

    // Something we don't have a preimage for, and allows downstream nodes to recognize this as a
    // test payment.
    let mut payment_hash = [0xaa; 32];
    SysRng.try_fill_bytes(&mut payment_hash[16..])?;
    let payment_hash = sha256::Hash::from_slice(&payment_hash[..])?;

    // We add a routehint that tells the sender how to get to this non-existent node. The trick is
    // that it has to go through the real destination.
    let rh = RouteHintHop {
        src_node_id: destination,
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

    let invoice = InvoiceBuilder::new(lnradar.currency.clone())
        .payment_hash(payment_hash)
        .amount_milli_satoshis(amount_msat)
        .description("Test invoice".into())
        .current_timestamp()
        .min_final_cltv_expiry_delta(144)
        .payment_secret(payment_secret)
        .private_route(rhs)
        .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &lnradar.private_key))?;
    let response = json!({"bolt11": invoice.to_string()});
    Ok(response)
}
