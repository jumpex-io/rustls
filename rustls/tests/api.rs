//! Assorted public API tests.

#![allow(clippy::disallowed_types, clippy::duplicate_mod)]

use std::fmt::Debug;
use std::io::{self, BufRead, IoSlice, Read, Write};
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fmt, mem};

use pki_types::{CertificateDer, DnsName, IpAddr, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::client::{ResolvesClientCert, Resumption, verify_server_cert_signed_by_trust_anchor};
use rustls::crypto::{ActiveKeyExchange, CryptoProvider, SharedSecret, SupportedKxGroup};
use rustls::internal::msgs::base::Payload;
use rustls::internal::msgs::codec::Codec;
use rustls::internal::msgs::enums::{AlertLevel, ExtensionType};
use rustls::internal::msgs::message::{Message, MessagePayload, PlainMessage};
use rustls::server::{CertificateType, ClientHello, ParsedCertificate, ResolvesServerCert};
use rustls::version::TLS12;
use rustls::{
    AlertDescription, CertificateError, CipherSuite, ClientConfig, ClientConnection,
    ConnectionCommon, ConnectionTrafficSecrets, ContentType, DistinguishedName, Error,
    ExtendedKeyPurpose, HandshakeKind, HandshakeType, InconsistentKeys, InvalidMessage, KeyLog,
    NamedGroup, PeerIncompatible, PeerMisbehaved, ProtocolVersion, RootCertStore, ServerConfig,
    ServerConnection, SideData, SignatureScheme, Stream, StreamOwned, SupportedCipherSuite,
    SupportedProtocolVersion, sign,
};
#[cfg(feature = "aws-lc-rs")]
use rustls::{
    client::{EchConfig, EchGreaseConfig, EchMode},
    crypto::aws_lc_rs::hpke::ALL_SUPPORTED_SUITES,
    internal::msgs::base::PayloadU16,
    internal::msgs::handshake::{
        EchConfigContents, EchConfigPayload, HpkeKeyConfig, HpkeSymmetricCipherSuite,
    },
    pki_types::EchConfigListBytes,
};
use webpki::anchor_from_trusted_cert;

use super::*;

mod common;
use common::*;
use provider::cipher_suite;
use provider::sign::RsaSigningKey;

mod test_raw_keys {
    use super::*;

    #[test]
    fn successful_raw_key_connection_and_correct_peer_certificates() {
        let provider = provider::default_provider();
        for kt in KeyType::all_for_provider(&provider) {
            let client_config = make_client_config_with_raw_key_support(*kt, &provider);
            let server_config = make_server_config_with_raw_key_support(*kt, &provider);

            let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
            do_handshake(&mut client, &mut server);

            // Test that the client peer certificate is the server's public key
            match client.peer_certificates() {
                Some(certificates) => {
                    assert_eq!(certificates.len(), 1);
                    let cert: CertificateDer<'_> = certificates[0].clone();
                    assert_eq!(cert.as_ref(), kt.get_spki().as_ref());
                }
                None => {
                    unreachable!("Client should have received a certificate")
                }
            }

            // Test that the server peer certificate is the client's public key
            match server.peer_certificates() {
                Some(certificates) => {
                    assert_eq!(certificates.len(), 1);
                    let cert = certificates[0].clone();
                    assert_eq!(cert.as_ref(), kt.get_client_spki().as_ref());
                }
                None => {
                    unreachable!("Server should have received a certificate")
                }
            }
        }
    }

    #[test]
    fn correct_certificate_type_extensions_from_client_hello() {
        let provider = provider::default_provider();
        for kt in KeyType::all_for_provider(&provider) {
            let client_config = make_client_config_with_raw_key_support(*kt, &provider);
            let mut server_config = make_server_config_with_raw_key_support(*kt, &provider);

            server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
                expected_client_cert_types: Some(vec![CertificateType::RawPublicKey]),
                expected_server_cert_types: Some(vec![CertificateType::RawPublicKey]),
                ..Default::default()
            });

            let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
            let err = do_handshake_until_error(&mut client, &mut server);
            assert!(err.is_err());
        }
    }

    #[test]
    fn only_client_supports_raw_keys() {
        let provider = provider::default_provider();
        for kt in KeyType::all_for_provider(&provider) {
            let client_config_rpk = make_client_config_with_raw_key_support(*kt, &provider);
            let server_config = make_server_config(*kt, &provider);

            let (mut client_rpk, mut server) =
                make_pair_for_configs(client_config_rpk, server_config);

            // The client
            match do_handshake_until_error(&mut client_rpk, &mut server) {
                Err(err) => {
                    assert_eq!(
                        err,
                        ErrorFromPeer::Server(Error::PeerIncompatible(
                            PeerIncompatible::IncorrectCertificateTypeExtension
                        ))
                    )
                }
                _ => {
                    unreachable!("Expected error because client is incorrectly configured")
                }
            }
        }
    }

    #[test]
    fn only_server_supports_raw_keys() {
        let provider = provider::default_provider();
        for kt in KeyType::all_for_provider(&provider) {
            let client_config =
                make_client_config_with_versions(*kt, &[&rustls::version::TLS13], &provider);
            let server_config_rpk = make_server_config_with_raw_key_support(*kt, &provider);

            let (mut client, mut server_rpk) =
                make_pair_for_configs(client_config, server_config_rpk);

            match do_handshake_until_error(&mut client, &mut server_rpk) {
                Err(err) => {
                    assert_eq!(
                        err,
                        ErrorFromPeer::Server(Error::PeerIncompatible(
                            PeerIncompatible::IncorrectCertificateTypeExtension
                        ))
                    )
                }
                _ => {
                    unreachable!("Expected error because client is incorrectly configured")
                }
            }
        }
    }
}

fn alpn_test_error(
    server_protos: Vec<Vec<u8>>,
    client_protos: Vec<Vec<u8>>,
    agreed: Option<&[u8]>,
    expected_error: Option<ErrorFromPeer>,
) {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.alpn_protocols = server_protos;

    let server_config = Arc::new(server_config);

    for version in rustls::ALL_VERSIONS {
        let mut client_config =
            make_client_config_with_versions(KeyType::Rsa2048, &[version], &provider);
        client_config
            .alpn_protocols
            .clone_from(&client_protos);

        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        assert_eq!(client.alpn_protocol(), None);
        assert_eq!(server.alpn_protocol(), None);
        let error = do_handshake_until_error(&mut client, &mut server);
        assert_eq!(client.alpn_protocol(), agreed);
        assert_eq!(server.alpn_protocol(), agreed);
        assert_eq!(error.err(), expected_error);
    }
}

fn alpn_test(server_protos: Vec<Vec<u8>>, client_protos: Vec<Vec<u8>>, agreed: Option<&[u8]>) {
    alpn_test_error(server_protos, client_protos, agreed, None)
}

#[test]
fn alpn() {
    // no support
    alpn_test(vec![], vec![], None);

    // server support
    alpn_test(vec![b"server-proto".to_vec()], vec![], None);

    // client support
    alpn_test(vec![], vec![b"client-proto".to_vec()], None);

    // no overlap
    alpn_test_error(
        vec![b"server-proto".to_vec()],
        vec![b"client-proto".to_vec()],
        None,
        Some(ErrorFromPeer::Server(Error::NoApplicationProtocol)),
    );

    // server chooses preference
    alpn_test(
        vec![b"server-proto".to_vec(), b"client-proto".to_vec()],
        vec![b"client-proto".to_vec(), b"server-proto".to_vec()],
        Some(b"server-proto"),
    );

    // case sensitive
    alpn_test_error(
        vec![b"PROTO".to_vec()],
        vec![b"proto".to_vec()],
        None,
        Some(ErrorFromPeer::Server(Error::NoApplicationProtocol)),
    );
}

#[test]
fn connection_level_alpn_protocols() {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let server_config = Arc::new(server_config);

    // Config specifies `h2`
    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.alpn_protocols = vec![b"h2".to_vec()];
    let client_config = Arc::new(client_config);

    // Client relies on config-specified `h2`, server agrees
    let mut client =
        ClientConnection::new(client_config.clone(), server_name("localhost")).unwrap();
    let mut server = ServerConnection::new(server_config.clone()).unwrap();
    do_handshake_until_error(&mut client, &mut server).unwrap();
    assert_eq!(client.alpn_protocol(), Some(&b"h2"[..]));

    // Specify `http/1.1` for the connection, server agrees
    let mut client = ClientConnection::new_with_alpn(
        client_config,
        server_name("localhost"),
        vec![b"http/1.1".to_vec()],
    )
    .unwrap();
    let mut server = ServerConnection::new(server_config).unwrap();
    do_handshake_until_error(&mut client, &mut server).unwrap();
    assert_eq!(client.alpn_protocol(), Some(&b"http/1.1"[..]));
}

fn version_test(
    client_versions: &[&'static rustls::SupportedProtocolVersion],
    server_versions: &[&'static rustls::SupportedProtocolVersion],
    result: Option<ProtocolVersion>,
) {
    let provider = provider::default_provider();
    let client_versions = if client_versions.is_empty() {
        rustls::ALL_VERSIONS
    } else {
        client_versions
    };
    let server_versions = if server_versions.is_empty() {
        rustls::ALL_VERSIONS
    } else {
        server_versions
    };

    let client_config =
        make_client_config_with_versions(KeyType::Rsa2048, client_versions, &provider);
    let server_config =
        make_server_config_with_versions(KeyType::Rsa2048, server_versions, &provider);

    println!("version {client_versions:?} {server_versions:?} -> {result:?}");

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(client.protocol_version(), None);
    assert_eq!(server.protocol_version(), None);
    if result.is_none() {
        let err = do_handshake_until_error(&mut client, &mut server);
        assert!(err.is_err());
    } else {
        do_handshake(&mut client, &mut server);
        assert_eq!(client.protocol_version(), result);
        assert_eq!(server.protocol_version(), result);
    }
}

#[test]
fn versions() {
    // default -> 1.3
    version_test(&[], &[], Some(ProtocolVersion::TLSv1_3));

    // client default, server 1.2 -> 1.2
    version_test(
        &[],
        &[&rustls::version::TLS12],
        Some(ProtocolVersion::TLSv1_2),
    );

    // client 1.2, server default -> 1.2
    version_test(
        &[&rustls::version::TLS12],
        &[],
        Some(ProtocolVersion::TLSv1_2),
    );

    // client 1.2, server 1.3 -> fail
    version_test(&[&rustls::version::TLS12], &[&rustls::version::TLS13], None);

    // client 1.3, server 1.2 -> fail
    version_test(&[&rustls::version::TLS13], &[&rustls::version::TLS12], None);

    // client 1.3, server 1.2+1.3 -> 1.3
    version_test(
        &[&rustls::version::TLS13],
        &[&rustls::version::TLS12, &rustls::version::TLS13],
        Some(ProtocolVersion::TLSv1_3),
    );

    // client 1.2+1.3, server 1.2 -> 1.2
    version_test(
        &[&rustls::version::TLS13, &rustls::version::TLS12],
        &[&rustls::version::TLS12],
        Some(ProtocolVersion::TLSv1_2),
    );
}

fn check_read(reader: &mut dyn io::Read, bytes: &[u8]) {
    let mut buf = vec![0u8; bytes.len() + 1];
    assert_eq!(bytes.len(), reader.read(&mut buf).unwrap());
    assert_eq!(bytes, &buf[..bytes.len()]);
}

fn check_read_err(reader: &mut dyn io::Read, err_kind: io::ErrorKind) {
    let mut buf = vec![0u8; 1];
    let err = reader.read(&mut buf).unwrap_err();
    assert!(matches!(err, err  if err.kind()  == err_kind))
}

fn check_fill_buf(reader: &mut dyn io::BufRead, bytes: &[u8]) {
    let b = reader.fill_buf().unwrap();
    assert_eq!(b, bytes);
    let len = b.len();
    reader.consume(len);
}

fn check_fill_buf_err(reader: &mut dyn io::BufRead, err_kind: io::ErrorKind) {
    let err = reader.fill_buf().unwrap_err();
    assert!(matches!(err, err  if err.kind()  == err_kind))
}

#[test]
fn config_builder_for_client_rejects_empty_kx_groups() {
    assert_eq!(
        ClientConfig::builder_with_provider(
            CryptoProvider {
                kx_groups: Vec::default(),
                ..provider::default_provider()
            }
            .into()
        )
        .with_safe_default_protocol_versions()
        .err(),
        Some(Error::General("no kx groups configured".into()))
    );
}

#[test]
fn config_builder_for_client_rejects_empty_cipher_suites() {
    assert_eq!(
        ClientConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: Vec::default(),
                ..provider::default_provider()
            }
            .into()
        )
        .with_safe_default_protocol_versions()
        .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[test]
fn config_builder_for_client_rejects_incompatible_cipher_suites() {
    assert_eq!(
        ClientConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![cipher_suite::TLS13_AES_256_GCM_SHA384],
                ..provider::default_provider()
            }
            .into()
        )
        .with_protocol_versions(&[&rustls::version::TLS12])
        .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[test]
fn config_builder_for_server_rejects_empty_kx_groups() {
    assert_eq!(
        ServerConfig::builder_with_provider(
            CryptoProvider {
                kx_groups: Vec::default(),
                ..provider::default_provider()
            }
            .into()
        )
        .with_safe_default_protocol_versions()
        .err(),
        Some(Error::General("no kx groups configured".into()))
    );
}

#[test]
fn config_builder_for_server_rejects_empty_cipher_suites() {
    assert_eq!(
        ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: Vec::default(),
                ..provider::default_provider()
            }
            .into()
        )
        .with_safe_default_protocol_versions()
        .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[test]
fn config_builder_for_server_rejects_incompatible_cipher_suites() {
    assert_eq!(
        ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![cipher_suite::TLS13_AES_256_GCM_SHA384],
                ..provider::default_provider()
            }
            .into()
        )
        .with_protocol_versions(&[&rustls::version::TLS12])
        .err(),
        Some(Error::General("no usable cipher suites configured".into()))
    );
}

#[test]
fn config_builder_for_client_with_time() {
    ClientConfig::builder_with_details(
        provider::default_provider().into(),
        Arc::new(rustls::time_provider::DefaultTimeProvider),
    )
    .with_safe_default_protocol_versions()
    .unwrap();
}

#[test]
fn config_builder_for_server_with_time() {
    ServerConfig::builder_with_details(
        provider::default_provider().into(),
        Arc::new(rustls::time_provider::DefaultTimeProvider),
    )
    .with_safe_default_protocol_versions()
    .unwrap();
}

#[test]
fn buffered_client_data_sent() {
    let provider = provider::default_provider();
    let server_config = Arc::new(make_server_config(KeyType::Rsa2048, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config =
            make_client_config_with_versions(KeyType::Rsa2048, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        assert_eq!(5, client.writer().write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut server.reader(), b"hello");
    }
}

#[test]
fn buffered_server_data_sent() {
    let provider = provider::default_provider();
    let server_config = Arc::new(make_server_config(KeyType::Rsa2048, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config =
            make_client_config_with_versions(KeyType::Rsa2048, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        assert_eq!(5, server.writer().write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();

        check_read(&mut client.reader(), b"hello");
    }
}

#[test]
fn buffered_both_data_sent() {
    let provider = provider::default_provider();
    let server_config = Arc::new(make_server_config(KeyType::Rsa2048, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config =
            make_client_config_with_versions(KeyType::Rsa2048, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        assert_eq!(
            12,
            server
                .writer()
                .write(b"from-server!")
                .unwrap()
        );
        assert_eq!(
            12,
            client
                .writer()
                .write(b"from-client!")
                .unwrap()
        );

        do_handshake(&mut client, &mut server);

        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut client.reader(), b"from-server!");
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[test]
fn client_can_get_server_cert() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_configs(client_config, make_server_config(*kt, &provider));
            do_handshake(&mut client, &mut server);

            let certs = client.peer_certificates();
            assert_eq!(certs, Some(kt.get_chain().as_slice()));
        }
    }
}

#[test]
fn client_can_get_server_cert_after_resumption() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = make_server_config(*kt, &provider);
        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone());
            do_handshake(&mut client, &mut server);
            assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));

            let original_certs = client.peer_certificates();

            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone());
            do_handshake(&mut client, &mut server);
            assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));

            let resumed_certs = client.peer_certificates();

            assert_eq!(original_certs, resumed_certs);
        }
    }
}

#[test]
fn client_only_attempts_resumption_with_compatible_security() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    CountingLogger::install();
    CountingLogger::reset();

    let server_config = make_server_config(kt, &provider);
    for version in rustls::ALL_VERSIONS {
        let base_client_config = make_client_config_with_versions(kt, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_configs(base_client_config.clone(), server_config.clone());
        do_handshake(&mut client, &mut server);
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));

        // base case
        let (mut client, mut server) =
            make_pair_for_configs(base_client_config.clone(), server_config.clone());
        do_handshake(&mut client, &mut server);
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));

        // allowed case, using `clone`
        let client_config = ClientConfig::clone(&base_client_config);
        let (mut client, mut server) =
            make_pair_for_configs(client_config.clone(), server_config.clone());
        do_handshake(&mut client, &mut server);
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));

        // disallowed case: unmatching `client_auth_cert_resolver`
        let mut client_config = ClientConfig::clone(&base_client_config);
        client_config.client_auth_cert_resolver =
            make_client_config_with_versions_with_auth(kt, &[version], &provider)
                .client_auth_cert_resolver;

        CountingLogger::reset();
        let (mut client, mut server) =
            make_pair_for_configs(client_config.clone(), server_config.clone());
        do_handshake(&mut client, &mut server);
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
        #[cfg(feature = "log")]
        assert!(COUNTS.with(|c| {
            c.borrow().trace.iter().any(|item| {
                item == "resumption not allowed between different ResolvesClientCert values"
            })
        }));

        // disallowed case: unmatching `verifier`
        let mut client_config =
            make_client_config_with_versions_with_auth(kt, &[version], &provider);
        client_config.resumption = base_client_config.resumption.clone();
        client_config.client_auth_cert_resolver = base_client_config
            .client_auth_cert_resolver
            .clone();

        CountingLogger::reset();
        let (mut client, mut server) =
            make_pair_for_configs(client_config.clone(), server_config.clone());
        do_handshake(&mut client, &mut server);
        assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
        #[cfg(feature = "log")]
        assert!(COUNTS.with(|c| {
            c.borrow()
                .trace
                .iter()
                .any(|item| item == "resumption not allowed between different ServerCertVerifiers")
        }));
    }
}

#[test]
fn server_can_get_client_cert() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(
            *kt, &provider,
        ));

        for version in rustls::ALL_VERSIONS {
            let client_config =
                make_client_config_with_versions_with_auth(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
            do_handshake(&mut client, &mut server);

            let certs = server.peer_certificates();
            assert_eq!(certs, Some(kt.get_client_chain().as_slice()));
        }
    }
}

#[test]
fn server_can_get_client_cert_after_resumption() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(
            *kt, &provider,
        ));

        for version in rustls::ALL_VERSIONS {
            let client_config =
                make_client_config_with_versions_with_auth(*kt, &[version], &provider);
            let client_config = Arc::new(client_config);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &server_config);
            do_handshake(&mut client, &mut server);
            let original_certs = server.peer_certificates();

            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &server_config);
            do_handshake(&mut client, &mut server);
            let resumed_certs = server.peer_certificates();
            assert_eq!(original_certs, resumed_certs);
        }
    }
}

#[test]
fn resumption_combinations() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = make_server_config(*kt, &provider);
        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone());
            do_handshake(&mut client, &mut server);

            let expected_kx = expected_kx_for_version(version);

            assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
            assert_eq!(server.handshake_kind(), Some(HandshakeKind::Full));
            assert_eq!(
                client
                    .negotiated_key_exchange_group()
                    .unwrap()
                    .name(),
                expected_kx
            );
            assert_eq!(
                server
                    .negotiated_key_exchange_group()
                    .unwrap()
                    .name(),
                expected_kx
            );

            let (mut client, mut server) =
                make_pair_for_configs(client_config.clone(), server_config.clone());
            do_handshake(&mut client, &mut server);

            assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));
            assert_eq!(server.handshake_kind(), Some(HandshakeKind::Resumed));
            if *version == &TLS12 {
                assert!(
                    client
                        .negotiated_key_exchange_group()
                        .is_none()
                );
                assert!(
                    server
                        .negotiated_key_exchange_group()
                        .is_none()
                );
            } else {
                assert_eq!(
                    client
                        .negotiated_key_exchange_group()
                        .unwrap()
                        .name(),
                    expected_kx
                );
                assert_eq!(
                    server
                        .negotiated_key_exchange_group()
                        .unwrap()
                        .name(),
                    expected_kx
                );
            }
        }
    }
}

#[test]
fn test_config_builders_debug() {
    if !provider_is_ring() {
        return;
    }

    let b = ServerConfig::builder_with_provider(
        CryptoProvider {
            cipher_suites: vec![cipher_suite::TLS13_CHACHA20_POLY1305_SHA256],
            kx_groups: vec![provider::kx_group::X25519],
            ..provider::default_provider()
        }
        .into(),
    );
    let _ = format!("{b:?}");
    let b = server_config_builder_with_versions(
        &[&rustls::version::TLS13],
        &provider::default_provider(),
    );
    let _ = format!("{b:?}");
    let b = b.with_no_client_auth();
    let _ = format!("{b:?}");

    let b = ClientConfig::builder_with_provider(
        CryptoProvider {
            cipher_suites: vec![cipher_suite::TLS13_CHACHA20_POLY1305_SHA256],
            kx_groups: vec![provider::kx_group::X25519],
            ..provider::default_provider()
        }
        .into(),
    );
    let _ = format!("{b:?}");
    let b = client_config_builder_with_versions(
        &[&rustls::version::TLS13],
        &provider::default_provider(),
    );
    let _ = format!("{b:?}");
}

/// Test that the server handles combination of `offer_client_auth()` returning true
/// and `client_auth_mandatory` returning `Some(false)`. This exercises both the
/// client's and server's ability to "recover" from the server asking for a client
/// certificate and not being given one.
#[test]
fn server_allow_any_anonymous_or_authenticated_client() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    for client_cert_chain in [None, Some(kt.get_client_chain())] {
        let client_auth_roots = get_client_root_store(kt);
        let client_auth = webpki_client_verifier_builder(client_auth_roots.clone(), &provider)
            .allow_unauthenticated()
            .build()
            .unwrap();

        let server_config = server_config_builder(&provider)
            .with_client_cert_verifier(client_auth)
            .with_single_cert(kt.get_chain(), kt.get_key())
            .unwrap();
        let server_config = Arc::new(server_config);

        for version in rustls::ALL_VERSIONS {
            let client_config = if client_cert_chain.is_some() {
                make_client_config_with_versions_with_auth(kt, &[version], &provider)
            } else {
                make_client_config_with_versions(kt, &[version], &provider)
            };
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
            do_handshake(&mut client, &mut server);

            let certs = server.peer_certificates();
            assert_eq!(certs, client_cert_chain.as_deref());
        }
    }
}

fn check_read_and_close(reader: &mut dyn io::Read, expect: &[u8]) {
    check_read(reader, expect);
    assert!(matches!(reader.read(&mut [0u8; 5]), Ok(0)));
}

#[test]
fn server_close_notify() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions_with_auth(kt, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
        do_handshake(&mut client, &mut server);

        // check that alerts don't overtake appdata
        assert_eq!(
            12,
            server
                .writer()
                .write(b"from-server!")
                .unwrap()
        );
        assert_eq!(
            12,
            client
                .writer()
                .write(b"from-client!")
                .unwrap()
        );
        server.send_close_notify();

        transfer(&mut server, &mut client);
        let io_state = client.process_new_packets().unwrap();
        assert!(io_state.peer_has_closed());
        check_read_and_close(&mut client.reader(), b"from-server!");

        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[test]
fn client_close_notify() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions_with_auth(kt, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
        do_handshake(&mut client, &mut server);

        // check that alerts don't overtake appdata
        assert_eq!(
            12,
            server
                .writer()
                .write(b"from-server!")
                .unwrap()
        );
        assert_eq!(
            12,
            client
                .writer()
                .write(b"from-client!")
                .unwrap()
        );
        client.send_close_notify();

        transfer(&mut client, &mut server);
        let io_state = server.process_new_packets().unwrap();
        assert!(io_state.peer_has_closed());
        check_read_and_close(&mut server.reader(), b"from-client!");

        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        check_read(&mut client.reader(), b"from-server!");
    }
}

#[test]
fn server_closes_uncleanly() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let server_config = Arc::new(make_server_config(kt, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
        do_handshake(&mut client, &mut server);

        // check that unclean EOF reporting does not overtake appdata
        assert_eq!(
            12,
            server
                .writer()
                .write(b"from-server!")
                .unwrap()
        );
        assert_eq!(
            12,
            client
                .writer()
                .write(b"from-client!")
                .unwrap()
        );

        transfer(&mut server, &mut client);
        transfer_eof(&mut client);
        let io_state = client.process_new_packets().unwrap();
        assert!(!io_state.peer_has_closed());
        check_read(&mut client.reader(), b"from-server!");

        check_read_err(
            &mut client.reader() as &mut dyn io::Read,
            io::ErrorKind::UnexpectedEof,
        );

        // may still transmit pending frames
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read(&mut server.reader(), b"from-client!");
    }
}

#[test]
fn client_closes_uncleanly() {
    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let server_config = Arc::new(make_server_config(kt, &provider));

    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version], &provider);
        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
        do_handshake(&mut client, &mut server);

        // check that unclean EOF reporting does not overtake appdata
        assert_eq!(
            12,
            server
                .writer()
                .write(b"from-server!")
                .unwrap()
        );
        assert_eq!(
            12,
            client
                .writer()
                .write(b"from-client!")
                .unwrap()
        );

        transfer(&mut client, &mut server);
        transfer_eof(&mut server);
        let io_state = server.process_new_packets().unwrap();
        assert!(!io_state.peer_has_closed());
        check_read(&mut server.reader(), b"from-client!");

        check_read_err(
            &mut server.reader() as &mut dyn io::Read,
            io::ErrorKind::UnexpectedEof,
        );

        // may still transmit pending frames
        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        check_read(&mut client.reader(), b"from-server!");
    }
}

#[test]
fn test_tls13_valid_early_plaintext_alert() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    // Perform the start of a TLS 1.3 handshake, sending a client hello to the server.
    // The client will not have written a CCS or any encrypted messages to the server yet.
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    // Inject a plaintext alert from the client. The server should accept this since:
    //  * It hasn't decrypted any messages from the peer yet.
    //  * The message content type is Alert.
    //  * The payload size is indicative of a plaintext alert message.
    //  * The negotiated protocol version is TLS 1.3.
    server
        .read_tls(&mut io::Cursor::new(&build_alert(
            AlertLevel::Fatal,
            AlertDescription::UnknownCA,
            &[],
        )))
        .unwrap();

    // The server should process the plaintext alert without error.
    assert_eq!(
        server.process_new_packets(),
        Err(Error::AlertReceived(AlertDescription::UnknownCA)),
    );
}

