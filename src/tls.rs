//! TLS for QUIC: a self-signed server certificate, pinned by SHA-256 on the client.
//! The pin travels over the authenticated SSH channel, as with mosh's session key.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

use crate::proto::ALPN;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn fingerprint(cert: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, cert))
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub fp: String,
}

/// Load the daemon certificate, or make a new one. A stable certificate keeps the
/// fingerprint the same when the daemon restarts.
pub fn load_or_create(dir: &Path) -> Result<Identity> {
    let cp = dir.join("cert.der");
    let kp = dir.join("key.der");
    if let (Ok(c), Ok(k)) = (std::fs::read(&cp), std::fs::read(&kp)) {
        let fp = fingerprint(&c);
        return Ok(Identity {
            cert: CertificateDer::from(c),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k)),
            fp,
        });
    }
    let ck = rcgen::generate_simple_self_signed(vec!["qsh".to_string()])?;
    let c = ck.cert.der().to_vec();
    let k = ck.signing_key.serialize_der();
    crate::paths::write_private(&kp, &k)?;
    crate::paths::write_private(&cp, &c)?;
    let fp = fingerprint(&c);
    Ok(Identity { cert: CertificateDer::from(c), key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k)), fp })
}

pub fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(60).try_into().unwrap()));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.stream_receive_window((4u32 << 20).into());
    t.receive_window((16u32 << 20).into());
    t.send_window(16 << 20);
    t.max_concurrent_bidi_streams(256u32.into());
    t.initial_rtt(Duration::from_millis(150));
    // BBR: it measures the bottleneck rate and the round trip, so it does
    // not fill the queues of the path. A terminal needs this: a Ctrl-C or a
    // key waits behind every byte in a queue. Loss-based controllers (Cubic)
    // also slow down on each random loss. In tests/netlab, BBR moved output
    // 4-7 times faster on lossy links, and Ctrl-C on a 1 Mbit/s link with a
    // 2 s queue took 0.7 s instead of 3.7 s. QSH_CC=cubic|newreno for
    // measurements.
    match std::env::var("QSH_CC").as_deref() {
        Ok("cubic") => t.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
        Ok("newreno") => t.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default())),
        _ => {
            // quinn starts BBR with 240 KB in flight: 2 s in the queue of a
            // 1 Mbit/s link before BBR knows the rate. Start as RFC 9002 says.
            let mut bbr = quinn::congestion::BbrConfig::default();
            bbr.initial_window(14720);
            t.congestion_controller_factory(Arc::new(Floored(Arc::new(bbr))))
        }
    };
    t
}

/// The smallest congestion window (16 packets, 19 KB: 0.15 s at 1 Mbit/s).
/// On an idle connection, BBR estimates the rate from the liveness pings
/// alone, and its window falls to 4 packets. Pings and keys count against
/// the window, so 4 lost pings would block both until loss detection: seconds
/// on a bad link, and a false "no reply" badge.
const WINDOW_FLOOR: u64 = 16 * 1200;

struct Floored(Arc<quinn::congestion::BbrConfig>);

impl quinn::congestion::ControllerFactory for Floored {
    fn build(self: Arc<Self>, now: std::time::Instant, mtu: u16) -> Box<dyn quinn::congestion::Controller> {
        Box::new(Floor(self.0.clone().build(now, mtu)))
    }
}

/// A congestion controller with a window of at least `WINDOW_FLOOR`.
struct Floor(Box<dyn quinn::congestion::Controller>);

impl quinn::congestion::Controller for Floor {
    fn on_sent(&mut self, now: std::time::Instant, bytes: u64, last_packet_number: u64) {
        self.0.on_sent(now, bytes, last_packet_number)
    }

    fn on_ack(&mut self, now: std::time::Instant, sent: std::time::Instant, bytes: u64, app_limited: bool, rtt: &quinn_proto::RttEstimator) {
        self.0.on_ack(now, sent, bytes, app_limited, rtt)
    }

    fn on_end_acks(&mut self, now: std::time::Instant, in_flight: u64, app_limited: bool, largest_packet_num_acked: Option<u64>) {
        self.0.on_end_acks(now, in_flight, app_limited, largest_packet_num_acked)
    }

    fn on_congestion_event(&mut self, now: std::time::Instant, sent: std::time::Instant, is_persistent_congestion: bool, lost_bytes: u64) {
        self.0.on_congestion_event(now, sent, is_persistent_congestion, lost_bytes)
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.0.on_mtu_update(new_mtu)
    }

    fn window(&self) -> u64 {
        self.0.window().max(WINDOW_FLOOR)
    }

    fn metrics(&self) -> quinn::congestion::ControllerMetrics {
        let mut m = self.0.metrics();
        m.congestion_window = self.window();
        m
    }

    fn clone_box(&self) -> Box<dyn quinn::congestion::Controller> {
        Box::new(Floor(self.0.clone_box()))
    }

    fn initial_window(&self) -> u64 {
        self.0.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

pub fn server_config(id: &Identity) -> Result<quinn::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key())?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    let q = quinn::crypto::rustls::QuicServerConfig::try_from(cfg)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(q));
    sc.transport_config(Arc::new(transport()));
    sc.migration(true);
    Ok(sc)
}

pub fn client_config(fp_hex: &str) -> Result<quinn::ClientConfig> {
    let fp = hex::decode(fp_hex).context("bad fingerprint")?;
    let p = provider();
    let mut cfg = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { fp, provider: p }))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    let q = quinn::crypto::rustls::QuicClientConfig::try_from(cfg)?;
    let mut cc = quinn::ClientConfig::new(Arc::new(q));
    cc.transport_config(Arc::new(transport()));
    Ok(cc)
}

#[derive(Debug)]
struct Pinned {
    fp: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if got.as_ref() == self.fp.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("server certificate fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
