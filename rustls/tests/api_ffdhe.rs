//! This file contains tests that use the test-only FFDHE KX group (defined in submodule `ffdhe`)

#![allow(clippy::duplicate_mod)]

mod common;
use common::*;
use rustls::crypto::CryptoProvider;
use rustls::version::{TLS12, TLS13};
use rustls::{CipherSuite, ClientConfig, NamedGroup};

use super::*;

#[test]
fn config_builder_for_client_rejects_cipher_suites_without_compatible_kx_groups() {
    let bad_crypto_provider = CryptoProvider {
        kx_groups: vec![&ffdhe::FFDHE2048_KX_GROUP],
        cipher_suites: vec![
            provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
        ],
        ..provider::default_provider()
    };

    let build_err = ClientConfig::builder_with_provider(bad_crypto_provider.into())
        .with_safe_default_protocol_versions()
        .unwrap_err()
        .to_string();

    // Current expected error:
    // Ciphersuite TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 requires [ECDHE] key exchange, but no \
    // [ECDHE]-compatible key exchange groups were present in `CryptoProvider`'s `kx_groups` field
    assert!(build_err.contains("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"));
    assert!(build_err.contains("ECDHE"));
    assert!(build_err.contains("key exchange"));
}

#[test]
fn ffdhe_ciphersuite() {
    use provider::cipher_suite;
    use rustls::version::{TLS12, TLS13};

    let test_cases = [
        (&TLS12, ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256),
        (&TLS13, cipher_suite::TLS13_CHACHA20_POLY1305_SHA256),
    ];

    for (expected_protocol, expected_cipher_suite) in test_cases {
        let client_config = finish_client_config(
            KeyType::Rsa2048,
            rustls::ClientConfig::builder_with_provider(ffdhe::ffdhe_provider().into())
                .with_protocol_versions(&[expected_protocol])
                .unwrap(),
        );
        let server_config = finish_server_config(
            KeyType::Rsa2048,
            rustls::ServerConfig::builder_with_provider(ffdhe::ffdhe_provider().into())
                .with_safe_default_protocol_versions()
                .unwrap(),
        );
        do_suite_and_kx_test(
            client_config,
            server_config,
            expected_cipher_suite,
            NamedGroup::FFDHE2048,
            expected_protocol.version(),
        );
    }
}