#[test]
fn test_tls13_too_short_early_plaintext_alert() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    // Perform the start of a TLS 1.3 handshake, sending a client hello to the server.
    // The client will not have written a CCS or any encrypted messages to the server yet.
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    // Inject a plaintext alert from the client. The server should attempt to decrypt this message
    // because the payload length is too large to be considered an early plaintext alert.
    server
        .read_tls(&mut io::Cursor::new(&build_alert(
            AlertLevel::Fatal,
            AlertDescription::UnknownCA,
            &[0xff],
        )))
        .unwrap();

    // The server should produce a decrypt error trying to decrypt the plaintext alert.
    assert_eq!(server.process_new_packets(), Err(Error::DecryptError),);
}

#[test]
fn test_tls13_late_plaintext_alert() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    // Complete a bi-directional TLS1.3 handshake. After this point no plaintext messages
    // should occur.
    do_handshake(&mut client, &mut server);

    // Inject a plaintext alert from the client. The server should attempt to decrypt this message.
    server
        .read_tls(&mut io::Cursor::new(&build_alert(
            AlertLevel::Fatal,
            AlertDescription::UnknownCA,
            &[],
        )))
        .unwrap();

    // The server should produce a decrypt error, trying to decrypt a plaintext alert.
    assert_eq!(server.process_new_packets(), Err(Error::DecryptError));
}

fn build_alert(level: AlertLevel, desc: AlertDescription, suffix: &[u8]) -> Vec<u8> {
    let mut v = vec![ContentType::Alert.into()];
    ProtocolVersion::TLSv1_2.encode(&mut v);
    ((2 + suffix.len()) as u16).encode(&mut v);
    level.encode(&mut v);
    desc.encode(&mut v);
    v.extend_from_slice(suffix);
    v
}

#[derive(Default, Debug)]
struct ServerCheckCertResolve {
    expected_sni: Option<DnsName<'static>>,
    expected_sigalgs: Option<Vec<SignatureScheme>>,
    expected_alpn: Option<Vec<Vec<u8>>>,
    expected_cipher_suites: Option<Vec<CipherSuite>>,
    expected_server_cert_types: Option<Vec<CertificateType>>,
    expected_client_cert_types: Option<Vec<CertificateType>>,
    expected_named_groups: Option<Vec<NamedGroup>>,
}

impl ResolvesServerCert for ServerCheckCertResolve {
    fn resolve(&self, client_hello: &ClientHello) -> Option<Arc<sign::CertifiedKey>> {
        if client_hello
            .signature_schemes()
            .is_empty()
        {
            panic!("no signature schemes shared by client");
        }

        if client_hello.cipher_suites().is_empty() {
            panic!("no cipher suites shared by client");
        }

        if let Some(expected_sni) = &self.expected_sni {
            let sni = client_hello
                .server_name()
                .expect("sni unexpectedly absent");
            assert_eq!(expected_sni, sni);
        }

        if let Some(expected_sigalgs) = &self.expected_sigalgs {
            assert_eq!(
                expected_sigalgs,
                client_hello.signature_schemes(),
                "unexpected signature schemes"
            );
        }

        if let Some(expected_alpn) = &self.expected_alpn {
            let alpn = client_hello
                .alpn()
                .expect("alpn unexpectedly absent")
                .collect::<Vec<_>>();
            assert_eq!(alpn.len(), expected_alpn.len());

            for (got, wanted) in alpn.iter().zip(expected_alpn.iter()) {
                assert_eq!(got, &wanted.as_slice());
            }
        }

        if let Some(expected_cipher_suites) = &self.expected_cipher_suites {
            assert_eq!(
                expected_cipher_suites,
                client_hello.cipher_suites(),
                "unexpected cipher suites"
            );
        }

        if let Some(expected_server_cert) = &self.expected_server_cert_types {
            assert_eq!(
                expected_server_cert,
                client_hello
                    .server_cert_types()
                    .expect("Server cert types not present"),
                "unexpected server cert"
            );
        }

        if let Some(expected_client_cert) = &self.expected_client_cert_types {
            assert_eq!(
                expected_client_cert,
                client_hello
                    .client_cert_types()
                    .expect("Client cert types not present"),
                "unexpected client cert"
            );
        }

        if let Some(expected_named_groups) = &self.expected_named_groups {
            assert_eq!(
                expected_named_groups,
                client_hello
                    .named_groups()
                    .expect("Named groups not present"),
            )
        }

        None
    }
}

#[test]
fn server_cert_resolve_with_sni() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let client_config = make_client_config(*kt, &provider);
        let mut server_config = make_server_config(*kt, &provider);

        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_sni: Some(DnsName::try_from("the.value.from.sni").unwrap()),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), server_name("the.value.from.sni"))
                .unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server);
        assert!(err.is_err());
    }
}

#[test]
fn server_cert_resolve_with_alpn() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let mut client_config = make_client_config(*kt, &provider);
        client_config.alpn_protocols = vec!["foo".into(), "bar".into()];

        let mut server_config = make_server_config(*kt, &provider);
        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_alpn: Some(vec![b"foo".to_vec(), b"bar".to_vec()]),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), server_name("sni-value")).unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server);
        assert!(err.is_err());
    }
}

#[test]
fn server_cert_resolve_with_named_groups() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let client_config = make_client_config(*kt, &provider);

        let mut server_config = make_server_config(*kt, &provider);
        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_named_groups: Some(
                provider
                    .kx_groups
                    .iter()
                    .map(|kx| kx.name())
                    .collect(),
            ),
            ..Default::default()
        });

        let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
        let err = do_handshake_until_error(&mut client, &mut server);
        assert!(err.is_err());
    }
}

#[test]
fn client_trims_terminating_dot() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let client_config = make_client_config(*kt, &provider);
        let mut server_config = make_server_config(*kt, &provider);

        server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
            expected_sni: Some(DnsName::try_from("some-host.com").unwrap()),
            ..Default::default()
        });

        let mut client =
            ClientConnection::new(Arc::new(client_config), server_name("some-host.com.")).unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        let err = do_handshake_until_error(&mut client, &mut server);
        assert!(err.is_err());
    }
}

fn check_sigalgs_reduced_by_ciphersuite(
    kt: KeyType,
    suite: CipherSuite,
    expected_sigalgs: Vec<SignatureScheme>,
) {
    let client_config = finish_client_config(
        kt,
        ClientConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![find_suite(suite)],
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    let mut server_config = make_server_config(kt, &provider::default_provider());

    server_config.cert_resolver = Arc::new(ServerCheckCertResolve {
        expected_sigalgs: Some(expected_sigalgs),
        expected_cipher_suites: Some(vec![suite, CipherSuite::TLS_EMPTY_RENEGOTIATION_INFO_SCSV]),
        ..Default::default()
    });

    let mut client =
        ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
    let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

    let err = do_handshake_until_error(&mut client, &mut server);
    assert!(err.is_err());
}

#[test]
fn server_cert_resolve_reduces_sigalgs_for_rsa_ciphersuite() {
    check_sigalgs_reduced_by_ciphersuite(
        KeyType::Rsa2048,
        CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        vec![
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA256,
        ],
    );
}

#[test]
fn server_cert_resolve_reduces_sigalgs_for_ecdsa_ciphersuite() {
    check_sigalgs_reduced_by_ciphersuite(
        KeyType::EcdsaP256,
        CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        if provider_is_aws_lc_rs() {
            vec![
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::ED25519,
            ]
        } else {
            vec![
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ED25519,
            ]
        },
    );
}

#[derive(Debug)]
struct ServerCheckNoSni {}

impl ResolvesServerCert for ServerCheckNoSni {
    fn resolve(&self, client_hello: &ClientHello) -> Option<Arc<sign::CertifiedKey>> {
        assert!(client_hello.server_name().is_none());

        None
    }
}

#[test]
fn client_with_sni_disabled_does_not_send_sni() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let mut server_config = make_server_config(*kt, &provider);
        server_config.cert_resolver = Arc::new(ServerCheckNoSni {});
        let server_config = Arc::new(server_config);

        for version in rustls::ALL_VERSIONS {
            let mut client_config = make_client_config_with_versions(*kt, &[version], &provider);
            client_config.enable_sni = false;

            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("value-not-sent"))
                    .unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            let err = do_handshake_until_error(&mut client, &mut server);
            assert!(err.is_err());
        }
    }
}

#[test]
fn client_checks_server_certificate_with_given_name() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version], &provider);
            let mut client = ClientConnection::new(
                Arc::new(client_config),
                server_name("not-the-right-hostname.com"),
            )
            .unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    certificate_error_expecting_name("not-the-right-hostname.com")
                )))
            );
        }
    }
}

#[test]
fn client_checks_server_certificate_with_given_ip_address() {
    fn check_server_name(
        client_config: Arc<ClientConfig>,
        server_config: Arc<ServerConfig>,
        name: &'static str,
    ) -> Result<(), ErrorFromPeer> {
        let mut client = ClientConnection::new(client_config, server_name(name)).unwrap();
        let mut server = ServerConnection::new(server_config).unwrap();
        do_handshake_until_error(&mut client, &mut server)
    }

    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        for version in rustls::ALL_VERSIONS {
            let client_config =
                Arc::new(make_client_config_with_versions(*kt, &[version], &provider));

            // positive ipv4 case
            assert_eq!(
                check_server_name(client_config.clone(), server_config.clone(), "198.51.100.1"),
                Ok(()),
            );

            // negative ipv4 case
            assert_eq!(
                check_server_name(client_config.clone(), server_config.clone(), "198.51.100.2"),
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    certificate_error_expecting_name("198.51.100.2")
                )))
            );

            // positive ipv6 case
            assert_eq!(
                check_server_name(client_config.clone(), server_config.clone(), "2001:db8::1"),
                Ok(()),
            );

            // negative ipv6 case
            assert_eq!(
                check_server_name(client_config.clone(), server_config.clone(), "2001:db8::2"),
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    certificate_error_expecting_name("2001:db8::2")
                )))
            );
        }
    }
}

#[test]
fn client_check_server_certificate_ee_revoked() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        // Setup a server verifier that will check the EE certificate's revocation status.
        let crls = vec![kt.end_entity_crl()];
        let builder = webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
            .with_crls(crls)
            .only_check_end_entity_revocation();

        for version in rustls::ALL_VERSIONS {
            let client_config =
                make_client_config_with_verifier(&[version], builder.clone(), &provider);
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            // We expect the handshake to fail since the server's EE certificate is revoked.
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    CertificateError::Revoked
                )))
            );
        }
    }
}

#[test]
fn client_check_server_certificate_ee_unknown_revocation() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        // Setup a server verifier builder that will check the EE certificate's revocation status, but not
        // allow unknown revocation status (the default). We'll provide CRLs that are not relevant
        // to the EE cert to ensure its status is unknown.
        let unrelated_crls = vec![kt.intermediate_crl()];
        let forbid_unknown_verifier =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(unrelated_crls.clone())
                .only_check_end_entity_revocation();

        // Also set up a verifier builder that will allow unknown revocation status.
        let allow_unknown_verifier =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(unrelated_crls)
                .only_check_end_entity_revocation()
                .allow_unknown_revocation_status();

        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_verifier(
                &[version],
                forbid_unknown_verifier.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            // We expect if we use the forbid_unknown_verifier that the handshake will fail since the
            // server's EE certificate's revocation status is unknown given the CRLs we've provided.
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    CertificateError::UnknownRevocationStatus
                )))
            );

            // We expect if we use the allow_unknown_verifier that the handshake will not fail.
            let client_config = make_client_config_with_verifier(
                &[version],
                allow_unknown_verifier.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();
            let res = do_handshake_until_error(&mut client, &mut server);
            assert!(res.is_ok());
        }
    }
}

#[test]
fn client_check_server_certificate_intermediate_revoked() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        // Setup a server verifier builder that will check the full chain revocation status against a CRL
        // that marks the intermediate certificate as revoked. We allow unknown revocation status
        // so the EE cert's unknown status doesn't cause an error.
        let crls = vec![kt.intermediate_crl()];
        let full_chain_verifier_builder =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls.clone())
                .allow_unknown_revocation_status();

        // Also set up a verifier builder that will use the same CRL, but only check the EE certificate
        // revocation status.
        let ee_verifier_builder =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls.clone())
                .only_check_end_entity_revocation()
                .allow_unknown_revocation_status();

        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_verifier(
                &[version],
                full_chain_verifier_builder.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            // We expect the handshake to fail when using the full chain verifier since the intermediate's
            // EE certificate is revoked.
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    CertificateError::Revoked
                )))
            );

            let client_config = make_client_config_with_verifier(
                &[version],
                ee_verifier_builder.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();
            // We expect the handshake to succeed when we use the verifier that only checks the EE certificate
            // revocation status. The revoked intermediate status should not be checked.
            let res = do_handshake_until_error(&mut client, &mut server);
            assert!(res.is_ok())
        }
    }
}

#[test]
fn client_check_server_certificate_ee_crl_expired() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config(*kt, &provider));

        // Setup a server verifier that will check the EE certificate's revocation status, with CRL expiration enforced.
        let crls = vec![kt.end_entity_crl_expired()];
        let enforce_expiration_builder =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls)
                .only_check_end_entity_revocation()
                .enforce_revocation_expiration();

        // Also setup a server verifier without CRL expiration enforced.
        let crls = vec![kt.end_entity_crl_expired()];
        let ignore_expiration_builder =
            webpki_server_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls)
                .only_check_end_entity_revocation();

        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_verifier(
                &[version],
                enforce_expiration_builder.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            // We expect the handshake to fail since the CRL is expired.
            let err = do_handshake_until_error(&mut client, &mut server);
            assert!(matches!(
                err,
                Err(ErrorFromPeer::Client(Error::InvalidCertificate(
                    CertificateError::ExpiredRevocationListContext { .. }
                )))
            ));

            let client_config = make_client_config_with_verifier(
                &[version],
                ignore_expiration_builder.clone(),
                &provider,
            );
            let mut client =
                ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();

            // We expect the handshake to succeed when CRL expiration is ignored.
            let res = do_handshake_until_error(&mut client, &mut server);
            assert!(res.is_ok())
        }
    }
}

/// Simple smoke-test of the webpki verify_server_cert_signed_by_trust_anchor helper API.
/// This public API is intended to be used by consumers implementing their own verifier and
/// so isn't used by the other existing verifier tests.
#[test]
fn client_check_server_certificate_helper_api() {
    for kt in KeyType::all_for_provider(&provider::default_provider()) {
        let chain = kt.get_chain();
        let correct_roots = get_client_root_store(*kt);
        let incorrect_roots = get_client_root_store(match kt {
            KeyType::Rsa2048 => KeyType::EcdsaP256,
            _ => KeyType::Rsa2048,
        });
        // Using the correct trust anchors, we should verify without error.
        assert!(
            verify_server_cert_signed_by_trust_anchor(
                &ParsedCertificate::try_from(chain.first().unwrap()).unwrap(),
                &correct_roots,
                &[chain.get(1).unwrap().clone()],
                UnixTime::now(),
                webpki::ALL_VERIFICATION_ALGS,
            )
            .is_ok()
        );
        // Using the wrong trust anchors, we should get the expected error.
        assert_eq!(
            verify_server_cert_signed_by_trust_anchor(
                &ParsedCertificate::try_from(chain.first().unwrap()).unwrap(),
                &incorrect_roots,
                &[chain.get(1).unwrap().clone()],
                UnixTime::now(),
                webpki::ALL_VERIFICATION_ALGS,
            )
            .unwrap_err(),
            Error::InvalidCertificate(CertificateError::UnknownIssuer)
        );
    }
}

#[test]
fn client_check_server_valid_purpose() {
    let chain = KeyType::EcdsaP256.get_client_chain();
    let trust_anchor = chain.last().unwrap();
    let roots = RootCertStore {
        roots: vec![
            anchor_from_trusted_cert(trust_anchor)
                .unwrap()
                .to_owned(),
        ],
    };

    let error = verify_server_cert_signed_by_trust_anchor(
        &ParsedCertificate::try_from(chain.first().unwrap()).unwrap(),
        &roots,
        &[chain.get(1).unwrap().clone()],
        UnixTime::now(),
        webpki::ALL_VERIFICATION_ALGS,
    )
    .unwrap_err();
    assert_eq!(
        error,
        Error::InvalidCertificate(CertificateError::InvalidPurposeContext {
            required: ExtendedKeyPurpose::ServerAuth,
            presented: vec![ExtendedKeyPurpose::ClientAuth],
        })
    );

    assert_eq!(
        format!("{error}"),
        "invalid peer certificate: certificate does not allow extended key usage for \
         server authentication, allows client authentication"
    );
}

#[derive(Debug)]
struct ClientCheckCertResolve {
    query_count: AtomicUsize,
    expect_queries: usize,
    expect_root_hint_subjects: Vec<Vec<u8>>,
    expect_sigschemes: Vec<SignatureScheme>,
}

impl ClientCheckCertResolve {
    fn new(
        expect_queries: usize,
        expect_root_hint_subjects: Vec<Vec<u8>>,
        expect_sigschemes: Vec<SignatureScheme>,
    ) -> Self {
        Self {
            query_count: AtomicUsize::new(0),
            expect_queries,
            expect_root_hint_subjects,
            expect_sigschemes,
        }
    }
}

impl Drop for ClientCheckCertResolve {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let count = self.query_count.load(Ordering::SeqCst);
            assert_eq!(count, self.expect_queries);
        }
    }
}

impl ResolvesClientCert for ClientCheckCertResolve {
    fn resolve(
        &self,
        root_hint_subjects: &[&[u8]],
        sigschemes: &[SignatureScheme],
    ) -> Option<Arc<sign::CertifiedKey>> {
        self.query_count
            .fetch_add(1, Ordering::SeqCst);

        if sigschemes.is_empty() {
            panic!("no signature schemes shared by server");
        }

        assert_eq!(sigschemes, self.expect_sigschemes);
        assert_eq!(root_hint_subjects, self.expect_root_hint_subjects);

        None
    }

    fn has_certs(&self) -> bool {
        true
    }
}

fn test_client_cert_resolve(
    key_type: KeyType,
    server_config: Arc<ServerConfig>,
    expected_root_hint_subjects: Vec<Vec<u8>>,
) {
    let provider = provider::default_provider();
    for version in rustls::ALL_VERSIONS {
        println!("{:?} {:?}:", version.version(), key_type);

        let mut client_config = make_client_config_with_versions(key_type, &[version], &provider);
        client_config.client_auth_cert_resolver = Arc::new(ClientCheckCertResolve::new(
            1,
            expected_root_hint_subjects.clone(),
            default_signature_schemes(version.version()),
        ));

        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        assert_eq!(
            do_handshake_until_error(&mut client, &mut server),
            Err(ErrorFromPeer::Server(Error::NoCertificatesPresented))
        );
    }
}

fn default_signature_schemes(version: ProtocolVersion) -> Vec<SignatureScheme> {
    let mut v = vec![];

    v.extend_from_slice(&[
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ED25519,
        SignatureScheme::RSA_PSS_SHA512,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA256,
    ]);

    if provider_is_aws_lc_rs() {
        v.insert(2, SignatureScheme::ECDSA_NISTP521_SHA512);
    }

    if version == ProtocolVersion::TLSv1_2 {
        v.extend_from_slice(&[
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]);
    }

    v
}

#[test]
fn client_cert_resolve_default() {
    // Test that in the default configuration that a client cert resolver gets the expected
    // CA subject hints, and supported signature algorithms.
    let provider = provider::default_provider();
    for key_type in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(
            *key_type, &provider,
        ));

        // In a default configuration we expect that the verifier's trust anchors are used
        // for the hint subjects.
        let expected_root_hint_subjects = vec![
            key_type
                .ca_distinguished_name()
                .to_vec(),
        ];

        test_client_cert_resolve(*key_type, server_config, expected_root_hint_subjects);
    }
}

#[test]
fn client_cert_resolve_server_no_hints() {
    // Test that a server can provide no hints and the client cert resolver gets the expected
    // arguments.
    let provider = provider::default_provider();
    for key_type in KeyType::all_for_provider(&provider) {
        // Build a verifier with no hint subjects.
        let verifier = webpki_client_verifier_builder(get_client_root_store(*key_type), &provider)
            .clear_root_hint_subjects();
        let server_config = make_server_config_with_client_verifier(*key_type, verifier, &provider);
        let expected_root_hint_subjects = Vec::default(); // no hints expected.
        test_client_cert_resolve(*key_type, server_config.into(), expected_root_hint_subjects);
    }
}

#[test]
fn client_cert_resolve_server_added_hint() {
    // Test that a server can add an extra subject above/beyond those found in its trust store
    // and the client cert resolver gets the expected arguments.
    let provider = provider::default_provider();
    let extra_name = b"0\x1a1\x180\x16\x06\x03U\x04\x03\x0c\x0fponyland IDK CA".to_vec();
    for key_type in KeyType::all_for_provider(&provider) {
        let expected_hint_subjects = vec![
            key_type
                .ca_distinguished_name()
                .to_vec(),
            extra_name.clone(),
        ];
        // Create a verifier that adds the extra_name as a hint subject in addition to the ones
        // from the root cert store.
        let verifier = webpki_client_verifier_builder(get_client_root_store(*key_type), &provider)
            .add_root_hint_subjects([DistinguishedName::from(extra_name.clone())].into_iter());
        let server_config = make_server_config_with_client_verifier(*key_type, verifier, &provider);
        test_client_cert_resolve(*key_type, server_config.into(), expected_hint_subjects);
    }
}

#[test]
fn client_auth_works() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(
            *kt, &provider,
        ));

        for version in rustls::ALL_VERSIONS {
            let client_config =
                make_client_config_with_versions_with_auth(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
            do_handshake(&mut client, &mut server);
        }
    }
}

#[test]
fn client_mandatory_auth_client_revocation_works() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        // Create a server configuration that includes a CRL that specifies the client certificate
        // is revoked.
        let relevant_crls = vec![kt.client_crl()];
        // Only check the EE certificate status. See client_mandatory_auth_intermediate_revocation_works
        // for testing revocation status of the whole chain.
        let ee_verifier_builder =
            webpki_client_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(relevant_crls)
                .only_check_end_entity_revocation();
        let revoked_server_config = Arc::new(make_server_config_with_client_verifier(
            *kt,
            ee_verifier_builder,
            &provider,
        ));

        // Create a server configuration that includes a CRL that doesn't cover the client certificate,
        // and uses the default behaviour of treating unknown revocation status as an error.
        let unrelated_crls = vec![kt.intermediate_crl()];
        let ee_verifier_builder =
            webpki_client_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(unrelated_crls.clone())
                .only_check_end_entity_revocation();
        let missing_client_crl_server_config = Arc::new(make_server_config_with_client_verifier(
            *kt,
            ee_verifier_builder,
            &provider,
        ));

        // Create a server configuration that includes a CRL that doesn't cover the client certificate,
        // but change the builder to allow unknown revocation status.
        let ee_verifier_builder =
            webpki_client_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(unrelated_crls.clone())
                .only_check_end_entity_revocation()
                .allow_unknown_revocation_status();
        let allow_missing_client_crl_server_config = Arc::new(
            make_server_config_with_client_verifier(*kt, ee_verifier_builder, &provider),
        );

        for version in rustls::ALL_VERSIONS {
            // Connecting to the server with a CRL that indicates the client certificate is revoked
            // should fail with the expected error.
            let client_config = Arc::new(make_client_config_with_versions_with_auth(
                *kt,
                &[version],
                &provider,
            ));
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &revoked_server_config);
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Server(Error::InvalidCertificate(
                    CertificateError::Revoked
                )))
            );
            // Connecting to the server missing CRL information for the client certificate should
            // fail with the expected unknown revocation status error.
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &missing_client_crl_server_config);
            let res = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                res,
                Err(ErrorFromPeer::Server(Error::InvalidCertificate(
                    CertificateError::UnknownRevocationStatus
                )))
            );
            // Connecting to the server missing CRL information for the client should not error
            // if the server's verifier allows unknown revocation status.
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &allow_missing_client_crl_server_config);
            let res = do_handshake_until_error(&mut client, &mut server);
            assert!(res.is_ok());
        }
    }
}

#[test]
fn client_mandatory_auth_intermediate_revocation_works() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        // Create a server configuration that includes a CRL that specifies the intermediate certificate
        // is revoked. We check the full chain for revocation status (default), and allow unknown
        // revocation status so the EE's unknown revocation status isn't an error.
        let crls = vec![kt.intermediate_crl()];
        let full_chain_verifier_builder =
            webpki_client_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls.clone())
                .allow_unknown_revocation_status();
        let full_chain_server_config = Arc::new(make_server_config_with_client_verifier(
            *kt,
            full_chain_verifier_builder,
            &provider,
        ));

        // Also create a server configuration that uses the same CRL, but that only checks the EE
        // cert revocation status.
        let ee_only_verifier_builder =
            webpki_client_verifier_builder(get_client_root_store(*kt), &provider)
                .with_crls(crls)
                .only_check_end_entity_revocation()
                .allow_unknown_revocation_status();
        let ee_server_config = Arc::new(make_server_config_with_client_verifier(
            *kt,
            ee_only_verifier_builder,
            &provider,
        ));

        for version in rustls::ALL_VERSIONS {
            // When checking the full chain, we expect an error - the intermediate is revoked.
            let client_config = Arc::new(make_client_config_with_versions_with_auth(
                *kt,
                &[version],
                &provider,
            ));
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &full_chain_server_config);
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Server(Error::InvalidCertificate(
                    CertificateError::Revoked
                )))
            );
            // However, when checking just the EE cert we expect no error - the intermediate's
            // revocation status should not be checked.
            let (mut client, mut server) =
                make_pair_for_arc_configs(&client_config, &ee_server_config);
            assert!(do_handshake_until_error(&mut client, &mut server).is_ok());
        }
    }
}

#[test]
fn client_optional_auth_client_revocation_works() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        // Create a server configuration that includes a CRL that specifies the client certificate
        // is revoked.
        let crls = vec![kt.client_crl()];
        let server_config = Arc::new(make_server_config_with_optional_client_auth(
            *kt, crls, &provider,
        ));

        for version in rustls::ALL_VERSIONS {
            let client_config =
                make_client_config_with_versions_with_auth(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_arc_configs(&Arc::new(client_config), &server_config);
            // Because the client certificate is revoked, the handshake should fail.
            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(
                err,
                Err(ErrorFromPeer::Server(Error::InvalidCertificate(
                    CertificateError::Revoked
                )))
            );
        }
    }
}

#[test]
fn client_error_is_sticky() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    client
        .read_tls(&mut b"\x16\x03\x03\x00\x08\x0f\x00\x00\x04junk".as_ref())
        .unwrap();
    let mut err = client.process_new_packets();
    assert!(err.is_err());
    err = client.process_new_packets();
    assert!(err.is_err());
}

#[test]
fn server_error_is_sticky() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    server
        .read_tls(&mut b"\x16\x03\x03\x00\x08\x0f\x00\x00\x04junk".as_ref())
        .unwrap();
    let mut err = server.process_new_packets();
    assert!(err.is_err());
    err = server.process_new_packets();
    assert!(err.is_err());
}

#[test]
fn server_flush_does_nothing() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(server.writer().flush(), Ok(())));
}

#[test]
fn client_flush_does_nothing() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(client.writer().flush(), Ok(())));
}

