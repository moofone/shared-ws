use std::sync::OnceLock;

/// rustls 0.23 requires a process-level CryptoProvider selection.
///
/// If multiple dependencies enable both `rustls` providers (`ring` and `aws-lc-rs`),
/// rustls cannot infer a default and will panic during the first TLS config build.
/// Installing a provider explicitly avoids that failure.
pub fn install_rustls_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Prefer ring (portable, no external deps). If some other part of the process already
        // installed a provider, this will return Err(Arc<_>); that's fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
