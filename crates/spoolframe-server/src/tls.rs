//! The process's TLS provider.
//!
//! Iroh and the OTLP exporter use aws-lc-rs. reqwest 0.12 (Plex, SSO, OTLP
//! over HTTP) also links ring, and takes the process's provider when one
//! is installed, ring only when none is. With both linked, rustls has no
//! default of its own, so the binary installs aws-lc-rs before anything
//! makes a TLS client: every connection then uses the one provider.

use anyhow::{Result, bail};

/// Installs aws-lc-rs as the process's rustls provider. An error if a
/// provider is already installed: that one would stay, and ring might be it.
pub fn install_provider() -> Result<()> {
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        bail!("a TLS provider was installed before the server's own");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rustls::NamedGroup;
    use rustls::crypto::CryptoProvider;

    use super::*;

    #[test]
    fn the_installed_provider_is_aws_lc_rs() {
        install_provider().expect("no provider is installed before this test");
        let installed = CryptoProvider::get_default().expect("a provider is installed");
        // X25519MLKEM768 is aws-lc-rs's; ring has no ML-KEM.
        assert!(
            installed
                .kx_groups
                .iter()
                .any(|group| group.name() == NamedGroup::X25519MLKEM768),
            "installed key exchanges: {:?}",
            installed.kx_groups
        );
    }
}