#[allow(clippy::no_effect)]
#[test]
fn server_is_send_and_sync() {
    let (_, server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    &server as &dyn Send;
    &server as &dyn Sync;
}

#[allow(clippy::no_effect)]
#[test]
fn client_is_send_and_sync() {
    let (client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    &client as &dyn Send;
    &client as &dyn Sync;
}

#[test]
fn server_respects_buffer_limit_pre_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    server.set_buffer_limit(Some(32));

    assert_eq!(
        server
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        20
    );
    assert_eq!(
        server
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        12
    );

    do_handshake(&mut client, &mut server);
    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    check_read(&mut client.reader(), b"01234567890123456789012345678901");
}

#[test]
fn server_respects_buffer_limit_pre_handshake_with_vectored_write() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    server.set_buffer_limit(Some(32));

    assert_eq!(
        server
            .writer()
            .write_vectored(&[
                IoSlice::new(b"01234567890123456789"),
                IoSlice::new(b"01234567890123456789")
            ])
            .unwrap(),
        32
    );

    do_handshake(&mut client, &mut server);
    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    check_read(&mut client.reader(), b"01234567890123456789012345678901");
}

#[test]
fn server_respects_buffer_limit_post_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    // this test will vary in behaviour depending on the default suites
    do_handshake(&mut client, &mut server);
    server.set_buffer_limit(Some(48));

    assert_eq!(
        server
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        20
    );
    assert_eq!(
        server
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        6
    );

    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    check_read(&mut client.reader(), b"01234567890123456789012345");
}

#[test]
fn client_respects_buffer_limit_pre_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    client.set_buffer_limit(Some(32));

    assert_eq!(
        client
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        20
    );
    assert_eq!(
        client
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        12
    );

    do_handshake(&mut client, &mut server);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server.reader(), b"01234567890123456789012345678901");
}

#[test]
fn client_respects_buffer_limit_pre_handshake_with_vectored_write() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    client.set_buffer_limit(Some(32));

    assert_eq!(
        client
            .writer()
            .write_vectored(&[
                IoSlice::new(b"01234567890123456789"),
                IoSlice::new(b"01234567890123456789")
            ])
            .unwrap(),
        32
    );

    do_handshake(&mut client, &mut server);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server.reader(), b"01234567890123456789012345678901");
}

#[test]
fn client_respects_buffer_limit_post_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    do_handshake(&mut client, &mut server);
    client.set_buffer_limit(Some(48));

    assert_eq!(
        client
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        20
    );
    assert_eq!(
        client
            .writer()
            .write(b"01234567890123456789")
            .unwrap(),
        6
    );

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server.reader(), b"01234567890123456789012345");
}

#[test]
fn client_detects_broken_write_vectored_impl() {
    // see https://github.com/rustls/rustls/issues/2316
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let err = client
        .write_tls(&mut BrokenWriteVectored)
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(format!("{err:?}").starts_with(
        "Custom { kind: Other, error: \"illegal write_vectored return value (9999 > "
    ));

    struct BrokenWriteVectored;

    impl io::Write for BrokenWriteVectored {
        fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> io::Result<usize> {
            Ok(9999)
        }

        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            unreachable!()
        }

        fn flush(&mut self) -> io::Result<()> {
            unreachable!()
        }
    }
}

#[test]
fn buf_read() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    do_handshake(&mut client, &mut server);

    // Write two separate messages
    assert_eq!(client.writer().write(b"hello").unwrap(), 5);
    transfer(&mut client, &mut server);
    assert_eq!(client.writer().write(b"world").unwrap(), 5);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let mut reader = server.reader();
    // fill_buf() returns each record separately (this is an implementation detail)
    assert_eq!(reader.fill_buf().unwrap(), b"hello");
    // partially consuming the buffer is OK
    reader.consume(1);
    assert_eq!(reader.fill_buf().unwrap(), b"ello");
    // Read::read is compatible with BufRead
    let mut b = [0u8; 2];
    reader.read_exact(&mut b).unwrap();
    assert_eq!(b, *b"el");
    assert_eq!(reader.fill_buf().unwrap(), b"lo");
    reader.consume(2);
    // once the first packet is consumed, the next one is available
    assert_eq!(reader.fill_buf().unwrap(), b"world");
    reader.consume(5);
    check_fill_buf_err(&mut reader, io::ErrorKind::WouldBlock);
}

struct OtherSession<'a, C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>>,
    S: SideData,
{
    sess: &'a mut C,
    pub reads: usize,
    pub writevs: Vec<Vec<usize>>,
    fail_ok: bool,
    pub short_writes: bool,
    pub last_error: Option<rustls::Error>,
    pub buffered: bool,
    buffer: Vec<Vec<u8>>,
}

impl<'a, C, S> OtherSession<'a, C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>>,
    S: SideData,
{
    fn new(sess: &'a mut C) -> OtherSession<'a, C, S> {
        OtherSession {
            sess,
            reads: 0,
            writevs: vec![],
            fail_ok: false,
            short_writes: false,
            last_error: None,
            buffered: false,
            buffer: vec![],
        }
    }

    fn new_buffered(sess: &'a mut C) -> OtherSession<'a, C, S> {
        let mut os = OtherSession::new(sess);
        os.buffered = true;
        os
    }

    fn new_fails(sess: &'a mut C) -> OtherSession<'a, C, S> {
        let mut os = OtherSession::new(sess);
        os.fail_ok = true;
        os
    }

    fn flush_vectored(&mut self, b: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        let mut lengths = vec![];
        for bytes in b {
            let write_len = if self.short_writes {
                if bytes.len() > 5 {
                    bytes.len() / 2
                } else {
                    bytes.len()
                }
            } else {
                bytes.len()
            };

            let l = self
                .sess
                .read_tls(&mut io::Cursor::new(&bytes[..write_len]))?;
            lengths.push(l);
            total += l;
            if bytes.len() != l {
                break;
            }
        }

        let rc = self.sess.process_new_packets();
        if !self.fail_ok {
            rc.unwrap();
        } else if rc.is_err() {
            self.last_error = rc.err();
        }

        self.writevs.push(lengths);
        Ok(total)
    }
}

impl<C, S> io::Read for OtherSession<'_, C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>>,
    S: SideData,
{
    fn read(&mut self, mut b: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.sess.write_tls(b.by_ref())
    }
}

impl<C, S> io::Write for OtherSession<'_, C, S>
where
    C: DerefMut + Deref<Target = ConnectionCommon<S>>,
    S: SideData,
{
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        unreachable!()
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let buffer = mem::take(&mut self.buffer);
            let slices = buffer
                .iter()
                .map(|b| io::IoSlice::new(b))
                .collect::<Vec<_>>();
            self.flush_vectored(&slices)?;
        }
        Ok(())
    }

    fn write_vectored(&mut self, b: &[io::IoSlice<'_>]) -> io::Result<usize> {
        if self.buffered {
            self.buffer
                .extend(b.iter().map(|s| s.to_vec()));
            return Ok(b.iter().map(|s| s.len()).sum());
        }
        self.flush_vectored(b)
    }
}

#[test]
fn server_read_returns_wouldblock_when_no_data() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(server.reader().read(&mut [0u8; 1]),
                     Err(err) if err.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn client_read_returns_wouldblock_when_no_data() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(client.reader().read(&mut [0u8; 1]),
                     Err(err) if err.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn server_fill_buf_returns_wouldblock_when_no_data() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(server.reader().fill_buf(),
                     Err(err) if err.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn client_fill_buf_returns_wouldblock_when_no_data() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(matches!(client.reader().fill_buf(),
                     Err(err) if err.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn new_server_returns_initial_io_state() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let io_state = server.process_new_packets().unwrap();
    println!("IoState is Debug {io_state:?}");
    assert_eq!(io_state.plaintext_bytes_to_read(), 0);
    assert!(!io_state.peer_has_closed());
    assert_eq!(io_state.tls_bytes_to_write(), 0);
}

#[test]
fn new_client_returns_initial_io_state() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let io_state = client.process_new_packets().unwrap();
    println!("IoState is Debug {io_state:?}");
    assert_eq!(io_state.plaintext_bytes_to_read(), 0);
    assert!(!io_state.peer_has_closed());
    assert!(io_state.tls_bytes_to_write() > 200);
}

#[test]
fn client_complete_io_for_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    assert!(client.is_handshaking());
    let (rdlen, wrlen) = client
        .complete_io(&mut OtherSession::new(&mut server))
        .unwrap();
    assert!(rdlen > 0 && wrlen > 0);
    assert!(!client.is_handshaking());
    assert!(!client.wants_write());
}

#[test]
fn buffered_client_complete_io_for_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    assert!(client.is_handshaking());
    let (rdlen, wrlen) = client
        .complete_io(&mut OtherSession::new_buffered(&mut server))
        .unwrap();
    assert!(rdlen > 0 && wrlen > 0);
    assert!(!client.is_handshaking());
    assert!(!client.wants_write());
}

#[test]
fn client_complete_io_for_handshake_eof() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let mut input = io::Cursor::new(Vec::new());

    assert!(client.is_handshaking());
    let err = client
        .complete_io(&mut input)
        .unwrap_err();
    assert_eq!(io::ErrorKind::UnexpectedEof, err.kind());
}

#[test]
fn client_complete_io_for_write() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        client
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        client
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            let mut pipe = OtherSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            println!("{:?}", pipe.writevs);
            assert_eq!(pipe.writevs, vec![vec![42, 42]]);
        }
        check_read(
            &mut server.reader(),
            b"0123456789012345678901234567890123456789",
        );
    }
}

#[test]
fn buffered_client_complete_io_for_write() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        client
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        client
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            let mut pipe = OtherSession::new_buffered(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            println!("{:?}", pipe.writevs);
            assert_eq!(pipe.writevs, vec![vec![42, 42]]);
        }
        check_read(
            &mut server.reader(),
            b"0123456789012345678901234567890123456789",
        );
    }
}

#[test]
fn client_complete_io_for_read() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        server
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            let mut pipe = OtherSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).unwrap();
            assert!(rdlen > 0 && wrlen == 0);
            assert_eq!(pipe.reads, 1);
        }
        check_read(&mut client.reader(), b"01234567890123456789");
    }
}

#[test]
fn server_complete_io_for_handshake() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        assert!(server.is_handshaking());
        let (rdlen, wrlen) = server
            .complete_io(&mut OtherSession::new(&mut client))
            .unwrap();
        assert!(rdlen > 0 && wrlen > 0);
        assert!(!server.is_handshaking());
        assert!(!server.wants_write());
    }
}

#[test]
fn server_complete_io_for_handshake_eof() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let mut input = io::Cursor::new(Vec::new());

    assert!(server.is_handshaking());
    let err = server
        .complete_io(&mut input)
        .unwrap_err();
    assert_eq!(io::ErrorKind::UnexpectedEof, err.kind());
}

#[test]
fn server_complete_io_for_write() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        server
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        server
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            let mut pipe = OtherSession::new(&mut client);
            let (rdlen, wrlen) = server.complete_io(&mut pipe).unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            assert_eq!(pipe.writevs, vec![vec![42, 42]]);
        }
        check_read(
            &mut client.reader(),
            b"0123456789012345678901234567890123456789",
        );
    }
}

#[test]
fn server_complete_io_for_write_eof() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        // Queue 20 bytes to write.
        server
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            const BYTES_BEFORE_EOF: usize = 5;
            let mut eof_writer = EofWriter::<BYTES_BEFORE_EOF>::default();

            // Only BYTES_BEFORE_EOF should be written.
            let (rdlen, wrlen) = server
                .complete_io(&mut eof_writer)
                .unwrap();
            assert_eq!(rdlen, 0);
            assert_eq!(wrlen, BYTES_BEFORE_EOF);

            // Now nothing should be written.
            let (rdlen, wrlen) = server
                .complete_io(&mut eof_writer)
                .unwrap();
            assert_eq!(rdlen, 0);
            assert_eq!(wrlen, 0);
        }
    }
}

#[derive(Default)]
struct EofWriter<const N: usize> {
    written: usize,
}

impl<const N: usize> std::io::Write for EofWriter<N> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let prev = self.written;
        self.written = N.min(self.written + buf.len());
        Ok(self.written - prev)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<const N: usize> std::io::Read for EofWriter<N> {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!() // This is a writer, it should not be read from.
    }
}

#[test]
fn server_complete_io_for_read() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);

        do_handshake(&mut client, &mut server);

        client
            .writer()
            .write_all(b"01234567890123456789")
            .unwrap();
        {
            let mut pipe = OtherSession::new(&mut client);
            let (rdlen, wrlen) = server.complete_io(&mut pipe).unwrap();
            assert!(rdlen > 0 && wrlen == 0);
            assert_eq!(pipe.reads, 1);
        }
        check_read(&mut server.reader(), b"01234567890123456789");
    }
}

#[test]
fn client_stream_write() {
    test_client_stream_write(StreamKind::Ref);
    test_client_stream_write(StreamKind::Owned);
}

#[test]
fn server_stream_write() {
    test_server_stream_write(StreamKind::Ref);
    test_server_stream_write(StreamKind::Owned);
}

#[derive(Debug, Copy, Clone)]
enum StreamKind {
    Owned,
    Ref,
}

fn test_client_stream_write(stream_kind: StreamKind) {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);
        let data = b"hello";
        {
            let mut pipe = OtherSession::new(&mut server);
            let mut stream: Box<dyn Write> = match stream_kind {
                StreamKind::Ref => Box::new(Stream::new(&mut client, &mut pipe)),
                StreamKind::Owned => Box::new(StreamOwned::new(client, pipe)),
            };
            assert_eq!(stream.write(data).unwrap(), 5);
        }
        check_read(&mut server.reader(), data);
    }
}

fn test_server_stream_write(stream_kind: StreamKind) {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);
        let data = b"hello";
        {
            let mut pipe = OtherSession::new(&mut client);
            let mut stream: Box<dyn Write> = match stream_kind {
                StreamKind::Ref => Box::new(Stream::new(&mut server, &mut pipe)),
                StreamKind::Owned => Box::new(StreamOwned::new(server, pipe)),
            };
            assert_eq!(stream.write(data).unwrap(), 5);
        }
        check_read(&mut client.reader(), data);
    }
}

#[test]
fn client_stream_read() {
    test_client_stream_read(StreamKind::Ref, ReadKind::Buf);
    test_client_stream_read(StreamKind::Owned, ReadKind::Buf);
    test_client_stream_read(StreamKind::Ref, ReadKind::BufRead);
    test_client_stream_read(StreamKind::Owned, ReadKind::BufRead);
}

#[test]
fn server_stream_read() {
    test_server_stream_read(StreamKind::Ref, ReadKind::Buf);
    test_server_stream_read(StreamKind::Owned, ReadKind::Buf);
    test_server_stream_read(StreamKind::Ref, ReadKind::BufRead);
    test_server_stream_read(StreamKind::Owned, ReadKind::BufRead);
}

#[derive(Debug, Copy, Clone)]
enum ReadKind {
    Buf,
    BufRead,
}

fn test_stream_read(read_kind: ReadKind, mut stream: impl BufRead, data: &[u8]) {
    match read_kind {
        ReadKind::Buf => {
            check_read(&mut stream, data);
            check_read_err(&mut stream, io::ErrorKind::UnexpectedEof)
        }
        ReadKind::BufRead => {
            check_fill_buf(&mut stream, data);
            check_fill_buf_err(&mut stream, io::ErrorKind::UnexpectedEof)
        }
    }
}

fn test_client_stream_read(stream_kind: StreamKind, read_kind: ReadKind) {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);
        let data = b"world";
        server.writer().write_all(data).unwrap();

        {
            let mut pipe = OtherSession::new(&mut server);
            transfer_eof(&mut client);

            let stream: Box<dyn BufRead> = match stream_kind {
                StreamKind::Ref => Box::new(Stream::new(&mut client, &mut pipe)),
                StreamKind::Owned => Box::new(StreamOwned::new(client, pipe)),
            };

            test_stream_read(read_kind, stream, data)
        }
    }
}

fn test_server_stream_read(stream_kind: StreamKind, read_kind: ReadKind) {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let (mut client, mut server) = make_pair(*kt, &provider);
        let data = b"world";
        client.writer().write_all(data).unwrap();

        {
            let mut pipe = OtherSession::new(&mut client);
            transfer_eof(&mut server);

            let stream: Box<dyn BufRead> = match stream_kind {
                StreamKind::Ref => Box::new(Stream::new(&mut server, &mut pipe)),
                StreamKind::Owned => Box::new(StreamOwned::new(server, pipe)),
            };

            test_stream_read(read_kind, stream, data)
        }
    }
}

#[test]
fn test_client_write_and_vectored_write_equivalence() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    const N: usize = 1000;

    let data_chunked: Vec<IoSlice> = std::iter::repeat(IoSlice::new(b"A"))
        .take(N)
        .collect();
    let bytes_written_chunked = client
        .writer()
        .write_vectored(&data_chunked)
        .unwrap();
    let bytes_sent_chunked = transfer(&mut client, &mut server);
    println!("write_vectored returned {bytes_written_chunked} and sent {bytes_sent_chunked}");

    let data_contiguous = &[b'A'; N];
    let bytes_written_contiguous = client
        .writer()
        .write(data_contiguous)
        .unwrap();
    let bytes_sent_contiguous = transfer(&mut client, &mut server);
    println!("write returned {bytes_written_contiguous} and sent {bytes_sent_contiguous}");

    assert_eq!(bytes_written_chunked, bytes_written_contiguous);
    assert_eq!(bytes_sent_chunked, bytes_sent_contiguous);
}

struct FailsWrites {
    errkind: io::ErrorKind,
    after: usize,
}

impl io::Read for FailsWrites {
    fn read(&mut self, _b: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl io::Write for FailsWrites {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.after > 0 {
            self.after -= 1;
            Ok(b.len())
        } else {
            Err(io::Error::new(self.errkind, "oops"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn stream_write_reports_underlying_io_error_before_plaintext_processed() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    let mut pipe = FailsWrites {
        errkind: io::ErrorKind::ConnectionAborted,
        after: 0,
    };
    client
        .writer()
        .write_all(b"hello")
        .unwrap();
    let mut client_stream = Stream::new(&mut client, &mut pipe);
    let rc = client_stream.write(b"world");
    assert!(rc.is_err());
    let err = rc.err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
}

#[test]
fn stream_write_swallows_underlying_io_error_after_plaintext_processed() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    let mut pipe = FailsWrites {
        errkind: io::ErrorKind::ConnectionAborted,
        after: 1,
    };
    client
        .writer()
        .write_all(b"hello")
        .unwrap();
    let mut client_stream = Stream::new(&mut client, &mut pipe);
    let rc = client_stream.write(b"world");
    assert_eq!(format!("{rc:?}"), "Ok(5)");
}

fn make_disjoint_suite_configs() -> (ClientConfig, ServerConfig) {
    let kt = KeyType::Rsa2048;
    let client_provider = CryptoProvider {
        cipher_suites: vec![cipher_suite::TLS13_CHACHA20_POLY1305_SHA256],
        ..provider::default_provider()
    };
    let server_config = finish_server_config(
        kt,
        ServerConfig::builder_with_provider(client_provider.into())
            .with_safe_default_protocol_versions()
            .unwrap(),
    );

    let server_provider = CryptoProvider {
        cipher_suites: vec![cipher_suite::TLS13_AES_256_GCM_SHA384],
        ..provider::default_provider()
    };
    let client_config = finish_client_config(
        kt,
        ClientConfig::builder_with_provider(server_provider.into())
            .with_safe_default_protocol_versions()
            .unwrap(),
    );

    (client_config, server_config)
}

#[test]
fn client_stream_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    {
        let mut pipe = OtherSession::new_fails(&mut server);
        let mut client_stream = Stream::new(&mut client, &mut pipe);
        let rc = client_stream.write(b"hello");
        assert!(rc.is_err());
        assert_eq!(
            format!("{rc:?}"),
            "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
        );
        let rc = client_stream.write(b"hello");
        assert!(rc.is_err());
        assert_eq!(
            format!("{rc:?}"),
            "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
        );
    }
}

#[test]
fn client_streamowned_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (client, mut server) = make_pair_for_configs(client_config, server_config);

    let pipe = OtherSession::new_fails(&mut server);
    let mut client_stream = StreamOwned::new(client, pipe);
    let rc = client_stream.write(b"hello");
    assert!(rc.is_err());
    assert_eq!(
        format!("{rc:?}"),
        "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
    );
    let rc = client_stream.write(b"hello");
    assert!(rc.is_err());
    assert_eq!(
        format!("{rc:?}"),
        "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })"
    );

    let (_, _) = client_stream.into_parts();
}

#[test]
fn server_stream_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    client
        .writer()
        .write_all(b"world")
        .unwrap();

    {
        let mut pipe = OtherSession::new_fails(&mut client);
        let mut server_stream = Stream::new(&mut server, &mut pipe);
        let mut bytes = [0u8; 5];
        let rc = server_stream.read(&mut bytes);
        assert!(rc.is_err());
        assert_eq!(
            format!("{rc:?}"),
            "Err(Custom { kind: InvalidData, error: PeerIncompatible(NoCipherSuitesInCommon) })"
        );
    }
}

#[test]
fn server_streamowned_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, server) = make_pair_for_configs(client_config, server_config);

    client
        .writer()
        .write_all(b"world")
        .unwrap();

    let pipe = OtherSession::new_fails(&mut client);
    let mut server_stream = StreamOwned::new(server, pipe);
    let mut bytes = [0u8; 5];
    let rc = server_stream.read(&mut bytes);
    assert!(rc.is_err());
    assert_eq!(
        format!("{rc:?}"),
        "Err(Custom { kind: InvalidData, error: PeerIncompatible(NoCipherSuitesInCommon) })"
    );
}

#[test]
fn server_config_is_clone() {
    let _ = make_server_config(KeyType::Rsa2048, &provider::default_provider());
}

#[test]
fn client_config_is_clone() {
    let _ = make_client_config(KeyType::Rsa2048, &provider::default_provider());
}

#[test]
fn client_connection_is_debug() {
    let (client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    println!("{client:?}");
}

#[test]
fn server_connection_is_debug() {
    let (_, server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    println!("{server:?}");
}

#[test]
fn server_complete_io_for_handshake_ending_with_alert() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert!(server.is_handshaking());

    let mut pipe = OtherSession::new_fails(&mut client);
    let rc = server.complete_io(&mut pipe);
    assert!(rc.is_err(), "server io failed due to handshake failure");
    assert!(!server.wants_write(), "but server did send its alert");
    assert_eq!(
        format!("{:?}", pipe.last_error),
        "Some(AlertReceived(HandshakeFailure))",
        "which was received by client"
    );
}

#[test]
fn server_exposes_offered_sni() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version], &provider);
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            server_name("second.testserver.com"),
        )
        .unwrap();
        let mut server =
            ServerConnection::new(Arc::new(make_server_config(kt, &provider))).unwrap();

        assert_eq!(None, server.server_name());
        do_handshake(&mut client, &mut server);
        assert_eq!(
            Some(&DnsName::try_from("second.testserver.com").unwrap()),
            server.server_name()
        );
    }
}

#[test]
fn server_exposes_offered_sni_smashed_to_lowercase() {
    // webpki actually does this for us in its DnsName type
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version], &provider);
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            server_name("SECOND.TESTServer.com"),
        )
        .unwrap();
        let mut server =
            ServerConnection::new(Arc::new(make_server_config(kt, &provider))).unwrap();

        assert_eq!(None, server.server_name());
        do_handshake(&mut client, &mut server);
        assert_eq!(
            Some(&DnsName::try_from("second.testserver.com").unwrap()),
            server.server_name()
        );
    }
}

#[test]
fn server_exposes_offered_sni_even_if_resolver_fails() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let resolver = rustls::server::ResolvesServerCertUsingSni::new();

    let mut server_config = make_server_config(kt, &provider);
    server_config.cert_resolver = Arc::new(resolver);
    let server_config = Arc::new(server_config);

    for version in rustls::ALL_VERSIONS {
        let client_config = make_client_config_with_versions(kt, &[version], &provider);
        let mut server = ServerConnection::new(server_config.clone()).unwrap();
        let mut client =
            ClientConnection::new(Arc::new(client_config), server_name("thisdoesNOTexist.com"))
                .unwrap();

        assert_eq!(None, server.server_name());
        transfer(&mut client, &mut server);
        assert_eq!(
            server.process_new_packets(),
            Err(Error::General(
                "no server certificate chain resolved".to_string()
            ))
        );
        assert_eq!(
            Some(&DnsName::try_from("thisdoesnotexist.com").unwrap()),
            server.server_name()
        );
    }
}

#[test]
fn sni_resolver_works() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    let signing_key = RsaSigningKey::new(&kt.get_key()).unwrap();
    let signing_key: Arc<dyn sign::SigningKey> = Arc::new(signing_key);
    resolver
        .add(
            DnsName::try_from("localhost").unwrap(),
            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()).expect("keys match"),
        )
        .unwrap();

    let mut server_config = make_server_config(kt, &provider);
    server_config.cert_resolver = Arc::new(resolver);
    let server_config = Arc::new(server_config);

    let mut server1 = ServerConnection::new(server_config.clone()).unwrap();
    let mut client1 = ClientConnection::new(
        Arc::new(make_client_config(kt, &provider)),
        server_name("localhost"),
    )
    .unwrap();
    let err = do_handshake_until_error(&mut client1, &mut server1);
    assert_eq!(err, Ok(()));

    let mut server2 = ServerConnection::new(server_config.clone()).unwrap();
    let mut client2 = ClientConnection::new(
        Arc::new(make_client_config(kt, &provider)),
        server_name("notlocalhost"),
    )
    .unwrap();
    let err = do_handshake_until_error(&mut client2, &mut server2);
    assert_eq!(
        err,
        Err(ErrorFromPeer::Server(Error::General(
            "no server certificate chain resolved".into()
        )))
    );
}

#[test]
fn sni_resolver_rejects_wrong_names() {
    let kt = KeyType::Rsa2048;
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    let signing_key = RsaSigningKey::new(&kt.get_key()).unwrap();
    let signing_key: Arc<dyn sign::SigningKey> = Arc::new(signing_key);

    assert_eq!(
        Ok(()),
        resolver.add(
            DnsName::try_from("localhost").unwrap(),
            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()).expect("keys match")
        )
    );
    assert_eq!(
        Err(Error::InvalidCertificate(certificate_error_expecting_name(
            "not-localhost"
        ))),
        resolver.add(
            DnsName::try_from("not-localhost").unwrap(),
            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()).expect("keys match")
        )
    );
}

fn certificate_error_expecting_name(expected: &str) -> CertificateError {
    CertificateError::NotValidForNameContext {
        expected: ServerName::try_from(expected)
            .unwrap()
            .to_owned(),
        presented: vec![
            // ref. examples/internal/test_ca.rs
            r#"DnsName("testserver.com")"#.into(),
            r#"DnsName("second.testserver.com")"#.into(),
            r#"DnsName("localhost")"#.into(),
            "IpAddress(198.51.100.1)".into(),
            "IpAddress(2001:db8::1)".into(),
        ],
    }
}

