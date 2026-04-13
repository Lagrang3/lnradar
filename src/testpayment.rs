use crate::primitives::{Amount, PublicKey, SecretKey, Sha256};
use anyhow::{anyhow, Result};
use bitcoin::secp256k1::Secp256k1;
use cln_rpc::model::requests::SendpayRoute;
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};
use lightning_types::payment::PaymentSecret;
use lightning_types::routing::{RouteHint, RouteHintHop, RoutingFees};
use rand::rngs::SysRng;
use rand::TryRng;

pub struct TestPayment {
    pub amount_msat: u64,
    pub fake_destination_priv: SecretKey,
    pub fake_destination_pubkey: PublicKey,
    pub payment_secret: PaymentSecret,
    pub payment_hash: Sha256,
    pub min_final_cltv_expiry: u16,
    pub route_hint: RouteHintHop,
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
            .try_fill_bytes(&mut payment_hash[..])
            .map_err(|e| anyhow!("failed creating payment_hash: {e}"))?;

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
        let payment_hash = bitcoin::hashes::sha256::Hash::from_bytes_ref(&self.payment_hash);
        InvoiceBuilder::new(currency)
            .payment_hash(*payment_hash)
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