#[test]
fn server_avoids_dhe_cipher_suites_when_client_has_no_known_dhe_in_groups_ext() {
    use rustls::{CipherSuite, NamedGroup};

    let client_config = finish_client_config(
        KeyType::Rsa2048,
        rustls::ClientConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![
                    ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
                    provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                ],
                kx_groups: vec![&ffdhe::FFDHE4096_KX_GROUP, provider::kx_group::SECP256R1],
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    let server_config = finish_server_config(
        KeyType::Rsa2048,
        rustls::ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![
                    ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
                    provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                ],
                kx_groups: vec![&ffdhe::FFDHE2048_KX_GROUP, provider::kx_group::SECP256R1],
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);
    assert_eq!(
        server
            .negotiated_cipher_suite()
            .unwrap()
            .suite(),
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
    );
    assert_eq!(
        server
            .negotiated_key_exchange_group()
            .unwrap()
            .name(),
        NamedGroup::secp256r1,
    )
}

#[test]
fn server_avoids_cipher_suite_with_no_common_kx_groups() {
    let server_config = finish_server_config(
        KeyType::Rsa2048,
        rustls::ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![
                    provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                    ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
                    provider::cipher_suite::TLS13_AES_128_GCM_SHA256,
                ],
                kx_groups: vec![provider::kx_group::SECP256R1, &ffdhe::FFDHE2048_KX_GROUP],
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    )
    .into();

    let test_cases = [
        (
            // TLS 1.2, have common
            vec![
                // this matches:
                provider::kx_group::SECP256R1,
                &ffdhe::FFDHE2048_KX_GROUP,
            ],
            &TLS12,
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            Some(NamedGroup::secp256r1),
        ),
        (
            vec![
                // this matches:
                provider::kx_group::SECP256R1,
                &ffdhe::FFDHE3072_KX_GROUP,
            ],
            &TLS12,
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            Some(NamedGroup::secp256r1),
        ),
        (
            vec![
                provider::kx_group::SECP384R1,
                // this matches:
                &ffdhe::FFDHE2048_KX_GROUP,
            ],
            &TLS12,
            CipherSuite::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
            Some(NamedGroup::FFDHE2048),
        ),
        (
            // TLS 1.3, have common
            vec![
                // this matches:
                provider::kx_group::SECP256R1,
                &ffdhe::FFDHE2048_KX_GROUP,
            ],
            &TLS13,
            CipherSuite::TLS13_AES_128_GCM_SHA256,
            Some(NamedGroup::secp256r1),
        ),
        (
            vec![
                // this matches:
                provider::kx_group::SECP256R1,
                &ffdhe::FFDHE3072_KX_GROUP,
            ],
            &TLS13,
            CipherSuite::TLS13_AES_128_GCM_SHA256,
            Some(NamedGroup::secp256r1),
        ),
        (
            vec![
                provider::kx_group::SECP384R1,
                // this matches:
                &ffdhe::FFDHE2048_KX_GROUP,
            ],
            &TLS13,
            CipherSuite::TLS13_AES_128_GCM_SHA256,
            Some(NamedGroup::FFDHE2048),
        ),
    ];

    for (client_kx_groups, protocol_version, expected_cipher_suite, expected_group) in test_cases {
        let client_config = finish_client_config(
            KeyType::Rsa2048,
            rustls::ClientConfig::builder_with_provider(
                CryptoProvider {
                    cipher_suites: vec![
                        provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                        ffdhe::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
                        provider::cipher_suite::TLS13_AES_128_GCM_SHA256,
                    ],
                    kx_groups: client_kx_groups,
                    ..provider::default_provider()
                }
                .into(),
            )
            .with_protocol_versions(&[protocol_version])
            .unwrap(),
        )
        .into();

        let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
        do_handshake(&mut client, &mut server);
        assert_eq!(
            server
                .negotiated_cipher_suite()
                .unwrap()
                .suite(),
            expected_cipher_suite
        );
        assert_eq!(server.protocol_version(), Some(protocol_version.version()));
        assert_eq!(
            server
                .negotiated_key_exchange_group()
                .map(|kx| kx.name()),
            expected_group,
        );
    }
}

#[test]
fn non_ffdhe_kx_does_not_have_ffdhe_group() {
    let non_ffdhe = provider::kx_group::SECP256R1;
    assert_eq!(non_ffdhe.ffdhe_group(), None);
    let active = non_ffdhe.start().unwrap();
    assert_eq!(active.ffdhe_group(), None);
}

mod ffdhe {
    use num_bigint::BigUint;
    use rustls::crypto::{
        ActiveKeyExchange, CipherSuiteCommon, CryptoProvider, KeyExchangeAlgorithm, SharedSecret,
        SupportedKxGroup,
    };
    use rustls::ffdhe_groups::FfdheGroup;
    use rustls::{CipherSuite, NamedGroup, SupportedCipherSuite, Tls12CipherSuite, ffdhe_groups};

    use super::provider;

    /// A test-only `CryptoProvider`, only supporting FFDHE key exchange
    pub fn ffdhe_provider() -> CryptoProvider {
        CryptoProvider {
            cipher_suites: FFDHE_CIPHER_SUITES.to_vec(),
            kx_groups: FFDHE_KX_GROUPS.to_vec(),
            ..provider::default_provider()
        }
    }

    static FFDHE_KX_GROUPS: &[&dyn SupportedKxGroup] = &[&FFDHE2048_KX_GROUP, &FFDHE3072_KX_GROUP];

    pub const FFDHE2048_KX_GROUP: FfdheKxGroup =
        FfdheKxGroup(NamedGroup::FFDHE2048, ffdhe_groups::FFDHE2048);
    pub const FFDHE3072_KX_GROUP: FfdheKxGroup =
        FfdheKxGroup(NamedGroup::FFDHE3072, ffdhe_groups::FFDHE3072);
    pub const FFDHE4096_KX_GROUP: FfdheKxGroup =
        FfdheKxGroup(NamedGroup::FFDHE4096, ffdhe_groups::FFDHE4096);

    static FFDHE_CIPHER_SUITES: &[rustls::SupportedCipherSuite] = &[
        TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
        provider::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
    ];

    /// The (test-only) TLS1.2 ciphersuite TLS_DHE_RSA_WITH_AES_128_GCM_SHA256
    pub static TLS_DHE_RSA_WITH_AES_128_GCM_SHA256: SupportedCipherSuite =
        SupportedCipherSuite::Tls12(&TLS12_DHE_RSA_WITH_AES_128_GCM_SHA256);

    static TLS12_DHE_RSA_WITH_AES_128_GCM_SHA256: Tls12CipherSuite =
        match &provider::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 {
            SupportedCipherSuite::Tls12(provider) => Tls12CipherSuite {
                common: CipherSuiteCommon {
                    suite: CipherSuite::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
                    ..provider.common
                },
                kx: KeyExchangeAlgorithm::DHE,
                ..**provider
            },
            _ => unreachable!(),
        };

    #[derive(Debug)]
    pub struct FfdheKxGroup(pub NamedGroup, pub FfdheGroup<'static>);

    impl SupportedKxGroup for FfdheKxGroup {
        fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, rustls::Error> {
            let mut x = vec![0; 64];
            ffdhe_provider()
                .secure_random
                .fill(&mut x)?;
            let x = BigUint::from_bytes_be(&x);

            let p = BigUint::from_bytes_be(self.1.p);
            let g = BigUint::from_bytes_be(self.1.g);

            let x_pub = g.modpow(&x, &p);
            let x_pub = to_bytes_be_with_len(x_pub, self.1.p.len());

            Ok(Box::new(ActiveFfdheKx {
                x_pub,
                x,
                p,
                group: self.1,
                named_group: self.0,
            }))
        }

        fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
            Some(self.1)
        }

        fn name(&self) -> NamedGroup {
            self.0
        }
    }

    struct ActiveFfdheKx {
        x_pub: Vec<u8>,
        x: BigUint,
        p: BigUint,
        group: FfdheGroup<'static>,
        named_group: NamedGroup,
    }

    impl ActiveKeyExchange for ActiveFfdheKx {
        fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, rustls::Error> {
            let peer_pub = BigUint::from_bytes_be(peer_pub_key);
            let secret = peer_pub.modpow(&self.x, &self.p);
            let secret = to_bytes_be_with_len(secret, self.group.p.len());

            Ok(SharedSecret::from(&secret[..]))
        }

        fn pub_key(&self) -> &[u8] {
            &self.x_pub
        }

        fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
            Some(self.group)
        }

        fn group(&self) -> NamedGroup {
            self.named_group
        }
    }

    fn to_bytes_be_with_len(n: BigUint, len_bytes: usize) -> Vec<u8> {
        let mut bytes = n.to_bytes_le();
        bytes.resize(len_bytes, 0);
        bytes.reverse();
        bytes
    }
}