#[test]
fn sni_resolver_lower_cases_configured_names() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    let signing_key = RsaSigningKey::new(&kt.get_key()).unwrap();
    let signing_key: Arc<dyn sign::SigningKey> = Arc::new(signing_key);

    assert_eq!(
        Ok(()),
        resolver.add(
            DnsName::try_from("LOCALHOST").unwrap(),
            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()).expect("keys match")
        )
    );

    let mut server_config = make_server_config(kt, &provider);
    server_config.cert_resolver = Arc::new(resolver);
    let server_config = Arc::new(server_config);

    let mut server1 = ServerConnection::new(server_config.clone()).unwrap();
    let mut client1 = ClientConnection::new(
        Arc::new(make_client_config(kt, &provider)),
        server_name("localhost"),
    )
    .unwrap();
    let err = do_handshake_until_error(&mut client1, &mut server1);
    assert_eq!(err, Ok(()));
}

#[test]
fn sni_resolver_lower_cases_queried_names() {
    // actually, the handshake parser does this, but the effect is the same.
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    let signing_key = RsaSigningKey::new(&kt.get_key()).unwrap();
    let signing_key: Arc<dyn sign::SigningKey> = Arc::new(signing_key);

    assert_eq!(
        Ok(()),
        resolver.add(
            DnsName::try_from("localhost").unwrap(),
            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()).expect("keys match")
        )
    );

    let mut server_config = make_server_config(kt, &provider);
    server_config.cert_resolver = Arc::new(resolver);
    let server_config = Arc::new(server_config);

    let mut server1 = ServerConnection::new(server_config.clone()).unwrap();
    let mut client1 = ClientConnection::new(
        Arc::new(make_client_config(kt, &provider)),
        server_name("LOCALHOST"),
    )
    .unwrap();
    let err = do_handshake_until_error(&mut client1, &mut server1);
    assert_eq!(err, Ok(()));
}

#[test]
fn sni_resolver_rejects_bad_certs() {
    let kt = KeyType::Rsa2048;
    let mut resolver = rustls::server::ResolvesServerCertUsingSni::new();
    let signing_key = RsaSigningKey::new(&kt.get_key()).unwrap();
    let signing_key: Arc<dyn sign::SigningKey> = Arc::new(signing_key);

    assert_eq!(
        Err(Error::NoCertificatesPresented),
        resolver.add(
            DnsName::try_from("localhost").unwrap(),
            sign::CertifiedKey::new_unchecked(vec![], signing_key.clone())
        )
    );

    let bad_chain = vec![CertificateDer::from(vec![0xa0])];
    assert_eq!(
        Err(Error::InvalidCertificate(CertificateError::BadEncoding)),
        resolver.add(
            DnsName::try_from("localhost").unwrap(),
            sign::CertifiedKey::new_unchecked(bad_chain, signing_key.clone())
        )
    );
}

#[test]
fn test_keys_match() {
    // Consistent: Both of these should have the same SPKI values
    let expect_consistent =
        sign::CertifiedKey::new(KeyType::Rsa2048.get_chain(), Arc::new(SigningKeySomeSpki));
    assert!(expect_consistent.is_ok());

    // Inconsistent: These should not have the same SPKI values
    let expect_inconsistent =
        sign::CertifiedKey::new(KeyType::EcdsaP256.get_chain(), Arc::new(SigningKeySomeSpki));
    assert!(matches!(
        expect_inconsistent,
        Err(Error::InconsistentKeys(InconsistentKeys::KeyMismatch))
    ));

    // Unknown: This signing key returns None for its SPKI, so we can't tell if the certified key is consistent
    assert!(matches!(
        sign::CertifiedKey::new(KeyType::Rsa2048.get_chain(), Arc::new(SigningKeyNoneSpki)),
        Err(Error::InconsistentKeys(InconsistentKeys::Unknown))
    ));
}

/// Represents a SigningKey that returns None for its SPKI via the default impl.
#[derive(Debug)]
struct SigningKeyNoneSpki;

impl sign::SigningKey for SigningKeyNoneSpki {
    fn choose_scheme(&self, _offered: &[SignatureScheme]) -> Option<Box<dyn sign::Signer>> {
        unimplemented!("Not meant to be called during tests")
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        None
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        unimplemented!("Not meant to be called during tests")
    }
}

/// Represents a SigningKey that returns Some for its SPKI.
#[derive(Debug)]
struct SigningKeySomeSpki;

impl sign::SigningKey for SigningKeySomeSpki {
    fn public_key(&self) -> Option<pki_types::SubjectPublicKeyInfoDer> {
        let chain = KeyType::Rsa2048.get_chain();
        let cert = ParsedCertificate::try_from(chain.first().unwrap()).unwrap();
        Some(
            cert.subject_public_key_info()
                .into_owned(),
        )
    }

    fn choose_scheme(&self, _offered: &[SignatureScheme]) -> Option<Box<dyn sign::Signer>> {
        unimplemented!("Not meant to be called during tests")
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        unimplemented!("Not meant to be called during tests")
    }
}

fn do_exporter_test(client_config: ClientConfig, server_config: ServerConfig) {
    let mut client_secret = [0u8; 64];
    let mut server_secret = [0u8; 64];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(
        Err(Error::HandshakeNotComplete),
        client.export_keying_material(&mut client_secret, b"label", Some(b"context"))
    );
    assert_eq!(
        Err(Error::HandshakeNotComplete),
        server.export_keying_material(&mut server_secret, b"label", Some(b"context"))
    );
    do_handshake(&mut client, &mut server);

    assert!(
        client
            .export_keying_material(&mut client_secret, b"label", Some(b"context"))
            .is_ok()
    );
    assert!(
        server
            .export_keying_material(&mut server_secret, b"label", Some(b"context"))
            .is_ok()
    );
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());

    let mut empty = vec![];
    assert_eq!(
        client
            .export_keying_material(&mut empty, b"label", Some(b"context"))
            .err(),
        Some(Error::General(
            "export_keying_material with zero-length output".into()
        ))
    );
    assert_eq!(
        server
            .export_keying_material(&mut empty, b"label", Some(b"context"))
            .err(),
        Some(Error::General(
            "export_keying_material with zero-length output".into()
        ))
    );

    assert!(
        client
            .export_keying_material(&mut client_secret, b"label", None)
            .is_ok()
    );
    assert_ne!(client_secret.to_vec(), server_secret.to_vec());
    assert!(
        server
            .export_keying_material(&mut server_secret, b"label", None)
            .is_ok()
    );
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());
}

#[test]
fn test_tls12_exporter() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let client_config =
            make_client_config_with_versions(*kt, &[&rustls::version::TLS12], &provider);
        let server_config = make_server_config(*kt, &provider);

        do_exporter_test(client_config, server_config);
    }
}

#[test]
fn test_tls13_exporter() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let client_config =
            make_client_config_with_versions(*kt, &[&rustls::version::TLS13], &provider);
        let server_config = make_server_config(*kt, &provider);

        do_exporter_test(client_config, server_config);
    }
}

#[test]
fn test_tls13_exporter_maximum_output_length() {
    let provider = provider::default_provider();
    let client_config =
        make_client_config_with_versions(KeyType::EcdsaP256, &[&rustls::version::TLS13], &provider);
    let server_config = make_server_config(KeyType::EcdsaP256, &provider);

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    assert_eq!(
        client.negotiated_cipher_suite(),
        Some(find_suite(CipherSuite::TLS13_AES_256_GCM_SHA384))
    );

    let mut maximum_allowed_output_client = [0u8; 255 * 48];
    let mut maximum_allowed_output_server = [0u8; 255 * 48];
    client
        .export_keying_material(
            &mut maximum_allowed_output_client,
            b"label",
            Some(b"context"),
        )
        .unwrap();
    server
        .export_keying_material(
            &mut maximum_allowed_output_server,
            b"label",
            Some(b"context"),
        )
        .unwrap();

    assert_eq!(maximum_allowed_output_client, maximum_allowed_output_server);

    let mut too_long_output = [0u8; 255 * 48 + 1];
    assert_eq!(
        client
            .export_keying_material(&mut too_long_output, b"label", Some(b"context"),)
            .err(),
        Some(Error::General("exporting too much".into()))
    );
    assert_eq!(
        server
            .export_keying_material(&mut too_long_output, b"label", Some(b"context"),)
            .err(),
        Some(Error::General("exporting too much".into()))
    );
}

fn find_suite(suite: CipherSuite) -> SupportedCipherSuite {
    for scs in provider::ALL_CIPHER_SUITES
        .iter()
        .copied()
    {
        if scs.suite() == suite {
            return scs;
        }
    }

    panic!("find_suite given unsupported suite");
}

fn test_ciphersuites() -> Vec<(
    &'static rustls::SupportedProtocolVersion,
    KeyType,
    CipherSuite,
)> {
    let mut v = vec![
        (
            &rustls::version::TLS13,
            KeyType::Rsa2048,
            CipherSuite::TLS13_AES_256_GCM_SHA384,
        ),
        (
            &rustls::version::TLS13,
            KeyType::Rsa2048,
            CipherSuite::TLS13_AES_128_GCM_SHA256,
        ),
        (
            &rustls::version::TLS12,
            KeyType::EcdsaP384,
            CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            &rustls::version::TLS12,
            KeyType::EcdsaP384,
            CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ),
        (
            &rustls::version::TLS12,
            KeyType::Rsa2048,
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            &rustls::version::TLS12,
            KeyType::Rsa2048,
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        ),
    ];

    if !provider_is_fips() {
        v.extend_from_slice(&[
            (
                &rustls::version::TLS13,
                KeyType::Rsa2048,
                CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            ),
            (
                &rustls::version::TLS12,
                KeyType::EcdsaP256,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            ),
            (
                &rustls::version::TLS12,
                KeyType::Rsa2048,
                CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
            ),
        ]);
    }

    v
}

#[test]
fn negotiated_ciphersuite_default() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        do_suite_and_kx_test(
            make_client_config(*kt, &provider),
            make_server_config(*kt, &provider),
            find_suite(CipherSuite::TLS13_AES_256_GCM_SHA384),
            expected_kx_for_version(&rustls::version::TLS13),
            ProtocolVersion::TLSv1_3,
        );
    }
}

#[test]
fn all_suites_covered() {
    assert_eq!(
        provider::DEFAULT_CIPHER_SUITES.len(),
        test_ciphersuites().len()
    );
}

#[test]
fn negotiated_ciphersuite_client() {
    for (version, kt, suite) in test_ciphersuites() {
        let scs = find_suite(suite);
        let client_config = finish_client_config(
            kt,
            ClientConfig::builder_with_provider(
                CryptoProvider {
                    cipher_suites: vec![scs],
                    ..provider::default_provider()
                }
                .into(),
            )
            .with_protocol_versions(&[version])
            .unwrap(),
        );

        do_suite_and_kx_test(
            client_config,
            make_server_config(kt, &provider::default_provider()),
            scs,
            expected_kx_for_version(version),
            version.version(),
        );
    }
}

#[test]
fn negotiated_ciphersuite_server() {
    for (version, kt, suite) in test_ciphersuites() {
        let scs = find_suite(suite);
        let server_config = finish_server_config(
            kt,
            ServerConfig::builder_with_provider(
                CryptoProvider {
                    cipher_suites: vec![scs],
                    ..provider::default_provider()
                }
                .into(),
            )
            .with_protocol_versions(&[version])
            .unwrap(),
        );

        do_suite_and_kx_test(
            make_client_config(kt, &provider::default_provider()),
            server_config,
            scs,
            expected_kx_for_version(version),
            version.version(),
        );
    }
}

#[test]
fn negotiated_ciphersuite_server_ignoring_client_preference() {
    for (version, kt, suite) in test_ciphersuites() {
        let scs = find_suite(suite);
        let scs_other = if scs.suite() == CipherSuite::TLS13_AES_256_GCM_SHA384 {
            find_suite(CipherSuite::TLS13_AES_128_GCM_SHA256)
        } else {
            find_suite(CipherSuite::TLS13_AES_256_GCM_SHA384)
        };
        let mut server_config = finish_server_config(
            kt,
            ServerConfig::builder_with_provider(
                CryptoProvider {
                    cipher_suites: vec![scs, scs_other],
                    ..provider::default_provider()
                }
                .into(),
            )
            .with_protocol_versions(&[version])
            .unwrap(),
        );
        server_config.ignore_client_order = true;

        let client_config = finish_client_config(
            kt,
            ClientConfig::builder_with_provider(
                CryptoProvider {
                    cipher_suites: vec![scs_other, scs],
                    ..provider::default_provider()
                }
                .into(),
            )
            .with_safe_default_protocol_versions()
            .unwrap(),
        );

        do_suite_and_kx_test(
            client_config,
            server_config,
            scs,
            expected_kx_for_version(version),
            version.version(),
        );
    }
}

fn expected_kx_for_version(version: &SupportedProtocolVersion) -> NamedGroup {
    match (
        version.version(),
        provider_is_aws_lc_rs(),
        provider_is_fips(),
    ) {
        (ProtocolVersion::TLSv1_3, true, _) => NamedGroup::X25519MLKEM768,
        (_, _, true) => NamedGroup::secp256r1,
        (_, _, _) => NamedGroup::X25519,
    }
}

#[derive(Debug, PartialEq)]
struct KeyLogItem {
    label: String,
    client_random: Vec<u8>,
    secret: Vec<u8>,
}

#[derive(Debug)]
struct KeyLogToVec {
    label: &'static str,
    items: Mutex<Vec<KeyLogItem>>,
}

impl KeyLogToVec {
    fn new(who: &'static str) -> Self {
        Self {
            label: who,
            items: Mutex::new(vec![]),
        }
    }

    fn take(&self) -> Vec<KeyLogItem> {
        std::mem::take(&mut self.items.lock().unwrap())
    }
}

impl KeyLog for KeyLogToVec {
    fn log(&self, label: &str, client: &[u8], secret: &[u8]) {
        let value = KeyLogItem {
            label: label.into(),
            client_random: client.into(),
            secret: secret.into(),
        };

        println!("key log {:?}: {:?}", self.label, value);

        self.items.lock().unwrap().push(value);
    }
}

#[test]
fn key_log_for_tls12() {
    let client_key_log = Arc::new(KeyLogToVec::new("client"));
    let server_key_log = Arc::new(KeyLogToVec::new("server"));

    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let mut client_config =
        make_client_config_with_versions(kt, &[&rustls::version::TLS12], &provider);
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt, &provider);
    server_config.key_log = server_key_log.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let client_full_log = client_key_log.take();
    let server_full_log = server_key_log.take();
    assert_eq!(client_full_log, server_full_log);
    assert_eq!(1, client_full_log.len());
    assert_eq!("CLIENT_RANDOM", client_full_log[0].label);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let client_resume_log = client_key_log.take();
    let server_resume_log = server_key_log.take();
    assert_eq!(client_resume_log, server_resume_log);
    assert_eq!(1, client_resume_log.len());
    assert_eq!("CLIENT_RANDOM", client_resume_log[0].label);
    assert_eq!(client_full_log[0].secret, client_resume_log[0].secret);
}

#[test]
fn key_log_for_tls13() {
    let client_key_log = Arc::new(KeyLogToVec::new("client"));
    let server_key_log = Arc::new(KeyLogToVec::new("server"));

    let provider = provider::default_provider();
    let kt = KeyType::Rsa2048;
    let mut client_config =
        make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt, &provider);
    server_config.key_log = server_key_log.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let client_full_log = client_key_log.take();
    let server_full_log = server_key_log.take();

    assert_eq!(5, client_full_log.len());
    assert_eq!("CLIENT_HANDSHAKE_TRAFFIC_SECRET", client_full_log[0].label);
    assert_eq!("SERVER_HANDSHAKE_TRAFFIC_SECRET", client_full_log[1].label);
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_full_log[2].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_full_log[3].label);
    assert_eq!("EXPORTER_SECRET", client_full_log[4].label);

    assert_eq!(client_full_log[0], server_full_log[0]);
    assert_eq!(client_full_log[1], server_full_log[1]);
    assert_eq!(client_full_log[2], server_full_log[2]);
    assert_eq!(client_full_log[3], server_full_log[3]);
    assert_eq!(client_full_log[4], server_full_log[4]);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let client_resume_log = client_key_log.take();
    let server_resume_log = server_key_log.take();

    assert_eq!(5, client_resume_log.len());
    assert_eq!(
        "CLIENT_HANDSHAKE_TRAFFIC_SECRET",
        client_resume_log[0].label
    );
    assert_eq!(
        "SERVER_HANDSHAKE_TRAFFIC_SECRET",
        client_resume_log[1].label
    );
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_resume_log[2].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_resume_log[3].label);
    assert_eq!("EXPORTER_SECRET", client_resume_log[4].label);

    assert_eq!(6, server_resume_log.len());
    assert_eq!("CLIENT_EARLY_TRAFFIC_SECRET", server_resume_log[0].label);
    assert_eq!(
        "CLIENT_HANDSHAKE_TRAFFIC_SECRET",
        server_resume_log[1].label
    );
    assert_eq!(
        "SERVER_HANDSHAKE_TRAFFIC_SECRET",
        server_resume_log[2].label
    );
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", server_resume_log[3].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", server_resume_log[4].label);
    assert_eq!("EXPORTER_SECRET", server_resume_log[5].label);

    assert_eq!(client_resume_log[0], server_resume_log[1]);
    assert_eq!(client_resume_log[1], server_resume_log[2]);
    assert_eq!(client_resume_log[2], server_resume_log[3]);
    assert_eq!(client_resume_log[3], server_resume_log[4]);
    assert_eq!(client_resume_log[4], server_resume_log[5]);
}

#[test]
fn vectored_write_for_server_appdata() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    server
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    server
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(
        &mut client.reader(),
        b"0123456789012345678901234567890123456789",
    );
}

#[test]
fn vectored_write_for_client_appdata() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    client
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    client
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(
        &mut server.reader(),
        b"0123456789012345678901234567890123456789",
    );
}

#[test]
fn vectored_write_for_server_handshake_with_half_rtt_data() {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.send_half_rtt_data = true;
    let (mut client, mut server) = make_pair_for_configs(
        make_client_config_with_auth(KeyType::Rsa2048, &provider),
        server_config,
    );

    server
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    server
        .writer()
        .write_all(b"0123456789")
        .unwrap();

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 2400); // its pretty big (contains cert chain)
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert_eq!(pipe.writevs[0].len(), 5); // at least a server hello/ccs/cert/serverkx/0.5rtt data
    }

    client.process_new_packets().unwrap();
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        // 2 tickets (in one flight)
        assert_eq!(wrlen, 184);
        assert_eq!(pipe.writevs, vec![vec![184]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut client.reader(), b"012345678901234567890123456789");
}

fn check_half_rtt_does_not_work(server_config: ServerConfig) {
    let (mut client, mut server) = make_pair_for_configs(
        make_client_config_with_auth(KeyType::Rsa2048, &provider::default_provider()),
        server_config,
    );

    server
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    server
        .writer()
        .write_all(b"0123456789")
        .unwrap();

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 2400); // its pretty big (contains cert chain)
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert_eq!(pipe.writevs[0].len(), 3); // at least a server hello/ccs/cert/serverkx data, in one message
    }

    // client second flight
    client.process_new_packets().unwrap();
    transfer(&mut client, &mut server);

    // when client auth is enabled, we don't sent 0.5-rtt data, as we'd be sending
    // it to an unauthenticated peer. so it happens here, in the server's second
    // flight (42 and 32 are lengths of appdata sent above).
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 258);
        assert_eq!(pipe.writevs, vec![vec![184, 42, 32]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut client.reader(), b"012345678901234567890123456789");
}

#[test]
fn vectored_write_for_server_handshake_no_half_rtt_with_client_auth() {
    let mut server_config = make_server_config_with_mandatory_client_auth(
        KeyType::Rsa2048,
        &provider::default_provider(),
    );
    server_config.send_half_rtt_data = true; // ask even though it will be ignored
    check_half_rtt_does_not_work(server_config);
}

#[test]
fn vectored_write_for_server_handshake_no_half_rtt_by_default() {
    let server_config = make_server_config(KeyType::Rsa2048, &provider::default_provider());
    assert!(!server_config.send_half_rtt_data);
    check_half_rtt_does_not_work(server_config);
}

#[test]
fn vectored_write_for_client_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    client
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();
    client
        .writer()
        .write_all(b"0123456789")
        .unwrap();
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 200); // just the client hello
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 1); // only a client hello
    }

    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 154);
        // CCS, finished, then two application data records
        assert_eq!(pipe.writevs, vec![vec![6, 74, 42, 32]]);
    }

    assert!(!server.is_handshaking());
    assert!(!client.is_handshaking());
    check_read(&mut server.reader(), b"012345678901234567890123456789");
}

#[test]
fn vectored_write_with_slow_client() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());

    client.set_buffer_limit(Some(32));

    do_handshake(&mut client, &mut server);
    server
        .writer()
        .write_all(b"01234567890123456789")
        .unwrap();

    {
        let mut pipe = OtherSession::new(&mut client);
        pipe.short_writes = true;
        let wrlen = server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap()
            + server.write_tls(&mut pipe).unwrap();
        assert_eq!(42, wrlen);
        assert_eq!(
            pipe.writevs,
            vec![vec![21], vec![10], vec![5], vec![3], vec![3]]
        );
    }
    check_read(&mut client.reader(), b"01234567890123456789");
}

struct ServerStorage {
    storage: Arc<dyn rustls::server::StoresServerSessions>,
    put_count: AtomicUsize,
    get_count: AtomicUsize,
    take_count: AtomicUsize,
}

impl ServerStorage {
    fn new() -> Self {
        Self {
            storage: rustls::server::ServerSessionMemoryCache::new(1024),
            put_count: AtomicUsize::new(0),
            get_count: AtomicUsize::new(0),
            take_count: AtomicUsize::new(0),
        }
    }

    fn puts(&self) -> usize {
        self.put_count.load(Ordering::SeqCst)
    }
    fn gets(&self) -> usize {
        self.get_count.load(Ordering::SeqCst)
    }
    fn takes(&self) -> usize {
        self.take_count.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for ServerStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(put: {:?}, get: {:?}, take: {:?})",
            self.put_count, self.get_count, self.take_count
        )
    }
}

impl rustls::server::StoresServerSessions for ServerStorage {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.put_count
            .fetch_add(1, Ordering::SeqCst);
        self.storage.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_count
            .fetch_add(1, Ordering::SeqCst);
        self.storage.get(key)
    }

    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.take_count
            .fetch_add(1, Ordering::SeqCst);
        self.storage.take(key)
    }

    fn can_cache(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // complete mock, but not 100% used in tests
enum ClientStorageOp {
    SetKxHint(ServerName<'static>, rustls::NamedGroup),
    GetKxHint(ServerName<'static>, Option<rustls::NamedGroup>),
    SetTls12Session(ServerName<'static>),
    GetTls12Session(ServerName<'static>, bool),
    RemoveTls12Session(ServerName<'static>),
    InsertTls13Ticket(ServerName<'static>),
    TakeTls13Ticket(ServerName<'static>, bool),
}

struct ClientStorage {
    storage: Arc<dyn rustls::client::ClientSessionStore>,
    ops: Mutex<Vec<ClientStorageOp>>,
    alter_max_early_data_size: Option<(u32, u32)>,
}

impl ClientStorage {
    fn new() -> Self {
        Self {
            storage: Arc::new(rustls::client::ClientSessionMemoryCache::new(1024)),
            ops: Mutex::new(Vec::new()),
            alter_max_early_data_size: None,
        }
    }

    fn alter_max_early_data_size(&mut self, expected: u32, altered: u32) {
        self.alter_max_early_data_size = Some((expected, altered));
    }

    fn ops(&self) -> Vec<ClientStorageOp> {
        self.ops.lock().unwrap().clone()
    }

    fn ops_and_reset(&self) -> Vec<ClientStorageOp> {
        std::mem::take(&mut self.ops.lock().unwrap())
    }
}

impl fmt::Debug for ClientStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(ops: {:?})", self.ops.lock().unwrap())
    }
}

impl rustls::client::ClientSessionStore for ClientStorage {
    fn set_kx_hint(&self, server_name: ServerName<'static>, group: rustls::NamedGroup) {
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::SetKxHint(server_name.clone(), group));
        self.storage
            .set_kx_hint(server_name, group)
    }

    fn kx_hint(&self, server_name: &ServerName<'_>) -> Option<rustls::NamedGroup> {
        let rc = self.storage.kx_hint(server_name);
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::GetKxHint(server_name.to_owned(), rc));
        rc
    }

    fn set_tls12_session(
        &self,
        server_name: ServerName<'static>,
        value: rustls::client::Tls12ClientSessionValue,
    ) {
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::SetTls12Session(server_name.clone()));
        self.storage
            .set_tls12_session(server_name, value)
    }

    fn tls12_session(
        &self,
        server_name: &ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        let rc = self.storage.tls12_session(server_name);
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::GetTls12Session(
                server_name.to_owned(),
                rc.is_some(),
            ));
        rc
    }

    fn remove_tls12_session(&self, server_name: &ServerName<'static>) {
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::RemoveTls12Session(server_name.clone()));
        self.storage
            .remove_tls12_session(server_name);
    }

    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        mut value: rustls::client::Tls13ClientSessionValue,
    ) {
        if let Some((expected, desired)) = self.alter_max_early_data_size {
            assert_eq!(value.max_early_data_size(), expected);
            value._private_set_max_early_data_size(desired);
        }

        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::InsertTls13Ticket(server_name.clone()));
        self.storage
            .insert_tls13_ticket(server_name, value);
    }

    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        let rc = self
            .storage
            .take_tls13_ticket(server_name);
        self.ops
            .lock()
            .unwrap()
            .push(ClientStorageOp::TakeTls13Ticket(
                server_name.clone(),
                rc.is_some(),
            ));
        rc
    }
}

#[test]
fn tls13_stateful_resumption() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let client_config = make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt, &provider);
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(client.tls13_tickets_received(), 2);
    assert_eq!(storage.puts(), 2);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Full));

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server);
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 4);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 1);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Resumed));

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 6);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 2);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Resumed));
}

#[test]
fn tls13_stateless_resumption() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let client_config = make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt, &provider);
    server_config.ticketer = provider::Ticketer::new().unwrap();
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Full));

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server);
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Resumed));

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
    assert_eq!(
        client
            .peer_certificates()
            .map(|certs| certs.len()),
        Some(3)
    );
    assert_eq!(client.handshake_kind(), Some(HandshakeKind::Resumed));
    assert_eq!(server.handshake_kind(), Some(HandshakeKind::Resumed));
}

#[test]
fn early_data_not_available() {
    let (mut client, _) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    assert!(client.early_data().is_none());
}

fn early_data_configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    let mut client_config = make_client_config(kt, &provider);
    client_config.enable_early_data = true;
    client_config.resumption = Resumption::store(Arc::new(ClientStorage::new()));

    let mut server_config = make_server_config(kt, &provider);
    server_config.max_early_data_size = 1234;
    (Arc::new(client_config), Arc::new(server_config))
}

#[test]
fn early_data_is_available_on_resumption() {
    let (client_config, server_config) = early_data_configs();

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    assert!(client.early_data().is_some());
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .bytes_left(),
        1234
    );
    client
        .early_data()
        .unwrap()
        .flush()
        .unwrap();
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(b"hello")
            .unwrap(),
        5
    );
    do_handshake(&mut client, &mut server);

    let mut received_early_data = [0u8; 5];
    assert_eq!(
        server
            .early_data()
            .expect("early_data didn't happen")
            .read(&mut received_early_data)
            .expect("early_data failed unexpectedly"),
        5
    );
    assert_eq!(&received_early_data[..], b"hello");
}

#[test]
fn early_data_not_available_on_server_before_client_hello() {
    let mut server = ServerConnection::new(Arc::new(make_server_config(
        KeyType::Rsa2048,
        &provider::default_provider(),
    )))
    .unwrap();
    assert!(server.early_data().is_none());
}

#[test]
fn early_data_can_be_rejected_by_server() {
    let (client_config, server_config) = early_data_configs();

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    assert!(client.early_data().is_some());
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .bytes_left(),
        1234
    );
    client
        .early_data()
        .unwrap()
        .flush()
        .unwrap();
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(b"hello")
            .unwrap(),
        5
    );
    server.reject_early_data();
    do_handshake(&mut client, &mut server);

    assert!(!client.is_early_data_accepted());
}

#[test]
fn early_data_is_limited_on_client() {
    let (client_config, server_config) = early_data_configs();

    // warm up
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    assert!(client.early_data().is_some());
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .bytes_left(),
        1234
    );
    client
        .early_data()
        .unwrap()
        .flush()
        .unwrap();
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(&[0xaa; 1234 + 1])
            .unwrap(),
        1234
    );
    do_handshake(&mut client, &mut server);

    let mut received_early_data = [0u8; 1234];
    assert_eq!(
        server
            .early_data()
            .expect("early_data didn't happen")
            .read(&mut received_early_data)
            .expect("early_data failed unexpectedly"),
        1234
    );
    assert_eq!(&received_early_data[..], [0xaa; 1234]);
}

fn early_data_configs_allowing_client_to_send_excess_data() -> (Arc<ClientConfig>, Arc<ServerConfig>)
{
    let (client_config, server_config) = early_data_configs();

    // adjust client session storage to corrupt received max_early_data_size
    let mut client_config = Arc::into_inner(client_config).unwrap();
    let mut storage = ClientStorage::new();
    storage.alter_max_early_data_size(1234, 2024);
    client_config.resumption = Resumption::store(Arc::new(storage));
    let client_config = Arc::new(client_config);

    // warm up
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);
    (client_config, server_config)
}

#[test]
fn server_detects_excess_early_data() {
    let (client_config, server_config) = early_data_configs_allowing_client_to_send_excess_data();

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    assert!(client.early_data().is_some());
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .bytes_left(),
        2024
    );
    client
        .early_data()
        .unwrap()
        .flush()
        .unwrap();
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(&[0xaa; 2024])
            .unwrap(),
        2024
    );
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Server(Error::PeerMisbehaved(
            PeerMisbehaved::TooMuchEarlyDataReceived
        ))),
    );
}

// regression test for https://github.com/rustls/rustls/issues/2096
#[test]
fn server_detects_excess_streamed_early_data() {
    let (client_config, server_config) = early_data_configs_allowing_client_to_send_excess_data();

    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    assert!(client.early_data().is_some());
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .bytes_left(),
        2024
    );
    client
        .early_data()
        .unwrap()
        .flush()
        .unwrap();
    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(&[0xaa; 1024])
            .unwrap(),
        1024
    );
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let mut received_early_data = [0u8; 1024];
    assert_eq!(
        server
            .early_data()
            .expect("early_data didn't happen")
            .read(&mut received_early_data)
            .expect("early_data failed unexpectedly"),
        1024
    );
    assert_eq!(&received_early_data[..], [0xaa; 1024]);

    assert_eq!(
        client
            .early_data()
            .unwrap()
            .write(&[0xbb; 1000])
            .unwrap(),
        1000
    );
    transfer(&mut client, &mut server);
    assert_eq!(
        server.process_new_packets(),
        Err(Error::PeerMisbehaved(
            PeerMisbehaved::TooMuchEarlyDataReceived
        ))
    );
}

mod test_quic {
    use rustls::quic::{self, ConnectionCommon};

    use super::*;

    // Returns the sender's next secrets to use, or the receiver's error.
    fn step<L: SideData, R: SideData>(
        send: &mut ConnectionCommon<L>,
        recv: &mut ConnectionCommon<R>,
    ) -> Result<Option<quic::KeyChange>, Error> {
        let mut buf = Vec::new();
        let change = loop {
            let prev = buf.len();
            if let Some(x) = send.write_hs(&mut buf) {
                break Some(x);
            }
            if prev == buf.len() {
                break None;
            }
        };

        recv.read_hs(&buf)?;
        assert_eq!(recv.alert(), None);
        Ok(change)
    }

    #[test]
    fn test_quic_handshake() {
        fn equal_packet_keys(x: &dyn quic::PacketKey, y: &dyn quic::PacketKey) -> bool {
            // Check that these two sets of keys are equal.
            let mut buf = [0; 32];
            let (header, payload_tag) = buf.split_at_mut(8);
            let (payload, tag_buf) = payload_tag.split_at_mut(8);
            let tag = x
                .encrypt_in_place(42, header, payload)
                .unwrap();
            tag_buf.copy_from_slice(tag.as_ref());

            let result = y.decrypt_in_place(42, header, payload_tag);
            match result {
                Ok(payload) => payload == [0; 8],
                Err(_) => false,
            }
        }

        fn compatible_keys(x: &quic::KeyChange, y: &quic::KeyChange) -> bool {
            fn keys(kc: &quic::KeyChange) -> &quic::Keys {
                match kc {
                    quic::KeyChange::Handshake { keys } => keys,
                    quic::KeyChange::OneRtt { keys, .. } => keys,
                }
            }

            let (x, y) = (keys(x), keys(y));
            equal_packet_keys(x.local.packet.as_ref(), y.remote.packet.as_ref())
                && equal_packet_keys(x.remote.packet.as_ref(), y.local.packet.as_ref())
        }

        let kt = KeyType::Rsa2048;
        let provider = provider::default_provider();
        let mut client_config =
            make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
        client_config.enable_early_data = true;
        let client_config = Arc::new(client_config);
        let mut server_config =
            make_server_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
        server_config.max_early_data_size = 0xffffffff;
        let server_config = Arc::new(server_config);
        let client_params = &b"client params"[..];
        let server_params = &b"server params"[..];

        // full handshake
        let mut client = quic::ClientConnection::new(
            client_config.clone(),
            quic::Version::V1,
            server_name("localhost"),
            client_params.into(),
        )
        .unwrap();

        let mut server = quic::ServerConnection::new(
            server_config.clone(),
            quic::Version::V1,
            server_params.into(),
        )
        .unwrap();

        let client_initial = step(&mut client, &mut server).unwrap();
        assert!(client_initial.is_none());
        assert!(client.zero_rtt_keys().is_none());
        assert_eq!(server.quic_transport_parameters(), Some(client_params));
        let server_hs = step(&mut server, &mut client)
            .unwrap()
            .unwrap();
        assert!(server.zero_rtt_keys().is_none());
        let client_hs = step(&mut client, &mut server)
            .unwrap()
            .unwrap();
        assert!(compatible_keys(&server_hs, &client_hs));
        assert!(client.is_handshaking());
        let server_1rtt = step(&mut server, &mut client)
            .unwrap()
            .unwrap();
        assert!(!client.is_handshaking());
        assert_eq!(client.quic_transport_parameters(), Some(server_params));
        assert!(server.is_handshaking());
        let client_1rtt = step(&mut client, &mut server)
            .unwrap()
            .unwrap();
        assert!(!server.is_handshaking());
        assert!(compatible_keys(&server_1rtt, &client_1rtt));
        assert!(!compatible_keys(&server_hs, &server_1rtt));

        assert!(
            step(&mut client, &mut server)
                .unwrap()
                .is_none()
        );
        assert!(
            step(&mut server, &mut client)
                .unwrap()
                .is_none()
        );
        assert_eq!(client.tls13_tickets_received(), 2);

        // 0-RTT handshake
        let mut client = quic::ClientConnection::new(
            client_config.clone(),
            quic::Version::V1,
            server_name("localhost"),
            client_params.into(),
        )
        .unwrap();
        assert!(
            client
                .negotiated_cipher_suite()
                .is_some()
        );

        let mut server = quic::ServerConnection::new(
            server_config.clone(),
            quic::Version::V1,
            server_params.into(),
        )
        .unwrap();

        step(&mut client, &mut server).unwrap();
        assert_eq!(client.quic_transport_parameters(), Some(server_params));
        {
            let client_early = client.zero_rtt_keys().unwrap();
            let server_early = server.zero_rtt_keys().unwrap();
            assert!(equal_packet_keys(
                client_early.packet.as_ref(),
                server_early.packet.as_ref()
            ));
        }
        step(&mut server, &mut client)
            .unwrap()
            .unwrap();
        step(&mut client, &mut server)
            .unwrap()
            .unwrap();
        step(&mut server, &mut client)
            .unwrap()
            .unwrap();
        assert!(client.is_early_data_accepted());
        // 0-RTT rejection
        {
            let client_config = (*client_config).clone();
            let mut client = quic::ClientConnection::new(
                Arc::new(client_config),
                quic::Version::V1,
                server_name("localhost"),
                client_params.into(),
            )
            .unwrap();

            let mut server = quic::ServerConnection::new(
                server_config.clone(),
                quic::Version::V1,
                server_params.into(),
            )
            .unwrap();
            server.reject_early_data();

            step(&mut client, &mut server).unwrap();
            assert_eq!(client.quic_transport_parameters(), Some(server_params));
            assert!(client.zero_rtt_keys().is_some());
            assert!(server.zero_rtt_keys().is_none());
            step(&mut server, &mut client)
                .unwrap()
                .unwrap();
            step(&mut client, &mut server)
                .unwrap()
                .unwrap();
            step(&mut server, &mut client)
                .unwrap()
                .unwrap();
            assert!(!client.is_early_data_accepted());
        }

        // failed handshake
        let mut client = quic::ClientConnection::new(
            client_config,
            quic::Version::V1,
            server_name("example.com"),
            client_params.into(),
        )
        .unwrap();

        let mut server =
            quic::ServerConnection::new(server_config, quic::Version::V1, server_params.into())
                .unwrap();

        step(&mut client, &mut server).unwrap();
        step(&mut server, &mut client)
            .unwrap()
            .unwrap();
        assert!(step(&mut server, &mut client).is_err());
        assert_eq!(
            client.alert(),
            Some(rustls::AlertDescription::BadCertificate)
        );

        // Key updates

        let (
            quic::KeyChange::OneRtt {
                next: mut client_secrets,
                ..
            },
            quic::KeyChange::OneRtt {
                next: mut server_secrets,
                ..
            },
        ) = (client_1rtt, server_1rtt)
        else {
            unreachable!();
        };

        let mut client_next = client_secrets.next_packet_keys();
        let mut server_next = server_secrets.next_packet_keys();
        assert!(equal_packet_keys(
            client_next.local.as_ref(),
            server_next.remote.as_ref()
        ));
        assert!(equal_packet_keys(
            server_next.local.as_ref(),
            client_next.remote.as_ref()
        ));

        client_next = client_secrets.next_packet_keys();
        server_next = server_secrets.next_packet_keys();
        assert!(equal_packet_keys(
            client_next.local.as_ref(),
            server_next.remote.as_ref()
        ));
        assert!(equal_packet_keys(
            server_next.local.as_ref(),
            client_next.remote.as_ref()
        ));
    }

    #[test]
    fn test_quic_rejects_missing_alpn() {
        let client_params = &b"client params"[..];
        let server_params = &b"server params"[..];
        let provider = provider::default_provider();

        for &kt in KeyType::all_for_provider(&provider) {
            let client_config =
                make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
            let client_config = Arc::new(client_config);

            let mut server_config =
                make_server_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
            server_config.alpn_protocols = vec!["foo".into()];
            let server_config = Arc::new(server_config);

            let mut client = quic::ClientConnection::new(
                client_config,
                quic::Version::V1,
                server_name("localhost"),
                client_params.into(),
            )
            .unwrap();
            let mut server =
                quic::ServerConnection::new(server_config, quic::Version::V1, server_params.into())
                    .unwrap();

            assert_eq!(
                step(&mut client, &mut server)
                    .err()
                    .unwrap(),
                Error::NoApplicationProtocol
            );

            assert_eq!(
                server.alert(),
                Some(rustls::AlertDescription::NoApplicationProtocol)
            );
        }
    }

    #[test]
    fn test_quic_no_tls13_error() {
        let provider = provider::default_provider();
        let mut client_config = make_client_config_with_versions(
            KeyType::Ed25519,
            &[&rustls::version::TLS12],
            &provider,
        );
        client_config.alpn_protocols = vec!["foo".into()];
        let client_config = Arc::new(client_config);

        assert!(
            quic::ClientConnection::new(
                client_config,
                quic::Version::V1,
                server_name("localhost"),
                b"client params".to_vec(),
            )
            .is_err()
        );

        let mut server_config = make_server_config_with_versions(
            KeyType::Ed25519,
            &[&rustls::version::TLS12],
            &provider,
        );
        server_config.alpn_protocols = vec!["foo".into()];
        let server_config = Arc::new(server_config);

        assert!(
            quic::ServerConnection::new(
                server_config,
                quic::Version::V1,
                b"server params".to_vec(),
            )
            .is_err()
        );
    }

    #[test]
    fn test_quic_invalid_early_data_size() {
        let provider = provider::default_provider();
        let mut server_config = make_server_config_with_versions(
            KeyType::Ed25519,
            &[&rustls::version::TLS13],
            &provider,
        );
        server_config.alpn_protocols = vec!["foo".into()];

        let cases = [
            (None, true),
            (Some(0u32), true),
            (Some(5), false),
            (Some(0xffff_ffff), true),
        ];

        for &(size, ok) in cases.iter() {
            println!("early data size case: {size:?}");
            if let Some(new) = size {
                server_config.max_early_data_size = new;
            }

            let wrapped = Arc::new(server_config.clone());
            assert_eq!(
                quic::ServerConnection::new(wrapped, quic::Version::V1, b"server params".to_vec(),)
                    .is_ok(),
                ok
            );
        }
    }

    #[test]
    fn test_quic_server_no_params_received() {
        let provider = provider::default_provider();
        let server_config = make_server_config_with_versions(
            KeyType::Ed25519,
            &[&rustls::version::TLS13],
            &provider,
        );
        let server_config = Arc::new(server_config);

        let mut server = quic::ServerConnection::new(
            server_config,
            quic::Version::V1,
            b"server params".to_vec(),
        )
        .unwrap();

        let buf = encoding::basic_client_hello(vec![]);
        assert_eq!(
            server.read_hs(buf.as_slice()).err(),
            Some(Error::PeerMisbehaved(
                PeerMisbehaved::MissingQuicTransportParameters
            ))
        );
    }

    #[test]
    fn test_quic_server_no_tls12() {
        let provider = provider::default_provider();
        let mut server_config = make_server_config_with_versions(
            KeyType::Ed25519,
            &[&rustls::version::TLS13],
            &provider,
        );
        server_config.alpn_protocols = vec!["foo".into()];
        let server_config = Arc::new(server_config);

        let mut server = quic::ServerConnection::new(
            server_config,
            quic::Version::V1,
            b"server params".to_vec(),
        )
        .unwrap();

        let buf = encoding::client_hello_with_extensions(vec![
            encoding::Extension::new_sig_algs(),
            encoding::Extension::new_dummy_key_share(),
            encoding::Extension::new_kx_groups(),
        ]);
        assert_eq!(
            server.read_hs(buf.as_slice()).err(),
            Some(Error::PeerIncompatible(
                PeerIncompatible::SupportedVersionsExtensionRequired
            )),
        );
    }

    #[test]
    fn packet_key_api() {
        use cipher_suite::TLS13_AES_128_GCM_SHA256;
        use rustls::Side;
        use rustls::quic::{Keys, Version};

        // Test vectors: https://www.rfc-editor.org/rfc/rfc9001.html#name-client-initial
        const CONNECTION_ID: &[u8] = &[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];
        const PACKET_NUMBER: u64 = 2;
        const PLAIN_HEADER: &[u8] = &[
            0xc3, 0x00, 0x00, 0x00, 0x01, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08,
            0x00, 0x00, 0x44, 0x9e, 0x00, 0x00, 0x00, 0x02,
        ];

        const PAYLOAD: &[u8] = &[
            0x06, 0x00, 0x40, 0xf1, 0x01, 0x00, 0x00, 0xed, 0x03, 0x03, 0xeb, 0xf8, 0xfa, 0x56,
            0xf1, 0x29, 0x39, 0xb9, 0x58, 0x4a, 0x38, 0x96, 0x47, 0x2e, 0xc4, 0x0b, 0xb8, 0x63,
            0xcf, 0xd3, 0xe8, 0x68, 0x04, 0xfe, 0x3a, 0x47, 0xf0, 0x6a, 0x2b, 0x69, 0x48, 0x4c,
            0x00, 0x00, 0x04, 0x13, 0x01, 0x13, 0x02, 0x01, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x0e, 0x00, 0x00, 0x0b, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e,
            0x63, 0x6f, 0x6d, 0xff, 0x01, 0x00, 0x01, 0x00, 0x00, 0x0a, 0x00, 0x08, 0x00, 0x06,
            0x00, 0x1d, 0x00, 0x17, 0x00, 0x18, 0x00, 0x10, 0x00, 0x07, 0x00, 0x05, 0x04, 0x61,
            0x6c, 0x70, 0x6e, 0x00, 0x05, 0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33,
            0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20, 0x93, 0x70, 0xb2, 0xc9, 0xca, 0xa4,
            0x7f, 0xba, 0xba, 0xf4, 0x55, 0x9f, 0xed, 0xba, 0x75, 0x3d, 0xe1, 0x71, 0xfa, 0x71,
            0xf5, 0x0f, 0x1c, 0xe1, 0x5d, 0x43, 0xe9, 0x94, 0xec, 0x74, 0xd7, 0x48, 0x00, 0x2b,
            0x00, 0x03, 0x02, 0x03, 0x04, 0x00, 0x0d, 0x00, 0x10, 0x00, 0x0e, 0x04, 0x03, 0x05,
            0x03, 0x06, 0x03, 0x02, 0x03, 0x08, 0x04, 0x08, 0x05, 0x08, 0x06, 0x00, 0x2d, 0x00,
            0x02, 0x01, 0x01, 0x00, 0x1c, 0x00, 0x02, 0x40, 0x01, 0x00, 0x39, 0x00, 0x32, 0x04,
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x05, 0x04, 0x80, 0x00, 0xff,
            0xff, 0x07, 0x04, 0x80, 0x00, 0xff, 0xff, 0x08, 0x01, 0x10, 0x01, 0x04, 0x80, 0x00,
            0x75, 0x30, 0x09, 0x01, 0x10, 0x0f, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57,
            0x08, 0x06, 0x04, 0x80, 0x00, 0xff, 0xff,
        ];

        let client_keys = Keys::initial(
            Version::V1,
            TLS13_AES_128_GCM_SHA256
                .tls13()
                .unwrap(),
            TLS13_AES_128_GCM_SHA256
                .tls13()
                .unwrap()
                .quic
                .unwrap(),
            CONNECTION_ID,
            Side::Client,
        );
        assert_eq!(client_keys.local.packet.tag_len(), 16);

        let mut buf = Vec::new();
        buf.extend(PLAIN_HEADER);
        buf.extend(PAYLOAD);
        let header_len = PLAIN_HEADER.len();
        let tag_len = client_keys.local.packet.tag_len();
        let padding_len = 1200 - header_len - PAYLOAD.len() - tag_len;
        buf.extend(std::iter::repeat(0).take(padding_len));
        let (header, payload) = buf.split_at_mut(header_len);
        let tag = client_keys
            .local
            .packet
            .encrypt_in_place(PACKET_NUMBER, header, payload)
            .unwrap();

        let sample_len = client_keys.local.header.sample_len();
        let sample = &payload[..sample_len];
        let (first, rest) = header.split_at_mut(1);
        client_keys
            .local
            .header
            .encrypt_in_place(sample, &mut first[0], &mut rest[17..21])
            .unwrap();
        buf.extend_from_slice(tag.as_ref());

        const PROTECTED: &[u8] = &[
            0xc0, 0x00, 0x00, 0x00, 0x01, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08,
            0x00, 0x00, 0x44, 0x9e, 0x7b, 0x9a, 0xec, 0x34, 0xd1, 0xb1, 0xc9, 0x8d, 0xd7, 0x68,
            0x9f, 0xb8, 0xec, 0x11, 0xd2, 0x42, 0xb1, 0x23, 0xdc, 0x9b, 0xd8, 0xba, 0xb9, 0x36,
            0xb4, 0x7d, 0x92, 0xec, 0x35, 0x6c, 0x0b, 0xab, 0x7d, 0xf5, 0x97, 0x6d, 0x27, 0xcd,
            0x44, 0x9f, 0x63, 0x30, 0x00, 0x99, 0xf3, 0x99, 0x1c, 0x26, 0x0e, 0xc4, 0xc6, 0x0d,
            0x17, 0xb3, 0x1f, 0x84, 0x29, 0x15, 0x7b, 0xb3, 0x5a, 0x12, 0x82, 0xa6, 0x43, 0xa8,
            0xd2, 0x26, 0x2c, 0xad, 0x67, 0x50, 0x0c, 0xad, 0xb8, 0xe7, 0x37, 0x8c, 0x8e, 0xb7,
            0x53, 0x9e, 0xc4, 0xd4, 0x90, 0x5f, 0xed, 0x1b, 0xee, 0x1f, 0xc8, 0xaa, 0xfb, 0xa1,
            0x7c, 0x75, 0x0e, 0x2c, 0x7a, 0xce, 0x01, 0xe6, 0x00, 0x5f, 0x80, 0xfc, 0xb7, 0xdf,
            0x62, 0x12, 0x30, 0xc8, 0x37, 0x11, 0xb3, 0x93, 0x43, 0xfa, 0x02, 0x8c, 0xea, 0x7f,
            0x7f, 0xb5, 0xff, 0x89, 0xea, 0xc2, 0x30, 0x82, 0x49, 0xa0, 0x22, 0x52, 0x15, 0x5e,
            0x23, 0x47, 0xb6, 0x3d, 0x58, 0xc5, 0x45, 0x7a, 0xfd, 0x84, 0xd0, 0x5d, 0xff, 0xfd,
            0xb2, 0x03, 0x92, 0x84, 0x4a, 0xe8, 0x12, 0x15, 0x46, 0x82, 0xe9, 0xcf, 0x01, 0x2f,
            0x90, 0x21, 0xa6, 0xf0, 0xbe, 0x17, 0xdd, 0xd0, 0xc2, 0x08, 0x4d, 0xce, 0x25, 0xff,
            0x9b, 0x06, 0xcd, 0xe5, 0x35, 0xd0, 0xf9, 0x20, 0xa2, 0xdb, 0x1b, 0xf3, 0x62, 0xc2,
            0x3e, 0x59, 0x6d, 0x11, 0xa4, 0xf5, 0xa6, 0xcf, 0x39, 0x48, 0x83, 0x8a, 0x3a, 0xec,
            0x4e, 0x15, 0xda, 0xf8, 0x50, 0x0a, 0x6e, 0xf6, 0x9e, 0xc4, 0xe3, 0xfe, 0xb6, 0xb1,
            0xd9, 0x8e, 0x61, 0x0a, 0xc8, 0xb7, 0xec, 0x3f, 0xaf, 0x6a, 0xd7, 0x60, 0xb7, 0xba,
            0xd1, 0xdb, 0x4b, 0xa3, 0x48, 0x5e, 0x8a, 0x94, 0xdc, 0x25, 0x0a, 0xe3, 0xfd, 0xb4,
            0x1e, 0xd1, 0x5f, 0xb6, 0xa8, 0xe5, 0xeb, 0xa0, 0xfc, 0x3d, 0xd6, 0x0b, 0xc8, 0xe3,
            0x0c, 0x5c, 0x42, 0x87, 0xe5, 0x38, 0x05, 0xdb, 0x05, 0x9a, 0xe0, 0x64, 0x8d, 0xb2,
            0xf6, 0x42, 0x64, 0xed, 0x5e, 0x39, 0xbe, 0x2e, 0x20, 0xd8, 0x2d, 0xf5, 0x66, 0xda,
            0x8d, 0xd5, 0x99, 0x8c, 0xca, 0xbd, 0xae, 0x05, 0x30, 0x60, 0xae, 0x6c, 0x7b, 0x43,
            0x78, 0xe8, 0x46, 0xd2, 0x9f, 0x37, 0xed, 0x7b, 0x4e, 0xa9, 0xec, 0x5d, 0x82, 0xe7,
            0x96, 0x1b, 0x7f, 0x25, 0xa9, 0x32, 0x38, 0x51, 0xf6, 0x81, 0xd5, 0x82, 0x36, 0x3a,
            0xa5, 0xf8, 0x99, 0x37, 0xf5, 0xa6, 0x72, 0x58, 0xbf, 0x63, 0xad, 0x6f, 0x1a, 0x0b,
            0x1d, 0x96, 0xdb, 0xd4, 0xfa, 0xdd, 0xfc, 0xef, 0xc5, 0x26, 0x6b, 0xa6, 0x61, 0x17,
            0x22, 0x39, 0x5c, 0x90, 0x65, 0x56, 0xbe, 0x52, 0xaf, 0xe3, 0xf5, 0x65, 0x63, 0x6a,
            0xd1, 0xb1, 0x7d, 0x50, 0x8b, 0x73, 0xd8, 0x74, 0x3e, 0xeb, 0x52, 0x4b, 0xe2, 0x2b,
            0x3d, 0xcb, 0xc2, 0xc7, 0x46, 0x8d, 0x54, 0x11, 0x9c, 0x74, 0x68, 0x44, 0x9a, 0x13,
            0xd8, 0xe3, 0xb9, 0x58, 0x11, 0xa1, 0x98, 0xf3, 0x49, 0x1d, 0xe3, 0xe7, 0xfe, 0x94,
            0x2b, 0x33, 0x04, 0x07, 0xab, 0xf8, 0x2a, 0x4e, 0xd7, 0xc1, 0xb3, 0x11, 0x66, 0x3a,
            0xc6, 0x98, 0x90, 0xf4, 0x15, 0x70, 0x15, 0x85, 0x3d, 0x91, 0xe9, 0x23, 0x03, 0x7c,
            0x22, 0x7a, 0x33, 0xcd, 0xd5, 0xec, 0x28, 0x1c, 0xa3, 0xf7, 0x9c, 0x44, 0x54, 0x6b,
            0x9d, 0x90, 0xca, 0x00, 0xf0, 0x64, 0xc9, 0x9e, 0x3d, 0xd9, 0x79, 0x11, 0xd3, 0x9f,
            0xe9, 0xc5, 0xd0, 0xb2, 0x3a, 0x22, 0x9a, 0x23, 0x4c, 0xb3, 0x61, 0x86, 0xc4, 0x81,
            0x9e, 0x8b, 0x9c, 0x59, 0x27, 0x72, 0x66, 0x32, 0x29, 0x1d, 0x6a, 0x41, 0x82, 0x11,
            0xcc, 0x29, 0x62, 0xe2, 0x0f, 0xe4, 0x7f, 0xeb, 0x3e, 0xdf, 0x33, 0x0f, 0x2c, 0x60,
            0x3a, 0x9d, 0x48, 0xc0, 0xfc, 0xb5, 0x69, 0x9d, 0xbf, 0xe5, 0x89, 0x64, 0x25, 0xc5,
            0xba, 0xc4, 0xae, 0xe8, 0x2e, 0x57, 0xa8, 0x5a, 0xaf, 0x4e, 0x25, 0x13, 0xe4, 0xf0,
            0x57, 0x96, 0xb0, 0x7b, 0xa2, 0xee, 0x47, 0xd8, 0x05, 0x06, 0xf8, 0xd2, 0xc2, 0x5e,
            0x50, 0xfd, 0x14, 0xde, 0x71, 0xe6, 0xc4, 0x18, 0x55, 0x93, 0x02, 0xf9, 0x39, 0xb0,
            0xe1, 0xab, 0xd5, 0x76, 0xf2, 0x79, 0xc4, 0xb2, 0xe0, 0xfe, 0xb8, 0x5c, 0x1f, 0x28,
            0xff, 0x18, 0xf5, 0x88, 0x91, 0xff, 0xef, 0x13, 0x2e, 0xef, 0x2f, 0xa0, 0x93, 0x46,
            0xae, 0xe3, 0x3c, 0x28, 0xeb, 0x13, 0x0f, 0xf2, 0x8f, 0x5b, 0x76, 0x69, 0x53, 0x33,
            0x41, 0x13, 0x21, 0x19, 0x96, 0xd2, 0x00, 0x11, 0xa1, 0x98, 0xe3, 0xfc, 0x43, 0x3f,
            0x9f, 0x25, 0x41, 0x01, 0x0a, 0xe1, 0x7c, 0x1b, 0xf2, 0x02, 0x58, 0x0f, 0x60, 0x47,
            0x47, 0x2f, 0xb3, 0x68, 0x57, 0xfe, 0x84, 0x3b, 0x19, 0xf5, 0x98, 0x40, 0x09, 0xdd,
            0xc3, 0x24, 0x04, 0x4e, 0x84, 0x7a, 0x4f, 0x4a, 0x0a, 0xb3, 0x4f, 0x71, 0x95, 0x95,
            0xde, 0x37, 0x25, 0x2d, 0x62, 0x35, 0x36, 0x5e, 0x9b, 0x84, 0x39, 0x2b, 0x06, 0x10,
            0x85, 0x34, 0x9d, 0x73, 0x20, 0x3a, 0x4a, 0x13, 0xe9, 0x6f, 0x54, 0x32, 0xec, 0x0f,
            0xd4, 0xa1, 0xee, 0x65, 0xac, 0xcd, 0xd5, 0xe3, 0x90, 0x4d, 0xf5, 0x4c, 0x1d, 0xa5,
            0x10, 0xb0, 0xff, 0x20, 0xdc, 0xc0, 0xc7, 0x7f, 0xcb, 0x2c, 0x0e, 0x0e, 0xb6, 0x05,
            0xcb, 0x05, 0x04, 0xdb, 0x87, 0x63, 0x2c, 0xf3, 0xd8, 0xb4, 0xda, 0xe6, 0xe7, 0x05,
            0x76, 0x9d, 0x1d, 0xe3, 0x54, 0x27, 0x01, 0x23, 0xcb, 0x11, 0x45, 0x0e, 0xfc, 0x60,
            0xac, 0x47, 0x68, 0x3d, 0x7b, 0x8d, 0x0f, 0x81, 0x13, 0x65, 0x56, 0x5f, 0xd9, 0x8c,
            0x4c, 0x8e, 0xb9, 0x36, 0xbc, 0xab, 0x8d, 0x06, 0x9f, 0xc3, 0x3b, 0xd8, 0x01, 0xb0,
            0x3a, 0xde, 0xa2, 0xe1, 0xfb, 0xc5, 0xaa, 0x46, 0x3d, 0x08, 0xca, 0x19, 0x89, 0x6d,
            0x2b, 0xf5, 0x9a, 0x07, 0x1b, 0x85, 0x1e, 0x6c, 0x23, 0x90, 0x52, 0x17, 0x2f, 0x29,
            0x6b, 0xfb, 0x5e, 0x72, 0x40, 0x47, 0x90, 0xa2, 0x18, 0x10, 0x14, 0xf3, 0xb9, 0x4a,
            0x4e, 0x97, 0xd1, 0x17, 0xb4, 0x38, 0x13, 0x03, 0x68, 0xcc, 0x39, 0xdb, 0xb2, 0xd1,
            0x98, 0x06, 0x5a, 0xe3, 0x98, 0x65, 0x47, 0x92, 0x6c, 0xd2, 0x16, 0x2f, 0x40, 0xa2,
            0x9f, 0x0c, 0x3c, 0x87, 0x45, 0xc0, 0xf5, 0x0f, 0xba, 0x38, 0x52, 0xe5, 0x66, 0xd4,
            0x45, 0x75, 0xc2, 0x9d, 0x39, 0xa0, 0x3f, 0x0c, 0xda, 0x72, 0x19, 0x84, 0xb6, 0xf4,
            0x40, 0x59, 0x1f, 0x35, 0x5e, 0x12, 0xd4, 0x39, 0xff, 0x15, 0x0a, 0xab, 0x76, 0x13,
            0x49, 0x9d, 0xbd, 0x49, 0xad, 0xab, 0xc8, 0x67, 0x6e, 0xef, 0x02, 0x3b, 0x15, 0xb6,
            0x5b, 0xfc, 0x5c, 0xa0, 0x69, 0x48, 0x10, 0x9f, 0x23, 0xf3, 0x50, 0xdb, 0x82, 0x12,
            0x35, 0x35, 0xeb, 0x8a, 0x74, 0x33, 0xbd, 0xab, 0xcb, 0x90, 0x92, 0x71, 0xa6, 0xec,
            0xbc, 0xb5, 0x8b, 0x93, 0x6a, 0x88, 0xcd, 0x4e, 0x8f, 0x2e, 0x6f, 0xf5, 0x80, 0x01,
            0x75, 0xf1, 0x13, 0x25, 0x3d, 0x8f, 0xa9, 0xca, 0x88, 0x85, 0xc2, 0xf5, 0x52, 0xe6,
            0x57, 0xdc, 0x60, 0x3f, 0x25, 0x2e, 0x1a, 0x8e, 0x30, 0x8f, 0x76, 0xf0, 0xbe, 0x79,
            0xe2, 0xfb, 0x8f, 0x5d, 0x5f, 0xbb, 0xe2, 0xe3, 0x0e, 0xca, 0xdd, 0x22, 0x07, 0x23,
            0xc8, 0xc0, 0xae, 0xa8, 0x07, 0x8c, 0xdf, 0xcb, 0x38, 0x68, 0x26, 0x3f, 0xf8, 0xf0,
            0x94, 0x00, 0x54, 0xda, 0x48, 0x78, 0x18, 0x93, 0xa7, 0xe4, 0x9a, 0xd5, 0xaf, 0xf4,
            0xaf, 0x30, 0x0c, 0xd8, 0x04, 0xa6, 0xb6, 0x27, 0x9a, 0xb3, 0xff, 0x3a, 0xfb, 0x64,
            0x49, 0x1c, 0x85, 0x19, 0x4a, 0xab, 0x76, 0x0d, 0x58, 0xa6, 0x06, 0x65, 0x4f, 0x9f,
            0x44, 0x00, 0xe8, 0xb3, 0x85, 0x91, 0x35, 0x6f, 0xbf, 0x64, 0x25, 0xac, 0xa2, 0x6d,
            0xc8, 0x52, 0x44, 0x25, 0x9f, 0xf2, 0xb1, 0x9c, 0x41, 0xb9, 0xf9, 0x6f, 0x3c, 0xa9,
            0xec, 0x1d, 0xde, 0x43, 0x4d, 0xa7, 0xd2, 0xd3, 0x92, 0xb9, 0x05, 0xdd, 0xf3, 0xd1,
            0xf9, 0xaf, 0x93, 0xd1, 0xaf, 0x59, 0x50, 0xbd, 0x49, 0x3f, 0x5a, 0xa7, 0x31, 0xb4,
            0x05, 0x6d, 0xf3, 0x1b, 0xd2, 0x67, 0xb6, 0xb9, 0x0a, 0x07, 0x98, 0x31, 0xaa, 0xf5,
            0x79, 0xbe, 0x0a, 0x39, 0x01, 0x31, 0x37, 0xaa, 0xc6, 0xd4, 0x04, 0xf5, 0x18, 0xcf,
            0xd4, 0x68, 0x40, 0x64, 0x7e, 0x78, 0xbf, 0xe7, 0x06, 0xca, 0x4c, 0xf5, 0xe9, 0xc5,
            0x45, 0x3e, 0x9f, 0x7c, 0xfd, 0x2b, 0x8b, 0x4c, 0x8d, 0x16, 0x9a, 0x44, 0xe5, 0x5c,
            0x88, 0xd4, 0xa9, 0xa7, 0xf9, 0x47, 0x42, 0x41, 0xe2, 0x21, 0xaf, 0x44, 0x86, 0x00,
            0x18, 0xab, 0x08, 0x56, 0x97, 0x2e, 0x19, 0x4c, 0xd9, 0x34,
        ];

        assert_eq!(&buf, PROTECTED);

        let (header, payload) = buf.split_at_mut(header_len);
        let (first, rest) = header.split_at_mut(1);
        let sample = &payload[..sample_len];

        let server_keys = Keys::initial(
            Version::V1,
            TLS13_AES_128_GCM_SHA256
                .tls13()
                .unwrap(),
            TLS13_AES_128_GCM_SHA256
                .tls13()
                .unwrap()
                .quic
                .unwrap(),
            CONNECTION_ID,
            Side::Server,
        );
        server_keys
            .remote
            .header
            .decrypt_in_place(sample, &mut first[0], &mut rest[17..21])
            .unwrap();
        let payload = server_keys
            .remote
            .packet
            .decrypt_in_place(PACKET_NUMBER, header, payload)
            .unwrap();

        assert_eq!(&payload[..PAYLOAD.len()], PAYLOAD);
        assert_eq!(payload.len(), buf.len() - header_len - tag_len);
    }

    #[test]
    fn test_quic_exporter() {
        let provider = provider::default_provider();
        for &kt in KeyType::all_for_provider(&provider) {
            let client_config =
                make_client_config_with_versions(kt, &[&rustls::version::TLS13], &provider);
            let server_config =
                make_server_config_with_versions(kt, &[&rustls::version::TLS13], &provider);

            do_exporter_test(client_config, server_config);
        }
    }

    #[test]
    fn test_fragmented_append() {
        // Create a QUIC client connection.
        let client_config = make_client_config_with_versions(
            KeyType::Rsa2048,
            &[&rustls::version::TLS13],
            &provider::default_provider(),
        );
        let client_config = Arc::new(client_config);
        let mut client = quic::ClientConnection::new(
            client_config.clone(),
            quic::Version::V1,
            server_name("localhost"),
            b"client params"[..].into(),
        )
        .unwrap();

        // Construct a message that is too large to fit in a single QUIC packet.
        // We want the partial pieces to be large enough to overflow the deframer's
        // 4096 byte buffer if mishandled.
        let mut out = vec![0; 4096];
        let len_bytes = u32::to_be_bytes(9266_u32);
        out[1..4].copy_from_slice(&len_bytes[1..]);

        // Read the message - this will put us into a joining handshake message state, buffering
        // 4096 bytes into the deframer buffer.
        client.read_hs(&out).unwrap();

        // Read the message again - once more it isn't a complete message, so we'll try to
        // append another 4096 bytes into the deframer buffer.
        //
        // If the deframer mishandles writing into the used buffer space this will panic with
        // an index out of range error:
        //   range end index 8192 out of range for slice of length 4096
        client.read_hs(&out).unwrap();
    }
} // mod test_quic

#[test]
fn test_client_config_keyshare() {
    let provider = provider::default_provider();
    let kx_groups = vec![provider::kx_group::SECP384R1];
    let client_config =
        make_client_config_with_kx_groups(KeyType::Rsa2048, kx_groups.clone(), &provider);
    let server_config = make_server_config_with_kx_groups(KeyType::Rsa2048, kx_groups, &provider);
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake_until_error(&mut client, &mut server).unwrap();
}

#[test]
fn test_client_config_keyshare_mismatch() {
    let provider = provider::default_provider();
    let client_config = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::SECP384R1],
        &provider,
    );
    let server_config = make_server_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::X25519],
        &provider,
    );
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    assert!(do_handshake_until_error(&mut client, &mut server).is_err());
}

#[test]
fn exercise_all_key_exchange_methods() {
    for version in rustls::ALL_VERSIONS {
        for kx_group in provider::ALL_KX_GROUPS {
            if !kx_group
                .name()
                .usable_for_version(version.version())
            {
                continue;
            }

            let provider = provider::default_provider();
            let client_config =
                make_client_config_with_kx_groups(KeyType::Rsa2048, vec![*kx_group], &provider);
            let server_config =
                make_server_config_with_kx_groups(KeyType::Rsa2048, vec![*kx_group], &provider);
            let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
            assert!(do_handshake_until_error(&mut client, &mut server).is_ok());
            println!("kx_group {:?} is self-consistent", kx_group.name());
        }
    }
}

#[test]
fn test_client_sends_helloretryrequest() {
    let provider = provider::default_provider();
    // client sends a secp384r1 key share
    let mut client_config = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::SECP384R1, provider::kx_group::X25519],
        &provider,
    );

    let storage = Arc::new(ClientStorage::new());
    client_config.resumption = Resumption::store(storage.clone());

    // but server only accepts x25519, so a HRR is required
    let server_config = make_server_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::X25519],
        &provider,
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(client.handshake_kind(), None);
    assert_eq!(server.handshake_kind(), None);

    // client sends hello
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200);
        assert_eq!(pipe.writevs.len(), 1);
        assert!(pipe.writevs[0].len() == 1);
    }

    assert_eq!(client.handshake_kind(), None);
    assert_eq!(
        server.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );

    // server sends HRR
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert!(wrlen < 100); // just the hello retry request
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 2); // hello retry request and CCS
    }

    assert_eq!(
        client.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );
    assert_eq!(
        server.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );

    // client sends fixed hello
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200); // just the client hello retry
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 2); // only a CCS & client hello retry
    }

    // server completes handshake
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.write_tls(&mut pipe).unwrap();
        assert!(wrlen > 200);
        assert_eq!(pipe.writevs.len(), 1);
        assert_eq!(pipe.writevs[0].len(), 2); // { server hello / encrypted exts / cert / cert-verify } / finished
    }

    assert_eq!(
        client.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );
    assert_eq!(
        server.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );

    do_handshake_until_error(&mut client, &mut server).unwrap();

    // client only did following storage queries:
    println!("storage {:#?}", storage.ops());
    assert_eq!(storage.ops().len(), 7);
    assert!(matches!(
        storage.ops()[0],
        ClientStorageOp::TakeTls13Ticket(_, false)
    ));
    assert!(matches!(
        storage.ops()[1],
        ClientStorageOp::GetTls12Session(_, false)
    ));
    assert!(matches!(
        storage.ops()[2],
        ClientStorageOp::GetKxHint(_, None)
    ));
    assert!(matches!(
        storage.ops()[3],
        ClientStorageOp::SetKxHint(_, rustls::NamedGroup::X25519)
    ));
    assert!(matches!(
        storage.ops()[4],
        ClientStorageOp::RemoveTls12Session(_)
    ));
    // server sends 2 tickets by default
    assert!(matches!(
        storage.ops()[5],
        ClientStorageOp::InsertTls13Ticket(_)
    ));
    assert!(matches!(
        storage.ops()[6],
        ClientStorageOp::InsertTls13Ticket(_)
    ));
}

#[test]
fn test_client_attempts_to_use_unsupported_kx_group() {
    // common to both client configs
    let shared_storage = Arc::new(ClientStorage::new());
    let provider = provider::default_provider();

    // first, client sends a secp-256 share and server agrees. secp-256 is inserted
    //   into kx group cache.
    let mut client_config_1 = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::SECP256R1],
        &provider,
    );
    client_config_1.resumption = Resumption::store(shared_storage.clone());

    // second, client only supports secp-384 and so kx group cache
    //   contains an unusable value.
    let mut client_config_2 = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::SECP384R1],
        &provider,
    );
    client_config_2.resumption = Resumption::store(shared_storage.clone());

    let server_config = make_server_config(KeyType::Rsa2048, &provider);

    // first handshake
    let (mut client_1, mut server) = make_pair_for_configs(client_config_1, server_config.clone());
    do_handshake_until_error(&mut client_1, &mut server).unwrap();

    let ops = shared_storage.ops();
    println!("storage {ops:#?}");
    assert_eq!(ops.len(), 7);
    assert!(matches!(
        ops[3],
        ClientStorageOp::SetKxHint(_, rustls::NamedGroup::secp256r1)
    ));

    // second handshake
    let (mut client_2, mut server) = make_pair_for_configs(client_config_2, server_config);
    do_handshake_until_error(&mut client_2, &mut server).unwrap();

    let ops = shared_storage.ops();
    println!("storage {:?} {:#?}", ops.len(), ops);
    assert_eq!(ops.len(), 13);
    assert!(matches!(ops[7], ClientStorageOp::TakeTls13Ticket(_, true)));
    assert!(matches!(
        ops[8],
        ClientStorageOp::GetKxHint(_, Some(rustls::NamedGroup::secp256r1))
    ));
    assert!(matches!(
        ops[9],
        ClientStorageOp::SetKxHint(_, rustls::NamedGroup::secp384r1)
    ));
}

#[test]
fn test_client_sends_share_for_less_preferred_group() {
    // this is a test for the case described in:
    // https://datatracker.ietf.org/doc/draft-davidben-tls-key-share-prediction/

    // common to both client configs
    let shared_storage = Arc::new(ClientStorage::new());
    let provider = provider::default_provider();

    // first, client sends a secp384r1 share and server agrees. secp384r1 is inserted
    //   into kx group cache.
    let mut client_config_1 = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::SECP384R1],
        &provider,
    );
    client_config_1.resumption = Resumption::store(shared_storage.clone());

    // second, client supports (x25519, secp384r1) and so kx group cache
    //   contains a supported but less-preferred group.
    let mut client_config_2 = make_client_config_with_kx_groups(
        KeyType::Rsa2048,
        vec![provider::kx_group::X25519, provider::kx_group::SECP384R1],
        &provider,
    );
    client_config_2.resumption = Resumption::store(shared_storage.clone());

    let server_config = make_server_config_with_kx_groups(
        KeyType::Rsa2048,
        provider::ALL_KX_GROUPS.to_vec(),
        &provider,
    );

    // first handshake
    let (mut client_1, mut server) = make_pair_for_configs(client_config_1, server_config.clone());
    do_handshake_until_error(&mut client_1, &mut server).unwrap();
    assert_eq!(
        client_1
            .negotiated_key_exchange_group()
            .map(|kxg| kxg.name()),
        Some(NamedGroup::secp384r1)
    );
    assert_eq!(client_1.handshake_kind(), Some(HandshakeKind::Full));

    let ops = shared_storage.ops();
    println!("storage {ops:#?}");
    assert_eq!(ops.len(), 7);
    assert!(matches!(
        ops[3],
        ClientStorageOp::SetKxHint(_, rustls::NamedGroup::secp384r1)
    ));

    // second handshake; HRR'd from secp384r1 to X25519
    let (mut client_2, mut server) = make_pair_for_configs(client_config_2, server_config);
    do_handshake(&mut client_2, &mut server);
    assert_eq!(
        client_2
            .negotiated_key_exchange_group()
            .map(|kxg| kxg.name()),
        Some(NamedGroup::X25519)
    );
    assert_eq!(
        client_2.handshake_kind(),
        Some(HandshakeKind::FullWithHelloRetryRequest)
    );
}

#[test]
fn test_tls13_client_resumption_does_not_reuse_tickets() {
    let shared_storage = Arc::new(ClientStorage::new());
    let provider = provider::default_provider();

    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.resumption = Resumption::store(shared_storage.clone());
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.send_tls13_tickets = 5;
    let server_config = Arc::new(server_config);

    // first handshake: client obtains 5 tickets from server.
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake_until_error(&mut client, &mut server).unwrap();

    let ops = shared_storage.ops_and_reset();
    println!("storage {ops:#?}");
    assert_eq!(ops.len(), 10);
    assert!(matches!(ops[5], ClientStorageOp::InsertTls13Ticket(_)));
    assert!(matches!(ops[6], ClientStorageOp::InsertTls13Ticket(_)));
    assert!(matches!(ops[7], ClientStorageOp::InsertTls13Ticket(_)));
    assert!(matches!(ops[8], ClientStorageOp::InsertTls13Ticket(_)));
    assert!(matches!(ops[9], ClientStorageOp::InsertTls13Ticket(_)));

    // 5 subsequent handshakes: all are resumptions

    // Note: we don't do complete the handshakes, because that means
    // we get five additional tickets per connection which is unhelpful
    // in this test.  It also acts to record a "Happy Eyeballs"-type use
    // case, where a client speculatively makes many connection attempts
    // in parallel without knowledge of which will work due to underlying
    // connectivity uncertainty.
    for _ in 0..5 {
        let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();

        let ops = shared_storage.ops_and_reset();
        assert!(matches!(ops[0], ClientStorageOp::TakeTls13Ticket(_, true)));
    }

    // 6th subsequent handshake: cannot be resumed; we ran out of tickets
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let ops = shared_storage.ops_and_reset();
    println!("last {ops:?}");
    assert!(matches!(ops[0], ClientStorageOp::TakeTls13Ticket(_, false)));
}

#[test]
fn test_client_mtu_reduction() {
    struct CollectWrites {
        writevs: Vec<Vec<usize>>,
    }

    impl io::Write for CollectWrites {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!()
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!()
        }
        fn write_vectored(&mut self, b: &[io::IoSlice<'_>]) -> io::Result<usize> {
            let writes = b
                .iter()
                .map(|slice| slice.len())
                .collect::<Vec<usize>>();
            let len = writes.iter().sum();
            self.writevs.push(writes);
            Ok(len)
        }
    }

    fn collect_write_lengths(client: &mut ClientConnection) -> Vec<usize> {
        let mut collector = CollectWrites { writevs: vec![] };

        client
            .write_tls(&mut collector)
            .unwrap();
        assert_eq!(collector.writevs.len(), 1);
        collector.writevs[0].clone()
    }

    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let mut client_config = make_client_config(*kt, &provider);
        client_config.max_fragment_size = Some(64);
        let mut client =
            ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();
        let writes = collect_write_lengths(&mut client);
        println!("writes at mtu=64: {writes:?}");
        assert!(writes.iter().all(|x| *x <= 64));
        assert!(writes.len() > 1);
    }
}

#[test]
fn test_server_mtu_reduction() {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.max_fragment_size = Some(64);
    server_config.send_half_rtt_data = true;
    let (mut client, mut server) = make_pair_for_configs(
        make_client_config(KeyType::Rsa2048, &provider),
        server_config,
    );

    let big_data = [0u8; 2048];
    server
        .writer()
        .write_all(&big_data)
        .unwrap();

    let encryption_overhead = 20; // FIXME: see issue #991

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        server.write_tls(&mut pipe).unwrap();

        assert_eq!(pipe.writevs.len(), 1);
        assert!(
            pipe.writevs[0]
                .iter()
                .all(|x| *x <= 64 + encryption_overhead)
        );
    }

    client.process_new_packets().unwrap();
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        server.write_tls(&mut pipe).unwrap();
        assert_eq!(pipe.writevs.len(), 1);
        assert!(
            pipe.writevs[0]
                .iter()
                .all(|x| *x <= 64 + encryption_overhead)
        );
    }

    client.process_new_packets().unwrap();
    check_read(&mut client.reader(), &big_data);
}

fn check_client_max_fragment_size(size: usize) -> Option<Error> {
    let provider = provider::default_provider();
    let mut client_config = make_client_config(KeyType::Ed25519, &provider);
    client_config.max_fragment_size = Some(size);
    ClientConnection::new(Arc::new(client_config), server_name("localhost")).err()
}

#[test]
fn bad_client_max_fragment_sizes() {
    assert_eq!(
        check_client_max_fragment_size(31),
        Some(Error::BadMaxFragmentSize)
    );
    assert_eq!(check_client_max_fragment_size(32), None);
    assert_eq!(check_client_max_fragment_size(64), None);
    assert_eq!(check_client_max_fragment_size(1460), None);
    assert_eq!(check_client_max_fragment_size(0x4000), None);
    assert_eq!(check_client_max_fragment_size(0x4005), None);
    assert_eq!(
        check_client_max_fragment_size(0x4006),
        Some(Error::BadMaxFragmentSize)
    );
    assert_eq!(
        check_client_max_fragment_size(0xffff),
        Some(Error::BadMaxFragmentSize)
    );
}

#[test]
fn handshakes_complete_and_data_flows_with_gratuitious_max_fragment_sizes() {
    // general exercising of msgs::fragmenter and msgs::deframer
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        for version in rustls::ALL_VERSIONS {
            // no hidden significance to these numbers
            for frag_size in [37, 61, 101, 257] {
                println!("test kt={kt:?} version={version:?} frag={frag_size:?}");
                let mut client_config =
                    make_client_config_with_versions(*kt, &[version], &provider);
                client_config.max_fragment_size = Some(frag_size);
                let mut server_config = make_server_config(*kt, &provider);
                server_config.max_fragment_size = Some(frag_size);

                let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
                do_handshake(&mut client, &mut server);

                // check server -> client data flow
                let pattern = (0x00..=0xffu8).collect::<Vec<u8>>();
                assert_eq!(pattern.len(), server.writer().write(&pattern).unwrap());
                transfer(&mut server, &mut client);
                client.process_new_packets().unwrap();
                check_read(&mut client.reader(), &pattern);

                // and client -> server
                assert_eq!(pattern.len(), client.writer().write(&pattern).unwrap());
                transfer(&mut client, &mut server);
                server.process_new_packets().unwrap();
                check_read(&mut server.reader(), &pattern);
            }
        }
    }
}

fn assert_lt(left: usize, right: usize) {
    if left >= right {
        panic!("expected {left} < {right}");
    }
}

#[test]
fn connection_types_are_not_huge() {
    // Arbitrary sizes
    assert_lt(mem::size_of::<ServerConnection>(), 1600);
    assert_lt(mem::size_of::<ClientConnection>(), 1600);
    assert_lt(
        mem::size_of::<rustls::server::UnbufferedServerConnection>(),
        1600,
    );
    assert_lt(
        mem::size_of::<rustls::client::UnbufferedClientConnection>(),
        1600,
    );
}

#[test]
fn test_server_rejects_clients_without_any_kx_groups() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    server
        .read_tls(
            &mut encoding::message_framing(
                ContentType::Handshake,
                ProtocolVersion::TLSv1_2,
                encoding::client_hello_with_extensions(vec![
                    encoding::Extension::new_sig_algs(),
                    encoding::Extension {
                        typ: ExtensionType::EllipticCurves,
                        body: encoding::len_u16(vec![]),
                    },
                    encoding::Extension {
                        typ: ExtensionType::KeyShare,
                        body: encoding::len_u16(vec![]),
                    },
                ]),
            )
            .as_slice(),
        )
        .unwrap();
    assert_eq!(
        server.process_new_packets(),
        Err(Error::InvalidMessage(InvalidMessage::IllegalEmptyList(
            "NamedGroups"
        )))
    );
}

#[test]
fn test_server_rejects_clients_without_any_kx_group_overlap() {
    for version in rustls::ALL_VERSIONS {
        let (mut client, mut server) = make_pair_for_configs(
            make_client_config_with_kx_groups(
                KeyType::Rsa2048,
                vec![provider::kx_group::X25519],
                &provider::default_provider(),
            ),
            finish_server_config(
                KeyType::Rsa2048,
                ServerConfig::builder_with_provider(
                    CryptoProvider {
                        kx_groups: vec![provider::kx_group::SECP384R1],
                        ..provider::default_provider()
                    }
                    .into(),
                )
                .with_protocol_versions(&[version])
                .unwrap(),
            ),
        );
        transfer(&mut client, &mut server);
        assert_eq!(
            server.process_new_packets(),
            Err(Error::PeerIncompatible(
                PeerIncompatible::NoKxGroupsInCommon
            ))
        );
        transfer(&mut server, &mut client);
        assert_eq!(
            client.process_new_packets(),
            Err(Error::AlertReceived(AlertDescription::HandshakeFailure))
        );
    }
}

#[test]
fn test_client_rejects_illegal_tls13_ccs() {
    fn corrupt_ccs(msg: &mut Message) -> Altered {
        if let MessagePayload::ChangeCipherSpec(_) = &mut msg.payload {
            println!("seen CCS {msg:?}");
            return Altered::Raw(encoding::message_framing(
                ContentType::ChangeCipherSpec,
                ProtocolVersion::TLSv1_2,
                vec![0x01, 0x02],
            ));
        }
        Altered::InPlace
    }

    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let (mut server, mut client) = (server.into(), client.into());

    transfer_altered(&mut server, corrupt_ccs, &mut client);
    assert_eq!(
        client.process_new_packets(),
        Err(Error::PeerMisbehaved(
            PeerMisbehaved::IllegalMiddleboxChangeCipherSpec
        ))
    );
}

/// https://github.com/rustls/rustls/issues/797
#[test]
fn test_client_tls12_no_resume_after_server_downgrade() {
    let provider = provider::default_provider();
    let mut client_config = common::make_client_config(KeyType::Ed25519, &provider);
    let client_storage = Arc::new(ClientStorage::new());
    client_config.resumption = Resumption::store(client_storage.clone());
    let client_config = Arc::new(client_config);

    let server_config_1 = Arc::new(common::finish_server_config(
        KeyType::Ed25519,
        server_config_builder_with_versions(&[&rustls::version::TLS13], &provider),
    ));

    let mut server_config_2 = common::finish_server_config(
        KeyType::Ed25519,
        server_config_builder_with_versions(&[&rustls::version::TLS12], &provider),
    );
    server_config_2.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});

    dbg!("handshake 1");
    let mut client_1 =
        ClientConnection::new(client_config.clone(), "localhost".try_into().unwrap()).unwrap();
    let mut server_1 = ServerConnection::new(server_config_1).unwrap();
    common::do_handshake(&mut client_1, &mut server_1);

    assert_eq!(client_storage.ops().len(), 7);
    println!("hs1 storage ops: {:#?}", client_storage.ops());
    assert!(matches!(
        client_storage.ops()[3],
        ClientStorageOp::SetKxHint(_, _)
    ));
    assert!(matches!(
        client_storage.ops()[4],
        ClientStorageOp::RemoveTls12Session(_)
    ));
    assert!(matches!(
        client_storage.ops()[5],
        ClientStorageOp::InsertTls13Ticket(_)
    ));

    dbg!("handshake 2");
    let mut client_2 =
        ClientConnection::new(client_config, "localhost".try_into().unwrap()).unwrap();
    let mut server_2 = ServerConnection::new(Arc::new(server_config_2)).unwrap();
    common::do_handshake(&mut client_2, &mut server_2);
    println!("hs2 storage ops: {:#?}", client_storage.ops());
    assert_eq!(client_storage.ops().len(), 9);

    // attempt consumes a TLS1.3 ticket
    assert!(matches!(
        client_storage.ops()[7],
        ClientStorageOp::TakeTls13Ticket(_, true)
    ));

    // but ends up with TLS1.2
    assert_eq!(
        client_2.protocol_version(),
        Some(rustls::ProtocolVersion::TLSv1_2)
    );
}

#[test]
fn test_acceptor() {
    use rustls::server::Acceptor;

    let provider = provider::default_provider();
    let client_config = Arc::new(make_client_config(KeyType::Ed25519, &provider));
    let mut client = ClientConnection::new(client_config, server_name("localhost")).unwrap();
    let mut buf = Vec::new();
    client.write_tls(&mut buf).unwrap();

    let server_config = Arc::new(make_server_config(KeyType::Ed25519, &provider));
    let mut acceptor = Acceptor::default();
    acceptor
        .read_tls(&mut buf.as_slice())
        .unwrap();
    let accepted = acceptor.accept().unwrap().unwrap();
    let ch = accepted.client_hello();
    assert_eq!(
        ch.server_name(),
        Some(&DnsName::try_from("localhost").unwrap())
    );
    assert_eq!(
        ch.named_groups().unwrap(),
        provider::default_provider()
            .kx_groups
            .iter()
            .map(|kx| kx.name())
            .collect::<Vec<NamedGroup>>()
    );

    let server = accepted
        .into_connection(server_config)
        .unwrap();
    assert!(server.wants_write());

    // Reusing an acceptor is not allowed
    assert_eq!(
        acceptor
            .read_tls(&mut [0u8].as_ref())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Other,
    );
    assert_eq!(
        acceptor.accept().err().unwrap().0,
        Error::General("Acceptor polled after completion".into())
    );

    let mut acceptor = Acceptor::default();
    assert!(acceptor.accept().unwrap().is_none());
    acceptor
        .read_tls(&mut &buf[..3])
        .unwrap(); // incomplete message
    assert!(acceptor.accept().unwrap().is_none());

    acceptor
        .read_tls(&mut [0x80, 0x00].as_ref())
        .unwrap(); // invalid message (len = 32k bytes)
    let (err, mut alert) = acceptor.accept().unwrap_err();
    assert_eq!(err, Error::InvalidMessage(InvalidMessage::MessageTooLarge));
    let mut alert_content = Vec::new();
    let _ = alert.write(&mut alert_content);
    let expected = build_alert(AlertLevel::Fatal, AlertDescription::DecodeError, &[]);
    assert_eq!(alert_content, expected);

    let mut acceptor = Acceptor::default();
    // Minimal valid 1-byte application data message is not a handshake message
    acceptor
        .read_tls(
            &mut encoding::message_framing(
                ContentType::ApplicationData,
                ProtocolVersion::TLSv1_2,
                vec![0x00],
            )
            .as_slice(),
        )
        .unwrap();
    let (err, mut alert) = acceptor.accept().unwrap_err();
    assert!(matches!(err, Error::InappropriateMessage { .. }));
    let mut alert_content = Vec::new();
    let _ = alert.write(&mut alert_content);
    assert!(alert_content.is_empty()); // We do not expect an alert for this condition.

    let mut acceptor = Acceptor::default();
    // Minimal 1-byte ClientHello message is not a legal handshake message
    acceptor
        .read_tls(
            &mut encoding::message_framing(
                ContentType::Handshake,
                ProtocolVersion::TLSv1_2,
                encoding::handshake_framing(HandshakeType::ClientHello, vec![0x00]),
            )
            .as_slice(),
        )
        .unwrap();
    let (err, mut alert) = acceptor.accept().unwrap_err();
    assert!(matches!(
        err,
        Error::InvalidMessage(InvalidMessage::MissingData(_))
    ));
    let mut alert_content = Vec::new();
    let _ = alert.write(&mut alert_content);
    let expected = build_alert(AlertLevel::Fatal, AlertDescription::DecodeError, &[]);
    assert_eq!(alert_content, expected);
}

#[test]
fn test_acceptor_rejected_handshake() {
    use rustls::server::Acceptor;

    let client_config = finish_client_config(
        KeyType::Ed25519,
        ClientConfig::builder_with_provider(provider::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap(),
    );
    let mut client = ClientConnection::new(client_config.into(), server_name("localhost")).unwrap();
    let mut buf = Vec::new();
    client.write_tls(&mut buf).unwrap();

    let server_config = finish_server_config(
        KeyType::Ed25519,
        ServerConfig::builder_with_provider(provider::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap(),
    );
    let mut acceptor = Acceptor::default();
    acceptor
        .read_tls(&mut buf.as_slice())
        .unwrap();
    let accepted = acceptor.accept().unwrap().unwrap();
    let ch = accepted.client_hello();
    assert_eq!(
        ch.server_name(),
        Some(&DnsName::try_from("localhost").unwrap())
    );

    let (err, mut alert) = accepted
        .into_connection(server_config.into())
        .unwrap_err();
    assert_eq!(
        err,
        Error::PeerIncompatible(PeerIncompatible::Tls12NotOfferedOrEnabled)
    );

    let mut alert_content = Vec::new();
    let _ = alert.write(&mut alert_content);
    let expected = build_alert(AlertLevel::Fatal, AlertDescription::ProtocolVersion, &[]);
    assert_eq!(alert_content, expected);
}

#[test]
fn test_no_warning_logging_during_successful_sessions() {
    CountingLogger::install();
    CountingLogger::reset();

    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        for version in rustls::ALL_VERSIONS {
            let client_config = make_client_config_with_versions(*kt, &[version], &provider);
            let (mut client, mut server) =
                make_pair_for_configs(client_config, make_server_config(*kt, &provider));
            do_handshake(&mut client, &mut server);
        }
    }

    if cfg!(feature = "log") {
        COUNTS.with(|c| {
            println!("After tests: {:?}", c.borrow());
            assert!(c.borrow().warn.is_empty());
            assert!(c.borrow().error.is_empty());
            assert!(c.borrow().info.is_empty());
            assert!(!c.borrow().trace.is_empty());
            assert!(!c.borrow().debug.is_empty());
        });
    } else {
        COUNTS.with(|c| {
            println!("After tests: {:?}", c.borrow());
            assert!(c.borrow().warn.is_empty());
            assert!(c.borrow().error.is_empty());
            assert!(c.borrow().info.is_empty());
            assert!(c.borrow().trace.is_empty());
            assert!(c.borrow().debug.is_empty());
        });
    }
}

/// Test that secrets can be extracted and used for encryption/decryption.
#[test]
fn test_secret_extraction_enabled() {
    // Normally, secret extraction would be used to configure kTLS (TLS offload
    // to the kernel). We want this test to run on any platform, though, so
    // instead we just compare secrets for equality.

    // TLS 1.2 and 1.3 have different mechanisms for key exchange and handshake,
    // and secrets are stored/extracted differently, so we want to test them both.
    // We support 3 different AEAD algorithms (AES-128-GCM mode, AES-256-GCM, and
    // Chacha20Poly1305), so that's 2*3 = 6 combinations to test.
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();
    for suite in [
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        #[cfg(not(feature = "fips"))]
        cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        #[cfg(not(feature = "fips"))]
        cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        println!("Testing suite {:?}", suite.suite().as_str());

        // Only offer the cipher suite (and protocol version) that we're testing
        let mut server_config = ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![suite],
                ..provider.clone()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(kt.get_chain(), kt.get_key())
        .unwrap();
        // Opt into secret extraction from both sides
        server_config.enable_secret_extraction = true;
        let server_config = Arc::new(server_config);

        let mut client_config = make_client_config(kt, &provider);
        client_config.enable_secret_extraction = true;

        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        do_handshake(&mut client, &mut server);

        // The handshake is finished, we're now able to extract traffic secrets
        let client_secrets = client
            .dangerous_extract_secrets()
            .unwrap();
        let server_secrets = server
            .dangerous_extract_secrets()
            .unwrap();

        // Comparing secrets for equality is something you should never have to
        // do in production code, so ConnectionTrafficSecrets doesn't implement
        // PartialEq/Eq on purpose. Instead, we have to get creative.
        fn explode_secrets(s: &ConnectionTrafficSecrets) -> (&[u8], &[u8]) {
            match s {
                ConnectionTrafficSecrets::Aes128Gcm { key, iv } => (key.as_ref(), iv.as_ref()),
                ConnectionTrafficSecrets::Aes256Gcm { key, iv } => (key.as_ref(), iv.as_ref()),
                ConnectionTrafficSecrets::Chacha20Poly1305 { key, iv } => {
                    (key.as_ref(), iv.as_ref())
                }
                _ => panic!("unexpected secret type"),
            }
        }

        fn assert_secrets_equal(
            (l_seq, l_sec): (u64, ConnectionTrafficSecrets),
            (r_seq, r_sec): (u64, ConnectionTrafficSecrets),
        ) {
            assert_eq!(l_seq, r_seq);
            assert_eq!(explode_secrets(&l_sec), explode_secrets(&r_sec));
        }

        assert_secrets_equal(client_secrets.tx, server_secrets.rx);
        assert_secrets_equal(client_secrets.rx, server_secrets.tx);
    }
}

#[test]
fn test_secret_extract_produces_correct_variant() {
    fn check(suite: SupportedCipherSuite, f: impl Fn(ConnectionTrafficSecrets) -> bool) {
        let kt = KeyType::Rsa2048;

        let provider: Arc<CryptoProvider> = CryptoProvider {
            cipher_suites: vec![suite],
            ..provider::default_provider()
        }
        .into();

        let mut server_config = finish_server_config(
            kt,
            ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap(),
        );

        server_config.enable_secret_extraction = true;
        let server_config = Arc::new(server_config);

        let mut client_config = finish_client_config(
            kt,
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap(),
        );
        client_config.enable_secret_extraction = true;

        let (mut client, mut server) =
            make_pair_for_arc_configs(&Arc::new(client_config), &server_config);

        do_handshake(&mut client, &mut server);

        let client_secrets = client
            .dangerous_extract_secrets()
            .unwrap();
        let server_secrets = server
            .dangerous_extract_secrets()
            .unwrap();

        assert!(f(client_secrets.tx.1));
        assert!(f(client_secrets.rx.1));
        assert!(f(server_secrets.tx.1));
        assert!(f(server_secrets.rx.1));
    }

    check(cipher_suite::TLS13_AES_128_GCM_SHA256, |sec| {
        matches!(sec, ConnectionTrafficSecrets::Aes128Gcm { .. })
    });
    check(cipher_suite::TLS13_AES_256_GCM_SHA384, |sec| {
        matches!(sec, ConnectionTrafficSecrets::Aes256Gcm { .. })
    });
    check(cipher_suite::TLS13_CHACHA20_POLY1305_SHA256, |sec| {
        matches!(sec, ConnectionTrafficSecrets::Chacha20Poly1305 { .. })
    });

    check(cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256, |sec| {
        matches!(sec, ConnectionTrafficSecrets::Aes128Gcm { .. })
    });
    check(cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, |sec| {
        matches!(sec, ConnectionTrafficSecrets::Aes256Gcm { .. })
    });
    check(
        cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        |sec| matches!(sec, ConnectionTrafficSecrets::Chacha20Poly1305 { .. }),
    );
}

/// Test that secrets cannot be extracted unless explicitly enabled, and until
/// the handshake is done.
#[test]
fn test_secret_extraction_disabled_or_too_early() {
    let kt = KeyType::Rsa2048;
    let provider = Arc::new(CryptoProvider {
        cipher_suites: vec![cipher_suite::TLS13_AES_128_GCM_SHA256],
        ..provider::default_provider()
    });

    for (server_enable, client_enable) in [(true, false), (false, true)] {
        let mut server_config = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(kt.get_chain(), kt.get_key())
            .unwrap();
        server_config.enable_secret_extraction = server_enable;
        let server_config = Arc::new(server_config);

        let mut client_config = make_client_config(kt, &provider);
        client_config.enable_secret_extraction = client_enable;

        let client_config = Arc::new(client_config);

        let (client, server) = make_pair_for_arc_configs(&client_config, &server_config);

        assert!(
            client
                .dangerous_extract_secrets()
                .is_err(),
            "extraction should fail until handshake completes"
        );
        assert!(
            server
                .dangerous_extract_secrets()
                .is_err(),
            "extraction should fail until handshake completes"
        );

        let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);

        do_handshake(&mut client, &mut server);

        assert_eq!(
            server_enable,
            server
                .dangerous_extract_secrets()
                .is_ok()
        );
        assert_eq!(
            client_enable,
            client
                .dangerous_extract_secrets()
                .is_ok()
        );
    }
}

#[test]
fn test_received_plaintext_backpressure() {
    let kt = KeyType::Rsa2048;
    let provider = provider::default_provider();

    let server_config = Arc::new(
        ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: vec![cipher_suite::TLS13_AES_128_GCM_SHA256],
                ..provider.clone()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(kt.get_chain(), kt.get_key())
        .unwrap(),
    );

    let client_config = Arc::new(make_client_config(kt, &provider));
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    // Fill the server's received plaintext buffer with 16k bytes
    let client_buf = [0; 16_385];
    dbg!(
        client
            .writer()
            .write(&client_buf)
            .unwrap()
    );
    let mut network_buf = Vec::with_capacity(32_768);
    let sent = dbg!(
        client
            .write_tls(&mut network_buf)
            .unwrap()
    );
    let mut read = 0;
    while read < sent {
        let new = dbg!(
            server
                .read_tls(&mut &network_buf[read..sent])
                .unwrap()
        );
        if new == 4096 {
            read += new;
        } else {
            break;
        }
    }
    server.process_new_packets().unwrap();

    // Send two more bytes from client to server
    dbg!(
        client
            .writer()
            .write(&client_buf[..2])
            .unwrap()
    );
    let sent = dbg!(
        client
            .write_tls(&mut network_buf)
            .unwrap()
    );

    // Get an error because the received plaintext buffer is full
    assert!(
        server
            .read_tls(&mut &network_buf[..sent])
            .is_err()
    );

    // Read out some of the plaintext
    server
        .reader()
        .read_exact(&mut [0; 2])
        .unwrap();

    // Now there's room again in the plaintext buffer
    assert_eq!(
        server
            .read_tls(&mut &network_buf[..sent])
            .unwrap(),
        24
    );
}

#[test]
fn test_debug_server_name_from_ip() {
    assert_eq!(
        format!(
            "{:?}",
            ServerName::IpAddress(IpAddr::try_from("127.0.0.1").unwrap())
        ),
        "IpAddress(V4(Ipv4Addr([127, 0, 0, 1])))"
    )
}

#[test]
fn test_debug_server_name_from_string() {
    assert_eq!(
        format!("{:?}", ServerName::try_from("a.com").unwrap()),
        "DnsName(\"a.com\")"
    )
}

#[cfg(all(feature = "ring", feature = "aws-lc-rs"))]
#[test]
fn test_explicit_provider_selection() {
    let client_config = finish_client_config(
        KeyType::Rsa2048,
        rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );
    let server_config = finish_server_config(
        KeyType::Rsa2048,
        rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);
}

#[derive(Debug)]
struct FaultyRandom {
    // when empty, `fill_random` requests return `GetRandomFailed`
    rand_queue: Mutex<&'static [u8]>,
}

impl rustls::crypto::SecureRandom for FaultyRandom {
    fn fill(&self, output: &mut [u8]) -> Result<(), rustls::crypto::GetRandomFailed> {
        let mut queue = self.rand_queue.lock().unwrap();

        println!(
            "fill_random request for {} bytes (got {})",
            output.len(),
            queue.len()
        );

        if queue.len() < output.len() {
            return Err(rustls::crypto::GetRandomFailed);
        }

        let fixed_output = &queue[..output.len()];
        output.copy_from_slice(fixed_output);
        *queue = &queue[output.len()..];
        Ok(())
    }
}

#[test]
fn test_client_construction_fails_if_random_source_fails_in_first_request() {
    static FAULTY_RANDOM: FaultyRandom = FaultyRandom {
        rand_queue: Mutex::new(b""),
    };

    let client_config = finish_client_config(
        KeyType::Rsa2048,
        rustls::ClientConfig::builder_with_provider(
            CryptoProvider {
                secure_random: &FAULTY_RANDOM,
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    assert_eq!(
        ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap_err(),
        Error::FailedToGetRandomBytes
    );
}

#[test]
fn test_client_construction_fails_if_random_source_fails_in_second_request() {
    static FAULTY_RANDOM: FaultyRandom = FaultyRandom {
        rand_queue: Mutex::new(b"nice random number generator huh"),
    };

    let client_config = finish_client_config(
        KeyType::Rsa2048,
        rustls::ClientConfig::builder_with_provider(
            CryptoProvider {
                secure_random: &FAULTY_RANDOM,
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    assert_eq!(
        ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap_err(),
        Error::FailedToGetRandomBytes
    );
}

#[test]
fn test_client_construction_requires_66_bytes_of_random_material() {
    static FAULTY_RANDOM: FaultyRandom = FaultyRandom {
        rand_queue: Mutex::new(
            b"nice random number generator !!!!!\
                                 it's really not very good is it?",
        ),
    };

    let client_config = finish_client_config(
        KeyType::Rsa2048,
        rustls::ClientConfig::builder_with_provider(
            CryptoProvider {
                secure_random: &FAULTY_RANDOM,
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );

    ClientConnection::new(Arc::new(client_config), server_name("localhost"))
        .expect("check how much random material ClientConnection::new consumes");
}

#[test]
fn test_client_removes_tls12_session_if_server_sends_undecryptable_first_message() {
    fn inject_corrupt_finished_message(msg: &mut Message) -> Altered {
        if let MessagePayload::ChangeCipherSpec(_) = msg.payload {
            // interdict "real" ChangeCipherSpec with its encoding, plus a faulty encrypted Finished.
            let mut raw_change_cipher_spec = encoding::message_framing(
                ContentType::ChangeCipherSpec,
                ProtocolVersion::TLSv1_2,
                vec![0x01],
            );
            let mut corrupt_finished = encoding::message_framing(
                ContentType::Handshake,
                ProtocolVersion::TLSv1_2,
                vec![0u8; 0x28],
            );

            let mut both = vec![];
            both.append(&mut raw_change_cipher_spec);
            both.append(&mut corrupt_finished);

            Altered::Raw(both)
        } else {
            Altered::InPlace
        }
    }

    let provider = provider::default_provider();
    let mut client_config =
        make_client_config_with_versions(KeyType::Rsa2048, &[&rustls::version::TLS12], &provider);
    let storage = Arc::new(ClientStorage::new());
    client_config.resumption = Resumption::store(storage.clone());
    let client_config = Arc::new(client_config);
    let server_config = Arc::new(make_server_config(KeyType::Rsa2048, &provider));

    // successful handshake to allow resumption
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    // resumption
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    let mut client = client.into();
    transfer_altered(
        &mut server.into(),
        inject_corrupt_finished_message,
        &mut client,
    );

    // discard storage operations up to this point, to observe the one we want to test for.
    storage.ops_and_reset();

    // client cannot decrypt faulty Finished, and deletes saved session in case
    // server resumption is buggy.
    assert_eq!(
        Some(Error::DecryptError),
        client.process_new_packets().err()
    );

    assert!(matches!(
        storage.ops()[0],
        ClientStorageOp::RemoveTls12Session(_)
    ));
}

#[test]
fn test_client_fips_service_indicator() {
    assert_eq!(
        make_client_config(KeyType::Rsa2048, &provider::default_provider()).fips(),
        provider_is_fips()
    );
}

#[test]
fn test_server_fips_service_indicator() {
    assert_eq!(
        make_server_config(KeyType::Rsa2048, &provider::default_provider()).fips(),
        provider_is_fips()
    );
}

#[test]
fn test_connection_fips_service_indicator() {
    let provider = provider::default_provider();
    let client_config = Arc::new(make_client_config(KeyType::Rsa2048, &provider));
    let server_config = Arc::new(make_server_config(KeyType::Rsa2048, &provider));
    let conn_pair = make_pair_for_arc_configs(&client_config, &server_config);
    // Each connection's FIPS status should reflect the FIPS status of the config it was created
    // from.
    assert_eq!(client_config.fips(), conn_pair.0.fips());
    assert_eq!(server_config.fips(), conn_pair.1.fips());
}

#[test]
fn test_client_fips_service_indicator_includes_require_ems() {
    if !provider_is_fips() {
        return;
    }

    let mut client_config = make_client_config(KeyType::Rsa2048, &provider::default_provider());
    assert!(client_config.fips());
    client_config.require_ems = false;
    assert!(!client_config.fips());
}

#[test]
fn test_server_fips_service_indicator_includes_require_ems() {
    if !provider_is_fips() {
        return;
    }

    let mut server_config = make_server_config(KeyType::Rsa2048, &provider::default_provider());
    assert!(server_config.fips());
    server_config.require_ems = false;
    assert!(!server_config.fips());
}

#[cfg(feature = "aws-lc-rs")]
#[test]
fn test_client_fips_service_indicator_includes_ech_hpke_suite() {
    if !provider_is_fips() {
        return;
    }

    for suite in ALL_SUPPORTED_SUITES {
        let (public_key, _) = suite.generate_key_pair().unwrap();

        let suite_id = suite.suite();
        let bogus_config = EchConfigPayload::V18(EchConfigContents {
            key_config: HpkeKeyConfig {
                config_id: 10,
                kem_id: suite_id.kem,
                public_key: PayloadU16::new(public_key.0.clone()),
                symmetric_cipher_suites: vec![HpkeSymmetricCipherSuite {
                    kdf_id: suite_id.sym.kdf_id,
                    aead_id: suite_id.sym.aead_id,
                }],
            },
            maximum_name_length: 0,
            public_name: DnsName::try_from("example.com").unwrap(),
            extensions: vec![],
        });
        let mut bogus_config_bytes = Vec::new();
        vec![bogus_config].encode(&mut bogus_config_bytes);
        let ech_config =
            EchConfig::new(EchConfigListBytes::from(bogus_config_bytes), &[*suite]).unwrap();

        // A ECH client configuration should only be considered FIPS approved if the
        // ECH HPKE suite is itself FIPS approved.
        let config = ClientConfig::builder_with_provider(provider::default_provider().into())
            .with_ech(EchMode::Enable(ech_config))
            .unwrap();
        let config = finish_client_config(KeyType::Rsa2048, config);
        assert_eq!(config.fips(), suite.fips());

        // The same applies if an ECH GREASE client configuration is used.
        let config = ClientConfig::builder_with_provider(provider::default_provider().into())
            .with_ech(EchMode::Grease(EchGreaseConfig::new(*suite, public_key)))
            .unwrap();
        let config = finish_client_config(KeyType::Rsa2048, config);
        assert_eq!(config.fips(), suite.fips());

        // And a connection made from a client config should retain the fips status of the
        // config w.r.t the HPKE suite.
        let conn = ClientConnection::new(
            config.into(),
            ServerName::DnsName(DnsName::try_from("example.org").unwrap()),
        )
        .unwrap();
        assert_eq!(conn.fips(), suite.fips());
    }
}

#[test]
fn test_complete_io_errors_if_close_notify_received_too_early() {
    let mut server = ServerConnection::new(Arc::new(make_server_config(
        KeyType::Rsa2048,
        &provider::default_provider(),
    )))
    .unwrap();
    let client_hello_followed_by_close_notify_alert = b"\
        \x16\x03\x01\x00\xc8\x01\x00\x00\xc4\x03\x03\xec\x12\xdd\x17\x64\
        \xa4\x39\xfd\x7e\x8c\x85\x46\xb8\x4d\x1e\xa0\x6e\xb3\xd7\xa0\x51\
        \xf0\x3c\xb8\x17\x47\x0d\x4c\x54\xc5\xdf\x72\x00\x00\x1c\xea\xea\
        \xc0\x2b\xc0\x2f\xc0\x2c\xc0\x30\xcc\xa9\xcc\xa8\xc0\x13\xc0\x14\
        \x00\x9c\x00\x9d\x00\x2f\x00\x35\x00\x0a\x01\x00\x00\x7f\xda\xda\
        \x00\x00\xff\x01\x00\x01\x00\x00\x00\x00\x16\x00\x14\x00\x00\x11\
        \x77\x77\x77\x2e\x77\x69\x6b\x69\x70\x65\x64\x69\x61\x2e\x6f\x72\
        \x67\x00\x17\x00\x00\x00\x23\x00\x00\x00\x0d\x00\x14\x00\x12\x04\
        \x03\x08\x04\x04\x01\x05\x03\x08\x05\x05\x01\x08\x06\x06\x01\x02\
        \x01\x00\x05\x00\x05\x01\x00\x00\x00\x00\x00\x12\x00\x00\x00\x10\
        \x00\x0e\x00\x0c\x02\x68\x32\x08\x68\x74\x74\x70\x2f\x31\x2e\x31\
        \x75\x50\x00\x00\x00\x0b\x00\x02\x01\x00\x00\x0a\x00\x0a\x00\x08\
        \x1a\x1a\x00\x1d\x00\x17\x00\x18\x1a\x1a\x00\x01\x00\
        \x15\x03\x03\x00\x02\x01\x00";

    let mut stream = FakeStream(client_hello_followed_by_close_notify_alert);
    assert_eq!(
        server
            .complete_io(&mut stream)
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn test_complete_io_with_no_io_needed() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);
    client
        .writer()
        .write_all(b"hello")
        .unwrap();
    client.send_close_notify();
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    server
        .writer()
        .write_all(b"hello")
        .unwrap();
    server.send_close_notify();
    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    // neither want any IO: both directions are closed.
    assert!(!client.wants_write());
    assert!(!client.wants_read());
    assert!(!server.wants_write());
    assert!(!server.wants_read());
    assert_eq!(
        client
            .complete_io(&mut FakeStream(&[]))
            .unwrap(),
        (0, 0)
    );
    assert_eq!(
        server
            .complete_io(&mut FakeStream(&[]))
            .unwrap(),
        (0, 0)
    );
}

#[test]
fn test_junk_after_close_notify_received() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);
    client
        .writer()
        .write_all(b"hello")
        .unwrap();
    client.send_close_notify();

    let mut client_buffer = vec![];
    client
        .write_tls(&mut io::Cursor::new(&mut client_buffer))
        .unwrap();

    // add some junk that will be dropped from the deframer buffer
    // after the close_notify
    client_buffer.extend_from_slice(&[0x17, 0x03, 0x03, 0x01]);

    server
        .read_tls(&mut io::Cursor::new(&client_buffer[..]))
        .unwrap();
    server.process_new_packets().unwrap();
    server.process_new_packets().unwrap(); // check for desync

    // can read data received prior to close_notify
    let mut received_data = [0u8; 128];
    let len = server
        .reader()
        .read(&mut received_data)
        .unwrap();
    assert_eq!(&received_data[..len], b"hello");

    // but subsequent reads just report clean EOF
    assert_eq!(
        server
            .reader()
            .read(&mut received_data)
            .unwrap(),
        0
    );
}

#[test]
fn test_data_after_close_notify_is_ignored() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    client
        .writer()
        .write_all(b"before")
        .unwrap();
    client.send_close_notify();
    client
        .writer()
        .write_all(b"after")
        .unwrap();
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    let mut received_data = [0u8; 128];
    let count = server
        .reader()
        .read(&mut received_data)
        .unwrap();
    assert_eq!(&received_data[..count], b"before");
    assert_eq!(
        server
            .reader()
            .read(&mut received_data)
            .unwrap(),
        0
    );
}

#[test]
fn test_close_notify_sent_prior_to_handshake_complete() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    client.send_close_notify();
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Server(Error::AlertReceived(
            AlertDescription::CloseNotify
        )))
    );
}

#[test]
fn test_subsequent_close_notify_ignored() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    client.send_close_notify();
    assert!(transfer(&mut client, &mut server) > 0);

    // does nothing
    client.send_close_notify();
    assert_eq!(transfer(&mut client, &mut server), 0);
}

#[test]
fn test_second_close_notify_after_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);
    client.send_close_notify();
    assert!(transfer(&mut client, &mut server) > 0);
    server.process_new_packets().unwrap();

    // does nothing
    client.send_close_notify();
    assert_eq!(transfer(&mut client, &mut server), 0);
}

#[test]
fn test_read_tls_artificial_eof_after_close_notify() {
    let (mut client, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    do_handshake(&mut client, &mut server);
    client.send_close_notify();
    assert!(transfer(&mut client, &mut server) > 0);
    server.process_new_packets().unwrap();

    let buf = [1, 2, 3, 4];
    assert_eq!(
        server
            .read_tls(&mut io::Cursor::new(buf))
            .unwrap(),
        0
    );
}

#[test]
fn test_pinned_ocsp_response_given_to_custom_server_cert_verifier() {
    let ocsp_response = b"hello-ocsp-world!";
    let kt = KeyType::EcdsaP256;
    let provider = provider::default_provider();

    for version in rustls::ALL_VERSIONS {
        let server_config = server_config_builder(&provider)
            .with_no_client_auth()
            .with_single_cert_with_ocsp(kt.get_chain(), kt.get_key(), ocsp_response.to_vec())
            .unwrap();

        let client_config = client_config_builder_with_versions(&[version], &provider)
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(MockServerVerifier::expects_ocsp_response(
                ocsp_response,
            )))
            .with_no_client_auth();

        let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
        do_handshake(&mut client, &mut server);
    }
}

#[cfg(feature = "zlib")]
#[test]
fn test_server_uses_cached_compressed_certificates() {
    static COMPRESS_COUNT: AtomicUsize = AtomicUsize::new(0);

    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.cert_compressors = vec![&CountingCompressor];
    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.resumption = Resumption::disabled();

    let server_config = Arc::new(server_config);
    let client_config = Arc::new(client_config);

    for _i in 0..10 {
        dbg!(_i);
        let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
        do_handshake(&mut client, &mut server);
        dbg!(client.handshake_kind());
    }

    assert_eq!(COMPRESS_COUNT.load(Ordering::SeqCst), 1);

    #[derive(Debug)]
    struct CountingCompressor;

    impl rustls::compress::CertCompressor for CountingCompressor {
        fn compress(
            &self,
            input: Vec<u8>,
            level: rustls::compress::CompressionLevel,
        ) -> Result<Vec<u8>, rustls::compress::CompressionFailed> {
            dbg!(COMPRESS_COUNT.fetch_add(1, Ordering::SeqCst));
            rustls::compress::ZLIB_COMPRESSOR.compress(input, level)
        }

        fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
            rustls::CertificateCompressionAlgorithm::Zlib
        }
    }
}

#[test]
fn test_server_uses_uncompressed_certificate_if_compression_fails() {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.cert_compressors = vec![&FailingCompressor];
    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.cert_decompressors = vec![&NeverDecompressor];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);
}

#[test]
fn test_client_uses_uncompressed_certificate_if_compression_fails() {
    let provider = provider::default_provider();
    let mut server_config =
        make_server_config_with_mandatory_client_auth(KeyType::Rsa2048, &provider);
    server_config.cert_decompressors = vec![&NeverDecompressor];
    let mut client_config = make_client_config_with_auth(KeyType::Rsa2048, &provider);
    client_config.cert_compressors = vec![&FailingCompressor];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);
}

#[derive(Debug)]
struct FailingCompressor;

impl rustls::compress::CertCompressor for FailingCompressor {
    fn compress(
        &self,
        _input: Vec<u8>,
        _level: rustls::compress::CompressionLevel,
    ) -> Result<Vec<u8>, rustls::compress::CompressionFailed> {
        println!("compress called but doesn't work");
        Err(rustls::compress::CompressionFailed)
    }

    fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
        rustls::CertificateCompressionAlgorithm::Zlib
    }
}

#[derive(Debug)]
struct NeverDecompressor;

impl rustls::compress::CertDecompressor for NeverDecompressor {
    fn decompress(
        &self,
        _input: &[u8],
        _output: &mut [u8],
    ) -> Result<(), rustls::compress::DecompressionFailed> {
        panic!("NeverDecompressor::decompress should not be called");
    }

    fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
        rustls::CertificateCompressionAlgorithm::Zlib
    }
}

#[cfg(feature = "zlib")]
#[test]
fn test_server_can_opt_out_of_compression_cache() {
    static COMPRESS_COUNT: AtomicUsize = AtomicUsize::new(0);

    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.cert_compressors = vec![&AlwaysInteractiveCompressor];
    server_config.cert_compression_cache = Arc::new(rustls::compress::CompressionCache::Disabled);
    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.resumption = Resumption::disabled();

    let server_config = Arc::new(server_config);
    let client_config = Arc::new(client_config);

    for _i in 0..10 {
        dbg!(_i);
        let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
        do_handshake(&mut client, &mut server);
        dbg!(client.handshake_kind());
    }

    assert_eq!(COMPRESS_COUNT.load(Ordering::SeqCst), 10);

    #[derive(Debug)]
    struct AlwaysInteractiveCompressor;

    impl rustls::compress::CertCompressor for AlwaysInteractiveCompressor {
        fn compress(
            &self,
            input: Vec<u8>,
            level: rustls::compress::CompressionLevel,
        ) -> Result<Vec<u8>, rustls::compress::CompressionFailed> {
            dbg!(COMPRESS_COUNT.fetch_add(1, Ordering::SeqCst));
            assert_eq!(level, rustls::compress::CompressionLevel::Interactive);
            rustls::compress::ZLIB_COMPRESSOR.compress(input, level)
        }

        fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
            rustls::CertificateCompressionAlgorithm::Zlib
        }
    }
}

#[test]
fn test_cert_decompression_by_client_produces_invalid_cert_payload() {
    let provider = provider::default_provider();
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.cert_compressors = vec![&IdentityCompressor];
    let mut client_config = make_client_config(KeyType::Rsa2048, &provider);
    client_config.cert_decompressors = vec![&GarbageDecompressor];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Client(Error::InvalidMessage(
            InvalidMessage::CertificatePayloadTooLarge
        )))
    );
    transfer(&mut client, &mut server);
    assert_eq!(
        server.process_new_packets(),
        Err(Error::AlertReceived(AlertDescription::BadCertificate))
    );
}

#[test]
fn test_cert_decompression_by_server_produces_invalid_cert_payload() {
    let provider = provider::default_provider();
    let mut server_config =
        make_server_config_with_mandatory_client_auth(KeyType::Rsa2048, &provider);
    server_config.cert_decompressors = vec![&GarbageDecompressor];
    let mut client_config = make_client_config_with_auth(KeyType::Rsa2048, &provider);
    client_config.cert_compressors = vec![&IdentityCompressor];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Server(Error::InvalidMessage(
            InvalidMessage::CertificatePayloadTooLarge
        )))
    );
    transfer(&mut server, &mut client);
    assert_eq!(
        client.process_new_packets(),
        Err(Error::AlertReceived(AlertDescription::BadCertificate))
    );
}

#[test]
fn test_cert_decompression_by_server_fails() {
    let provider = provider::default_provider();
    let mut server_config =
        make_server_config_with_mandatory_client_auth(KeyType::Rsa2048, &provider);
    server_config.cert_decompressors = vec![&FailingDecompressor];
    let mut client_config = make_client_config_with_auth(KeyType::Rsa2048, &provider);
    client_config.cert_compressors = vec![&IdentityCompressor];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Server(Error::PeerMisbehaved(
            PeerMisbehaved::InvalidCertCompression
        )))
    );
    transfer(&mut server, &mut client);
    assert_eq!(
        client.process_new_packets(),
        Err(Error::AlertReceived(AlertDescription::BadCertificate))
    );
}

#[cfg(feature = "zlib")]
#[test]
fn test_cert_decompression_by_server_would_result_in_excessively_large_cert() {
    let provider = provider::default_provider();
    let server_config = make_server_config_with_mandatory_client_auth(KeyType::Rsa2048, &provider);
    let mut client_config = make_client_config_with_auth(KeyType::Rsa2048, &provider);

    let big_cert = CertificateDer::from(vec![0u8; 0xffff]);
    let key = provider::default_provider()
        .key_provider
        .load_private_key(KeyType::Rsa2048.get_client_key())
        .unwrap();
    let big_cert_and_key = sign::CertifiedKey::new_unchecked(vec![big_cert], key);
    client_config.client_auth_cert_resolver =
        Arc::new(sign::SingleCertAndKey::from(big_cert_and_key));

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    assert_eq!(
        do_handshake_until_error(&mut client, &mut server),
        Err(ErrorFromPeer::Server(Error::InvalidMessage(
            InvalidMessage::MessageTooLarge
        )))
    );
    transfer(&mut server, &mut client);
    assert_eq!(
        client.process_new_packets(),
        Err(Error::AlertReceived(AlertDescription::BadCertificate))
    );
}

#[derive(Debug)]
struct GarbageDecompressor;

impl rustls::compress::CertDecompressor for GarbageDecompressor {
    fn decompress(
        &self,
        _input: &[u8],
        output: &mut [u8],
    ) -> Result<(), rustls::compress::DecompressionFailed> {
        output.fill(0xff);
        Ok(())
    }

    fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
        rustls::CertificateCompressionAlgorithm::Zlib
    }
}

#[derive(Debug)]
struct FailingDecompressor;

impl rustls::compress::CertDecompressor for FailingDecompressor {
    fn decompress(
        &self,
        _input: &[u8],
        _output: &mut [u8],
    ) -> Result<(), rustls::compress::DecompressionFailed> {
        Err(rustls::compress::DecompressionFailed)
    }

    fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
        rustls::CertificateCompressionAlgorithm::Zlib
    }
}

#[derive(Debug)]
struct IdentityCompressor;

impl rustls::compress::CertCompressor for IdentityCompressor {
    fn compress(
        &self,
        input: Vec<u8>,
        _level: rustls::compress::CompressionLevel,
    ) -> Result<Vec<u8>, rustls::compress::CompressionFailed> {
        Ok(input.to_vec())
    }

    fn algorithm(&self) -> rustls::CertificateCompressionAlgorithm {
        rustls::CertificateCompressionAlgorithm::Zlib
    }
}

struct FakeStream<'a>(&'a [u8]);

impl io::Read for FakeStream<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let take = core::cmp::min(b.len(), self.0.len());
        let (taken, remain) = self.0.split_at(take);
        b[..take].copy_from_slice(taken);
        self.0 = remain;
        Ok(take)
    }
}

impl io::Write for FakeStream<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_illegal_server_renegotiation_attempt_after_tls13_handshake() {
    let provider = provider::default_provider();
    let client_config =
        make_client_config_with_versions(KeyType::Rsa2048, &[&rustls::version::TLS13], &provider);
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.enable_secret_extraction = true;

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    let mut raw_server = RawTls::new_server(server);

    let msg = PlainMessage {
        typ: ContentType::Handshake,
        version: ProtocolVersion::TLSv1_3,
        payload: Payload::new(encoding::handshake_framing(
            HandshakeType::HelloRequest,
            vec![],
        )),
    };
    raw_server.encrypt_and_send(&msg, &mut client);
    let err = client
        .process_new_packets()
        .unwrap_err();
    assert_eq!(
        err,
        Error::InappropriateHandshakeMessage {
            expect_types: vec![HandshakeType::NewSessionTicket, HandshakeType::KeyUpdate],
            got_type: HandshakeType::HelloRequest
        }
    );
}

#[test]
fn test_illegal_server_renegotiation_attempt_after_tls12_handshake() {
    let provider = provider::default_provider();
    let client_config =
        make_client_config_with_versions(KeyType::Rsa2048, &[&rustls::version::TLS12], &provider);
    let mut server_config = make_server_config(KeyType::Rsa2048, &provider);
    server_config.enable_secret_extraction = true;

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    let mut raw_server = RawTls::new_server(server);

    let msg = PlainMessage {
        typ: ContentType::Handshake,
        version: ProtocolVersion::TLSv1_3,
        payload: Payload::new(encoding::handshake_framing(
            HandshakeType::HelloRequest,
            vec![],
        )),
    };

    // one is allowed (and elicits a warning alert)
    raw_server.encrypt_and_send(&msg, &mut client);
    client.process_new_packets().unwrap();
    raw_server.receive_and_decrypt(&mut client, |m| {
        assert_eq!(format!("{m:?}"),
                   "Message { version: TLSv1_2, payload: Alert(AlertMessagePayload { level: Warning, description: NoRenegotiation }) }");
    });

    // second is fatal
    raw_server.encrypt_and_send(&msg, &mut client);
    assert_eq!(
        client
            .process_new_packets()
            .unwrap_err(),
        Error::PeerMisbehaved(PeerMisbehaved::TooManyRenegotiationRequests)
    );
}

#[test]
fn test_illegal_client_renegotiation_attempt_after_tls13_handshake() {
    let provider = provider::default_provider();
    let mut client_config =
        make_client_config_with_versions(KeyType::Rsa2048, &[&rustls::version::TLS13], &provider);
    client_config.enable_secret_extraction = true;
    let server_config = make_server_config(KeyType::Rsa2048, &provider);

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    let mut raw_client = RawTls::new_client(client);

    let msg = PlainMessage {
        typ: ContentType::Handshake,
        version: ProtocolVersion::TLSv1_3,
        payload: Payload::new(encoding::basic_client_hello(vec![])),
    };
    raw_client.encrypt_and_send(&msg, &mut server);
    let err = server
        .process_new_packets()
        .unwrap_err();
    assert_eq!(
        format!("{err:?}"),
        "InappropriateHandshakeMessage { expect_types: [KeyUpdate], got_type: ClientHello }"
    );
}

#[test]
fn test_illegal_client_renegotiation_attempt_during_tls12_handshake() {
    let provider = provider::default_provider();
    let server_config = make_server_config(KeyType::Rsa2048, &provider);
    let client_config =
        make_client_config_with_versions(KeyType::Rsa2048, &[&rustls::version::TLS12], &provider);
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    let mut client_hello = vec![];
    client
        .write_tls(&mut io::Cursor::new(&mut client_hello))
        .unwrap();

    server
        .read_tls(&mut io::Cursor::new(&client_hello))
        .unwrap();
    server
        .read_tls(&mut io::Cursor::new(&client_hello))
        .unwrap();
    assert_eq!(
        server
            .process_new_packets()
            .unwrap_err(),
        Error::InappropriateHandshakeMessage {
            expect_types: vec![HandshakeType::ClientKeyExchange],
            got_type: HandshakeType::ClientHello
        }
    );
}

#[test]
fn test_refresh_traffic_keys_during_handshake() {
    let (mut client, mut server) = make_pair(KeyType::Ed25519, &provider::default_provider());
    assert_eq!(
        client
            .refresh_traffic_keys()
            .unwrap_err(),
        Error::HandshakeNotComplete
    );
    assert_eq!(
        server
            .refresh_traffic_keys()
            .unwrap_err(),
        Error::HandshakeNotComplete
    );
}

#[test]
fn test_refresh_traffic_keys() {
    let (mut client, mut server) = make_pair(KeyType::Ed25519, &provider::default_provider());
    do_handshake(&mut client, &mut server);

    fn check_both_directions(client: &mut ClientConnection, server: &mut ServerConnection) {
        client
            .writer()
            .write_all(b"to-server-1")
            .unwrap();
        server
            .writer()
            .write_all(b"to-client-1")
            .unwrap();
        transfer(client, server);
        server.process_new_packets().unwrap();

        transfer(server, client);
        client.process_new_packets().unwrap();

        let mut buf = [0u8; 16];
        let len = server.reader().read(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"to-server-1");

        let len = client.reader().read(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"to-client-1");
    }

    check_both_directions(&mut client, &mut server);
    client.refresh_traffic_keys().unwrap();
    check_both_directions(&mut client, &mut server);
    server.refresh_traffic_keys().unwrap();
    check_both_directions(&mut client, &mut server);
}

#[test]
fn test_automatic_refresh_traffic_keys() {
    const fn encrypted_size(body: usize) -> usize {
        let padding = 1;
        let header = 5;
        let tag = 16;
        header + body + padding + tag
    }

    const KEY_UPDATE_SIZE: usize = encrypted_size(5);
    let provider = aes_128_gcm_with_1024_confidentiality_limit(provider::default_provider());

    let client_config = finish_client_config(
        KeyType::Ed25519,
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap(),
    );
    let server_config = finish_server_config(
        KeyType::Ed25519,
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap(),
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    for i in 0..(CONFIDENTIALITY_LIMIT + 16) {
        let message = format!("{i:08}");
        client
            .writer()
            .write_all(message.as_bytes())
            .unwrap();
        let transferred = transfer(&mut client, &mut server);
        println!(
            "{}: {} -> {:?}",
            i,
            transferred,
            server.process_new_packets().unwrap()
        );

        // at CONFIDENTIALITY_LIMIT messages, we also have a key_update message sent
        assert_eq!(
            transferred,
            match i {
                CONFIDENTIALITY_LIMIT => KEY_UPDATE_SIZE + encrypted_size(message.len()),
                _ => encrypted_size(message.len()),
            }
        );

        let mut buf = [0u8; 32];
        let recvd = server.reader().read(&mut buf).unwrap();
        assert_eq!(&buf[..recvd], message.as_bytes());
    }

    // finally, server writes and pumps its key_update response
    let message = b"finished";
    server
        .writer()
        .write_all(message)
        .unwrap();
    let transferred = transfer(&mut server, &mut client);

    println!(
        "F: {} -> {:?}",
        transferred,
        client.process_new_packets().unwrap()
    );
    assert_eq!(transferred, KEY_UPDATE_SIZE + encrypted_size(message.len()));
}

#[test]
fn tls12_connection_fails_after_key_reaches_confidentiality_limit() {
    let provider = aes_128_gcm_with_1024_confidentiality_limit(provider::default_provider());

    let client_config = finish_client_config(
        KeyType::Ed25519,
        ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap(),
    );
    let server_config = finish_server_config(
        KeyType::Ed25519,
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap(),
    );

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);
    do_handshake(&mut client, &mut server);

    for i in 0..CONFIDENTIALITY_LIMIT {
        let message = format!("{i:08}");
        client
            .writer()
            .write_all(message.as_bytes())
            .unwrap();
        let transferred = transfer(&mut client, &mut server);
        println!(
            "{}: {} -> {:?}",
            i,
            transferred,
            server.process_new_packets().unwrap()
        );

        let mut buf = [0u8; 32];
        let recvd = server.reader().read(&mut buf).unwrap();

        match i {
            1023 => assert_eq!(recvd, 0),
            _ => assert_eq!(&buf[..recvd], message.as_bytes()),
        }
    }
}

#[test]
fn test_keys_match_for_all_signing_key_types() {
    let provider = provider::default_provider();
    for kt in KeyType::all_for_provider(&provider) {
        let key = provider
            .key_provider
            .load_private_key(kt.get_client_key())
            .unwrap();
        let _ = sign::CertifiedKey::new(kt.get_client_chain(), key).expect("keys match");
        println!("{kt:?} ok");
    }
}

#[test]
fn tls13_packed_handshake() {
    // transcript requires selection of X25519
    if provider_is_fips() {
        return;
    }

    // regression test for https://github.com/rustls/rustls/issues/2040
    // (did not affect the buffered api)
    let client_config = ClientConfig::builder_with_provider(unsafe_plaintext_crypto_provider(
        provider::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(MockServerVerifier::rejects_certificate(
        CertificateError::UnknownIssuer.into(),
    )))
    .with_no_client_auth();

    let mut client =
        ClientConnection::new(Arc::new(client_config), server_name("localhost")).unwrap();

    let mut hello = Vec::new();
    client
        .write_tls(&mut io::Cursor::new(&mut hello))
        .unwrap();

    let first_flight = include_bytes!("data/bug2040-message-1.bin");
    client
        .read_tls(&mut io::Cursor::new(first_flight))
        .unwrap();
    client.process_new_packets().unwrap();

    let second_flight = include_bytes!("data/bug2040-message-2.bin");
    client
        .read_tls(&mut io::Cursor::new(second_flight))
        .unwrap();
    assert_eq!(
        client
            .process_new_packets()
            .unwrap_err(),
        Error::InvalidCertificate(CertificateError::UnknownIssuer),
    );
}

#[test]
fn large_client_hello() {
    let (_, mut server) = make_pair(KeyType::Rsa2048, &provider::default_provider());
    let hello = include_bytes!("data/bug2227-clienthello.bin");
    let mut cursor = io::Cursor::new(hello);
    loop {
        if server.read_tls(&mut cursor).unwrap() == 0 {
            break;
        }
        server.process_new_packets().unwrap();
    }
}

#[test]
fn large_client_hello_acceptor() {
    let mut acceptor = rustls::server::Acceptor::default();
    let hello = include_bytes!("data/bug2227-clienthello.bin");
    let mut cursor = io::Cursor::new(hello);
    loop {
        acceptor.read_tls(&mut cursor).unwrap();

        if let Some(accepted) = acceptor.accept().unwrap() {
            println!("{accepted:?}");
            break;
        }
    }
}

#[test]
fn hybrid_kx_component_share_offered_but_server_chooses_something_else() {
    let kt = KeyType::Rsa2048;
    let client_config = finish_client_config(
        kt,
        ClientConfig::builder_with_provider(
            CryptoProvider {
                kx_groups: vec![&FakeHybrid, provider::kx_group::SECP384R1],
                ..provider::default_provider()
            }
            .into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap(),
    );
    let provider = provider::default_provider();
    let server_config = make_server_config(kt, &provider);

    let (mut client_1, mut server) = make_pair_for_configs(client_config, server_config);
    let (mut client_2, _) = make_pair(kt, &provider);

    // client_2 supplies the ClientHello, client_1 receives the ServerHello
    transfer(&mut client_2, &mut server);
    server.process_new_packets().unwrap();
    transfer(&mut server, &mut client_1);
    assert_eq!(
        client_1
            .process_new_packets()
            .unwrap_err(),
        PeerMisbehaved::WrongGroupForKeyShare.into()
    );
}

#[derive(Debug)]
struct FakeHybrid;

impl SupportedKxGroup for FakeHybrid {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        Ok(Box::new(FakeHybridActive))
    }

    fn name(&self) -> NamedGroup {
        NamedGroup::from(0x1234)
    }
}

struct FakeHybridActive;

impl ActiveKeyExchange for FakeHybridActive {
    fn complete(self: Box<Self>, _peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        Err(PeerMisbehaved::InvalidKeyShare.into())
    }

    fn hybrid_component(&self) -> Option<(NamedGroup, &[u8])> {
        Some((provider::kx_group::SECP384R1.name(), b"classical"))
    }

    fn pub_key(&self) -> &[u8] {
        b"hybrid"
    }

    fn group(&self) -> NamedGroup {
        FakeHybrid.name()
    }
}

const CONFIDENTIALITY_LIMIT: u64 = 1024;
