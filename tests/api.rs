// Assorted public API tests.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::mem;
use std::fmt;
use std::env;
use std::error::Error;
use std::io::{self, Write, Read};

use rustls;

use rustls::{ClientConfig, ClientSession, ResolvesClientCert};
use rustls::{ServerConfig, ServerSession, ResolvesServerCert};
use rustls::Session;
use rustls::{Stream, StreamOwned};
use rustls::{ProtocolVersion, SignatureScheme, CipherSuite};
use rustls::TLSError;
use rustls::sign;
use rustls::{ALL_CIPHERSUITES, SupportedCipherSuite};
use rustls::KeyLog;
#[cfg(feature = "quic")]
use rustls::quic::{self, QuicExt, ClientQuicExt, ServerQuicExt};

use webpki;

#[allow(dead_code)]
mod common;
use crate::common::*;

fn alpn_test(server_protos: Vec<Vec<u8>>, client_protos: Vec<Vec<u8>>, agreed: Option<&[u8]>) {
    let mut client_config = make_client_config(KeyType::RSA);
    let mut server_config = make_server_config(KeyType::RSA);

    client_config.alpn_protocols = client_protos;
    server_config.alpn_protocols = server_protos;

    let server_config = Arc::new(server_config);

    for client_config in AllClientVersions::new(client_config) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(client.get_alpn_protocol(), None);
        assert_eq!(server.get_alpn_protocol(), None);
        do_handshake(&mut client, &mut server);
        assert_eq!(client.get_alpn_protocol(), agreed);
        assert_eq!(server.get_alpn_protocol(), agreed);
    }
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
    alpn_test(vec![b"server-proto".to_vec()],
              vec![b"client-proto".to_vec()],
              None);

    // server chooses preference
    alpn_test(vec![b"server-proto".to_vec(), b"client-proto".to_vec()],
              vec![b"client-proto".to_vec(), b"server-proto".to_vec()],
              Some(b"server-proto"));

    // case sensitive
    alpn_test(vec![b"PROTO".to_vec()], vec![b"proto".to_vec()], None);
}

fn version_test(client_versions: Vec<ProtocolVersion>,
                server_versions: Vec<ProtocolVersion>,
                result: Option<ProtocolVersion>) {
    let mut client_config = make_client_config(KeyType::RSA);
    let mut server_config = make_server_config(KeyType::RSA);

    println!("version {:?} {:?} -> {:?}",
             client_versions,
             server_versions,
             result);

    if !client_versions.is_empty() {
        client_config.versions = client_versions;
    }

    if !server_versions.is_empty() {
        server_config.versions = server_versions;
    }

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(client.get_protocol_version(), None);
    assert_eq!(server.get_protocol_version(), None);
    if result.is_none() {
        let err = do_handshake_until_error(&mut client, &mut server);
        assert_eq!(err.is_err(), true);
    } else {
        do_handshake(&mut client, &mut server);
        assert_eq!(client.get_protocol_version(), result);
        assert_eq!(server.get_protocol_version(), result);
    }
}

#[test]
fn versions() {
    // default -> 1.3
    version_test(vec![], vec![], Some(ProtocolVersion::TLSv1_3));

    // client default, server 1.2 -> 1.2
    version_test(vec![],
                 vec![ProtocolVersion::TLSv1_2],
                 Some(ProtocolVersion::TLSv1_2));

    // client 1.2, server default -> 1.2
    version_test(vec![ProtocolVersion::TLSv1_2],
                 vec![],
                 Some(ProtocolVersion::TLSv1_2));

    // client 1.2, server 1.3 -> fail
    version_test(vec![ProtocolVersion::TLSv1_2],
                 vec![ProtocolVersion::TLSv1_3],
                 None);

    // client 1.3, server 1.2 -> fail
    version_test(vec![ProtocolVersion::TLSv1_3],
                 vec![ProtocolVersion::TLSv1_2],
                 None);

    // client 1.3, server 1.2+1.3 -> 1.3
    version_test(vec![ProtocolVersion::TLSv1_3],
                 vec![ProtocolVersion::TLSv1_2, ProtocolVersion::TLSv1_3],
                 Some(ProtocolVersion::TLSv1_3));

    // client 1.2+1.3, server 1.2 -> 1.2
    version_test(vec![ProtocolVersion::TLSv1_3, ProtocolVersion::TLSv1_2],
                 vec![ProtocolVersion::TLSv1_2],
                 Some(ProtocolVersion::TLSv1_2));
}

fn check_read(reader: &mut dyn io::Read, bytes: &[u8]) {
    let mut buf = Vec::new();
    assert_eq!(bytes.len(), reader.read_to_end(&mut buf).unwrap());
    assert_eq!(bytes.to_vec(), buf);
}

#[test]
fn buffered_client_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::RSA));

    for client_config in AllClientVersions::new(make_client_config(KeyType::RSA)) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(5, client.write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut server, b"hello");
    }
}

#[test]
fn buffered_server_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::RSA));

    for client_config in AllClientVersions::new(make_client_config(KeyType::RSA)) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(5, server.write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();

        check_read(&mut client, b"hello");
    }
}

#[test]
fn buffered_both_data_sent() {
    let server_config = Arc::new(make_server_config(KeyType::RSA));

    for client_config in AllClientVersions::new(make_client_config(KeyType::RSA)) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(12, server.write(b"from-server!").unwrap());
        assert_eq!(12, client.write(b"from-client!").unwrap());

        do_handshake(&mut client, &mut server);

        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();

        check_read(&mut client, b"from-server!");
        check_read(&mut server, b"from-client!");
    }
}

#[test]
fn client_can_get_server_cert() {
    for kt in ALL_KEY_TYPES.iter() {
        for client_config in AllClientVersions::new(make_client_config(*kt)) {
            let (mut client, mut server) = make_pair_for_configs(client_config,
                                                                 make_server_config(*kt));
            do_handshake(&mut client, &mut server);

            let certs = client.get_peer_certificates();
            assert_eq!(certs, Some(kt.get_chain()));
        }
    }
}

#[test]
fn server_can_get_client_cert() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.set_single_client_cert(kt.get_chain(), kt.get_key());

        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for client_config in AllClientVersions::new(client_config) {
            let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                     &server_config);
            do_handshake(&mut client, &mut server);

            let certs = server.get_peer_certificates();
            assert_eq!(certs, Some(kt.get_chain()));
        }
    }
}

fn check_read_and_close(reader: &mut dyn io::Read, expect: &[u8]) {
    let mut buf = Vec::new();
    buf.resize(expect.len(), 0u8);
    assert_eq!(expect.len(), reader.read(&mut buf).unwrap());
    assert_eq!(expect.to_vec(), buf);

    let err = reader.read(&mut buf);
    assert!(err.is_err());
    assert_eq!(err.err().unwrap().kind(), io::ErrorKind::ConnectionAborted);
}

#[test]
fn server_close_notify() {
    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.set_single_client_cert(kt.get_chain(), kt.get_key());

    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt));

    for client_config in AllClientVersions::new(client_config) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);
        do_handshake(&mut client, &mut server);

        // check that alerts don't overtake appdata
        assert_eq!(12, server.write(b"from-server!").unwrap());
        assert_eq!(12, client.write(b"from-client!").unwrap());
        server.send_close_notify();

        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        check_read_and_close(&mut client, b"from-server!");

        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read(&mut server, b"from-client!");
    }
}

#[test]
fn client_close_notify() {
    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.set_single_client_cert(kt.get_chain(), kt.get_key());

    let server_config = Arc::new(make_server_config_with_mandatory_client_auth(kt));

    for client_config in AllClientVersions::new(client_config) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);
        do_handshake(&mut client, &mut server);

        // check that alerts don't overtake appdata
        assert_eq!(12, server.write(b"from-server!").unwrap());
        assert_eq!(12, client.write(b"from-client!").unwrap());
        client.send_close_notify();

        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
        check_read_and_close(&mut server, b"from-client!");

        transfer(&mut server, &mut client);
        client.process_new_packets().unwrap();
        check_read(&mut client, b"from-server!");
    }
}

struct ServerCheckCertResolve {
    expected: String
}

impl ServerCheckCertResolve {
    fn new(expect: &str) -> ServerCheckCertResolve {
        ServerCheckCertResolve {
            expected: expect.to_string()
        }
    }
}

impl ResolvesServerCert for ServerCheckCertResolve {
    fn resolve(&self,
               server_name: Option<webpki::DNSNameRef<'_>>,
               sigschemes: &[SignatureScheme])
        -> Option<sign::CertifiedKey> {
        if let Some(got_dns_name) = server_name {
            let got: &str = got_dns_name.into();
            if got != self.expected {
                panic!("unexpected dns name (wanted '{}' got '{:?}')", &self.expected, got_dns_name);
            }
        } else {
            panic!("dns name not provided (wanted '{}')", &self.expected);
        }

        if sigschemes.len() == 0 {
            panic!("no signature schemes shared by client");
        }

        None
    }
}

#[test]
fn server_cert_resolve_with_sni() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config(*kt);
        let mut server_config = make_server_config(*kt);

        server_config.cert_resolver = Arc::new(ServerCheckCertResolve::new("the-value-from-sni"));

        let mut client = ClientSession::new(&Arc::new(client_config), dns_name("the-value-from-sni"));
        let mut server = ServerSession::new(&Arc::new(server_config));

        let err = do_handshake_until_error(&mut client, &mut server);
        assert_eq!(err.is_err(), true);
    }
}

struct ServerCheckNoSNI {}

impl ResolvesServerCert for ServerCheckNoSNI {
    fn resolve(&self,
               server_name: Option<webpki::DNSNameRef<'_>>,
               _sigschemes: &[SignatureScheme])
        -> Option<sign::CertifiedKey> {
        assert!(server_name.is_none());

        None
    }
}

#[test]
fn client_with_sni_disabled_does_not_send_sni() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.enable_sni = false;

        let mut server_config = make_server_config(*kt);
        server_config.cert_resolver = Arc::new(ServerCheckNoSNI {});
        let server_config = Arc::new(server_config);

        for client_config in AllClientVersions::new(client_config) {
            let mut client = ClientSession::new(&Arc::new(client_config), dns_name("value-not-sent"));
            let mut server = ServerSession::new(&server_config);

            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(err.is_err(), true);
        }
    }
}

#[test]
fn client_checks_server_certificate_with_given_name() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config(*kt);
        let server_config = Arc::new(make_server_config(*kt));

        for client_config in AllClientVersions::new(client_config) {
            let mut client = ClientSession::new(&Arc::new(client_config),
                                                dns_name("not-the-right-hostname.com"));
            let mut server = ServerSession::new(&server_config);

            let err = do_handshake_until_error(&mut client, &mut server);
            assert_eq!(err,
                       Err(TLSErrorFromPeer::Client(
                               TLSError::WebPKIError(webpki::Error::CertNotValidForName))
                           )
                       );
        }
    }
}

struct ClientCheckCertResolve {
    query_count: AtomicUsize,
    expect_queries: usize
}

impl ClientCheckCertResolve {
    fn new(expect_queries: usize) -> ClientCheckCertResolve {
        ClientCheckCertResolve {
            query_count: AtomicUsize::new(0),
            expect_queries: expect_queries
        }
    }
}

impl Drop for ClientCheckCertResolve {
    fn drop(&mut self) {
        let count = self.query_count.load(Ordering::SeqCst);
        assert_eq!(count, self.expect_queries);
    }
}

impl ResolvesClientCert for ClientCheckCertResolve {
    fn resolve(&self,
               acceptable_issuers: &[&[u8]],
               sigschemes: &[SignatureScheme])
        -> Option<sign::CertifiedKey> {
        self.query_count.fetch_add(1, Ordering::SeqCst);

        if acceptable_issuers.len() == 0 {
            panic!("no issuers offered by server");
        }

        if sigschemes.len() == 0 {
            panic!("no signature schemes shared by server");
        }

        None
    }

    fn has_certs(&self) -> bool {
        true
    }
}

#[test]
fn client_cert_resolve() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.client_auth_cert_resolver = Arc::new(ClientCheckCertResolve::new(2));

        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for client_config in AllClientVersions::new(client_config) {
            let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                     &server_config);

            assert_eq!(
                do_handshake_until_error(&mut client, &mut server),
                Err(TLSErrorFromPeer::Server(TLSError::NoCertificatesPresented)));
        }
    }
}

#[test]
fn client_auth_works() {
    for kt in ALL_KEY_TYPES.iter() {
        let client_config = make_client_config_with_auth(*kt);
        let server_config = Arc::new(make_server_config_with_mandatory_client_auth(*kt));

        for client_config in AllClientVersions::new(client_config) {
            let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                     &server_config);
            do_handshake(&mut client, &mut server);
        }
    }
}

#[test]
fn client_error_is_sticky() {
    let (mut client, _) = make_pair(KeyType::RSA);
    client.read_tls(&mut b"\x16\x03\x03\x00\x08\x0f\x00\x00\x04junk".as_ref()).unwrap();
    let mut err = client.process_new_packets();
    assert_eq!(err.is_err(), true);
    err = client.process_new_packets();
    assert_eq!(err.is_err(), true);
}

#[test]
fn server_error_is_sticky() {
    let (_, mut server) = make_pair(KeyType::RSA);
    server.read_tls(&mut b"\x16\x03\x03\x00\x08\x0f\x00\x00\x04junk".as_ref()).unwrap();
    let mut err = server.process_new_packets();
    assert_eq!(err.is_err(), true);
    err = server.process_new_packets();
    assert_eq!(err.is_err(), true);
}

#[test]
fn server_is_send_and_sync() {
    let (_, server) = make_pair(KeyType::RSA);
    &server as &dyn Send;
    &server as &dyn Sync;
}

#[test]
fn client_is_send_and_sync() {
    let (client, _) = make_pair(KeyType::RSA);
    &client as &dyn Send;
    &client as &dyn Sync;
}

#[test]
fn server_respects_buffer_limit_pre_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    server.set_buffer_limit(32);

    assert_eq!(server.write(b"01234567890123456789").unwrap(), 20);
    assert_eq!(server.write(b"01234567890123456789").unwrap(), 12);

    do_handshake(&mut client, &mut server);
    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    check_read(&mut client, b"01234567890123456789012345678901");
}

#[test]
fn server_respects_buffer_limit_post_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    // this test will vary in behaviour depending on the default suites
    do_handshake(&mut client, &mut server);
    server.set_buffer_limit(48);

    assert_eq!(server.write(b"01234567890123456789").unwrap(), 20);
    assert_eq!(server.write(b"01234567890123456789").unwrap(), 6);

    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    check_read(&mut client, b"01234567890123456789012345");
}

#[test]
fn client_respects_buffer_limit_pre_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    client.set_buffer_limit(32);

    assert_eq!(client.write(b"01234567890123456789").unwrap(), 20);
    assert_eq!(client.write(b"01234567890123456789").unwrap(), 12);

    do_handshake(&mut client, &mut server);
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server, b"01234567890123456789012345678901");
}

#[test]
fn client_respects_buffer_limit_post_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    do_handshake(&mut client, &mut server);
    client.set_buffer_limit(48);

    assert_eq!(client.write(b"01234567890123456789").unwrap(), 20);
    assert_eq!(client.write(b"01234567890123456789").unwrap(), 6);

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    check_read(&mut server, b"01234567890123456789012345");
}

struct OtherSession<'a> {
    sess: &'a mut dyn Session,
    pub reads: usize,
    pub writes: usize,
    pub writevs: Vec<Vec<usize>>,
    fail_ok: bool,
    pub short_writes: bool,
    pub last_error: Option<rustls::TLSError>,
}

impl<'a> OtherSession<'a> {
    fn new(sess: &'a mut dyn Session) -> OtherSession<'a> {
        OtherSession {
            sess,
            reads: 0,
            writes: 0,
            writevs: vec![],
            fail_ok: false,
            short_writes: false,
            last_error: None,
        }
    }

    fn new_fails(sess: &'a mut dyn Session) -> OtherSession<'a> {
        let mut os = OtherSession::new(sess);
        os.fail_ok = true;
        os
    }
}

impl<'a> io::Read for OtherSession<'a> {
    fn read(&mut self, mut b: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        self.sess.write_tls(b.by_ref())
    }
}

impl<'a> io::Write for OtherSession<'a> {
    fn write(&mut self, mut b: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        let l = self.sess.read_tls(b.by_ref())?;
        let rc = self.sess.process_new_packets();

        if !self.fail_ok {
            rc.unwrap();
        } else if rc.is_err() {
            self.last_error = rc.err();
        }

        Ok(l)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> rustls::WriteV for OtherSession<'a> {
    fn writev(&mut self, b: &[&[u8]]) -> io::Result<usize> {
        let mut total = 0;
        let mut lengths = vec![];
        for bytes in b {
            let write_len = if self.short_writes {
                if bytes.len() > 5 { bytes.len() / 2 } else { bytes.len() }
            } else {
                bytes.len()
            };

            let l = self.sess.read_tls(&mut io::Cursor::new(&bytes[..write_len]))?;
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

#[test]
fn client_complete_io_for_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    assert_eq!(true, client.is_handshaking());
    let (rdlen, wrlen) = client.complete_io(&mut OtherSession::new(&mut server)).unwrap();
    assert!(rdlen > 0 && wrlen > 0);
    assert_eq!(false, client.is_handshaking());
}

#[test]
fn client_complete_io_for_handshake_eof() {
    let (mut client, _) = make_pair(KeyType::RSA);
    let mut input = io::Cursor::new(Vec::new());

    assert_eq!(true, client.is_handshaking());
    let err = client.complete_io(&mut input).unwrap_err();
    assert_eq!(io::ErrorKind::UnexpectedEof, err.kind());
}

#[test]
fn client_complete_io_for_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        do_handshake(&mut client, &mut server);

        client.write(b"01234567890123456789").unwrap();
        client.write(b"01234567890123456789").unwrap();
        {
            let mut pipe = OtherSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            assert_eq!(pipe.writes, 2);
        }
        check_read(&mut server, b"0123456789012345678901234567890123456789");
    }
}

#[test]
fn client_complete_io_for_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        do_handshake(&mut client, &mut server);

        server.write(b"01234567890123456789").unwrap();
        {
            let mut pipe = OtherSession::new(&mut server);
            let (rdlen, wrlen) = client.complete_io(&mut pipe).unwrap();
            assert!(rdlen > 0 && wrlen == 0);
            assert_eq!(pipe.reads, 1);
        }
        check_read(&mut client, b"01234567890123456789");
    }
}

#[test]
fn server_complete_io_for_handshake() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        assert_eq!(true, server.is_handshaking());
        let (rdlen, wrlen) = server.complete_io(&mut OtherSession::new(&mut client)).unwrap();
        assert!(rdlen > 0 && wrlen > 0);
        assert_eq!(false, server.is_handshaking());
    }
}

#[test]
fn server_complete_io_for_handshake_eof() {
    let (_, mut server) = make_pair(KeyType::RSA);
    let mut input = io::Cursor::new(Vec::new());

    assert_eq!(true, server.is_handshaking());
    let err = server.complete_io(&mut input).unwrap_err();
    assert_eq!(io::ErrorKind::UnexpectedEof, err.kind());
}

#[test]
fn server_complete_io_for_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        do_handshake(&mut client, &mut server);

        server.write(b"01234567890123456789").unwrap();
        server.write(b"01234567890123456789").unwrap();
        {
            let mut pipe = OtherSession::new(&mut client);
            let (rdlen, wrlen) = server.complete_io(&mut pipe).unwrap();
            assert!(rdlen == 0 && wrlen > 0);
            assert_eq!(pipe.writes, 2);
        }
        check_read(&mut client, b"0123456789012345678901234567890123456789");
    }
}

#[test]
fn server_complete_io_for_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        do_handshake(&mut client, &mut server);

        client.write(b"01234567890123456789").unwrap();
        {
            let mut pipe = OtherSession::new(&mut client);
            let (rdlen, wrlen) = server.complete_io(&mut pipe).unwrap();
            assert!(rdlen > 0 && wrlen == 0);
            assert_eq!(pipe.reads, 1);
        }
        check_read(&mut server, b"01234567890123456789");
    }
}

#[test]
fn client_stream_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        {
            let mut pipe = OtherSession::new(&mut server);
            let mut stream = Stream::new(&mut client, &mut pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut server, b"hello");
    }
}

#[test]
fn client_streamowned_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (client, mut server) = make_pair(*kt);

        {
            let pipe = OtherSession::new(&mut server);
            let mut stream = StreamOwned::new(client, pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut server, b"hello");
    }
}

#[test]
fn client_stream_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        server.write(b"world").unwrap();

        {
            let mut pipe = OtherSession::new(&mut server);
            let mut stream = Stream::new(&mut client, &mut pipe);
            check_read(&mut stream, b"world");
        }
    }
}

#[test]
fn client_streamowned_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (client, mut server) = make_pair(*kt);

        server.write(b"world").unwrap();

        {
            let pipe = OtherSession::new(&mut server);
            let mut stream = StreamOwned::new(client, pipe);
            check_read(&mut stream, b"world");
        }
    }
}

#[test]
fn server_stream_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        {
            let mut pipe = OtherSession::new(&mut client);
            let mut stream = Stream::new(&mut server, &mut pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut client, b"hello");
    }
}

#[test]
fn server_streamowned_write() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, server) = make_pair(*kt);

        {
            let pipe = OtherSession::new(&mut client);
            let mut stream = StreamOwned::new(server, pipe);
            assert_eq!(stream.write(b"hello").unwrap(), 5);
        }
        check_read(&mut client, b"hello");
    }
}

#[test]
fn server_stream_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, mut server) = make_pair(*kt);

        client.write(b"world").unwrap();

        {
            let mut pipe = OtherSession::new(&mut client);
            let mut stream = Stream::new(&mut server, &mut pipe);
            check_read(&mut stream, b"world");
        }
    }
}

#[test]
fn server_streamowned_read() {
    for kt in ALL_KEY_TYPES.iter() {
        let (mut client, server) = make_pair(*kt);

        client.write(b"world").unwrap();

        {
            let pipe = OtherSession::new(&mut client);
            let mut stream = StreamOwned::new(server, pipe);
            check_read(&mut stream, b"world");
        }
    }
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
    let (mut client, mut server) = make_pair(KeyType::RSA);
    do_handshake(&mut client, &mut server);

    let mut pipe = FailsWrites {
        errkind: io::ErrorKind::WouldBlock,
        after: 0,
    };
    client.write(b"hello").unwrap();
    let mut client_stream = Stream::new(&mut client, &mut pipe);
    let rc = client_stream.write(b"world");
    assert!(rc.is_err());
    let err = rc.err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(err.description(), "oops");
}

#[test]
fn stream_write_swallows_underlying_io_error_after_plaintext_processed() {
    let (mut client, mut server) = make_pair(KeyType::RSA);
    do_handshake(&mut client, &mut server);

    let mut pipe = FailsWrites {
        errkind: io::ErrorKind::WouldBlock,
        after: 1,
    };
    client.write(b"hello").unwrap();
    let mut client_stream = Stream::new(&mut client, &mut pipe);
    let rc = client_stream.write(b"world");
    assert_eq!(format!("{:?}", rc), "Ok(5)");
}

fn make_disjoint_suite_configs() -> (ClientConfig, ServerConfig) {
    let kt = KeyType::RSA;
    let mut server_config = make_server_config(kt);
    server_config.ciphersuites = vec![find_suite(CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256)];

    let mut client_config = make_client_config(kt);
    client_config.ciphersuites = vec![find_suite(CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384)];

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
        assert_eq!(format!("{:?}", rc),
                   "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })");
        let rc = client_stream.write(b"hello");
        assert!(rc.is_err());
        assert_eq!(format!("{:?}", rc),
                   "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })");

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
    assert_eq!(format!("{:?}", rc),
               "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })");
    let rc = client_stream.write(b"hello");
    assert!(rc.is_err());
    assert_eq!(format!("{:?}", rc),
               "Err(Custom { kind: InvalidData, error: AlertReceived(HandshakeFailure) })");
}

#[test]
fn server_stream_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    client.write(b"world").unwrap();

    {
        let mut pipe = OtherSession::new_fails(&mut client);
        let mut server_stream = Stream::new(&mut server, &mut pipe);
        let mut bytes = [0u8; 5];
        let rc = server_stream.read(&mut bytes);
        assert!(rc.is_err());
        assert_eq!(format!("{:?}", rc),
                   "Err(Custom { kind: InvalidData, error: PeerIncompatibleError(\"no ciphersuites in common\") })");
    }
}

#[test]
fn server_streamowned_handshake_error() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, server) = make_pair_for_configs(client_config, server_config);

    client.write(b"world").unwrap();

    let pipe = OtherSession::new_fails(&mut client);
    let mut server_stream = StreamOwned::new(server, pipe);
    let mut bytes = [0u8; 5];
    let rc = server_stream.read(&mut bytes);
    assert!(rc.is_err());
    assert_eq!(format!("{:?}", rc),
               "Err(Custom { kind: InvalidData, error: PeerIncompatibleError(\"no ciphersuites in common\") })");
}

#[test]
fn server_config_is_clone() {
    let _ = make_server_config(KeyType::RSA).clone();
}

#[test]
fn client_config_is_clone() {
    let _ = make_client_config(KeyType::RSA).clone();
}

#[test]
fn client_session_is_debug() {
    let (client, _) = make_pair(KeyType::RSA);
    println!("{:?}", client);
}

#[test]
fn server_session_is_debug() {
    let (_, server) = make_pair(KeyType::RSA);
    println!("{:?}", server);
}

#[test]
fn server_complete_io_for_handshake_ending_with_alert() {
    let (client_config, server_config) = make_disjoint_suite_configs();
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(true, server.is_handshaking());

    let mut pipe = OtherSession::new_fails(&mut client);
    let rc = server.complete_io(&mut pipe);
    assert!(rc.is_err(),
            "server io failed due to handshake failure");
    assert!(!server.wants_write(),
            "but server did send its alert");
    assert_eq!(format!("{:?}", pipe.last_error),
               "Some(AlertReceived(HandshakeFailure))",
               "which was received by client");
}

#[test]
fn server_exposes_offered_sni() {
    let kt = KeyType::RSA;
    let mut client = ClientSession::new(&Arc::new(make_client_config(kt)),
                                        dns_name("second.testserver.com"));
    let mut server = ServerSession::new(&Arc::new(make_server_config(kt)));

    assert_eq!(None, server.get_sni_hostname());
    do_handshake(&mut client, &mut server);
    assert_eq!(Some("second.testserver.com"), server.get_sni_hostname());
}

#[test]
fn sni_resolver_works() {
    let kt = KeyType::RSA;
    let mut resolver = rustls::ResolvesServerCertUsingSNI::new();
    let signing_key = sign::RSASigningKey::new(&kt.get_key())
        .unwrap();
    let signing_key: Arc<Box<dyn sign::SigningKey>> = Arc::new(Box::new(signing_key));
    resolver.add("localhost",
                 sign::CertifiedKey::new(kt.get_chain(), signing_key.clone()))
        .unwrap();

    let mut server_config = make_server_config(kt);
    server_config.cert_resolver = Arc::new(resolver);
    let server_config = Arc::new(server_config);

    let mut server1 = ServerSession::new(&server_config);
    let mut client1 = ClientSession::new(&Arc::new(make_client_config(kt)), dns_name("localhost"));
    let err = do_handshake_until_error(&mut client1, &mut server1);
    assert_eq!(err, Ok(()));

    let mut server2 = ServerSession::new(&server_config);
    let mut client2 = ClientSession::new(&Arc::new(make_client_config(kt)), dns_name("notlocalhost"));
    let err = do_handshake_until_error(&mut client2, &mut server2);
    assert_eq!(err,
               Err(TLSErrorFromPeer::Server(
                       TLSError::General("no server certificate chain resolved".into()))));
}

#[test]
fn sni_resolver_rejects_wrong_names() {
    let kt = KeyType::RSA;
    let mut resolver = rustls::ResolvesServerCertUsingSNI::new();
    let signing_key = sign::RSASigningKey::new(&kt.get_key())
        .unwrap();
    let signing_key: Arc<Box<dyn sign::SigningKey>> = Arc::new(Box::new(signing_key));

    assert_eq!(Ok(()),
               resolver.add("localhost",
                            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone())));
    assert_eq!(Err(TLSError::General("The server certificate is not valid for the given name".into())),
               resolver.add("not-localhost",
                            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone())));
    assert_eq!(Err(TLSError::General("Bad DNS name".into())),
               resolver.add("not ascii 🦀",
                            sign::CertifiedKey::new(kt.get_chain(), signing_key.clone())));
}

#[test]
fn sni_resolver_rejects_bad_certs() {
    let kt = KeyType::RSA;
    let mut resolver = rustls::ResolvesServerCertUsingSNI::new();
    let signing_key = sign::RSASigningKey::new(&kt.get_key())
        .unwrap();
    let signing_key: Arc<Box<dyn sign::SigningKey>> = Arc::new(Box::new(signing_key));

    assert_eq!(Err(TLSError::General("No end-entity certificate in certificate chain".into())),
               resolver.add("localhost",
                            sign::CertifiedKey::new(vec![], signing_key.clone())));

    let bad_chain = vec![ rustls::Certificate(vec![ 0xa0 ]) ];
    assert_eq!(Err(TLSError::General("End-entity certificate in certificate chain is syntactically invalid".into())),
               resolver.add("localhost",
                            sign::CertifiedKey::new(bad_chain, signing_key.clone())));
}

fn do_exporter_test(client_config: ClientConfig, server_config: ServerConfig) {
    let mut client_secret = [0u8; 64];
    let mut server_secret = [0u8; 64];

    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(Err(TLSError::HandshakeNotComplete),
               client.export_keying_material(&mut client_secret, b"label", Some(b"context")));
    assert_eq!(Err(TLSError::HandshakeNotComplete),
               server.export_keying_material(&mut server_secret, b"label", Some(b"context")));
    do_handshake(&mut client, &mut server);

    assert_eq!(Ok(()),
               client.export_keying_material(&mut client_secret, b"label", Some(b"context")));
    assert_eq!(Ok(()),
               server.export_keying_material(&mut server_secret, b"label", Some(b"context")));
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());

    assert_eq!(Ok(()),
               client.export_keying_material(&mut client_secret, b"label", None));
    assert_ne!(client_secret.to_vec(), server_secret.to_vec());
    assert_eq!(Ok(()),
               server.export_keying_material(&mut server_secret, b"label", None));
    assert_eq!(client_secret.to_vec(), server_secret.to_vec());
}

#[test]
fn test_tls12_exporter() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        let server_config = make_server_config(*kt);
        client_config.versions = vec![ ProtocolVersion::TLSv1_2 ];

        do_exporter_test(client_config, server_config);
    }
}

#[test]
fn test_tls13_exporter() {
    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        let server_config = make_server_config(*kt);
        client_config.versions = vec![ ProtocolVersion::TLSv1_3 ];

        do_exporter_test(client_config, server_config);
    }
}

fn do_suite_test(client_config: ClientConfig,
                 server_config: ServerConfig,
                 expect_suite: &'static SupportedCipherSuite,
                 expect_version: ProtocolVersion) {
    println!("do_suite_test {:?} {:?}", expect_version, expect_suite.suite);
    let (mut client, mut server) = make_pair_for_configs(client_config, server_config);

    assert_eq!(None, client.get_negotiated_ciphersuite());
    assert_eq!(None, server.get_negotiated_ciphersuite());
    assert_eq!(None, client.get_protocol_version());
    assert_eq!(None, server.get_protocol_version());
    assert_eq!(true, client.is_handshaking());
    assert_eq!(true, server.is_handshaking());

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();

    assert_eq!(true, client.is_handshaking());
    assert_eq!(true, server.is_handshaking());
    assert_eq!(None, client.get_protocol_version());
    assert_eq!(Some(expect_version), server.get_protocol_version());
    assert_eq!(None, client.get_negotiated_ciphersuite());
    assert_eq!(Some(expect_suite), server.get_negotiated_ciphersuite());

    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    assert_eq!(Some(expect_suite), client.get_negotiated_ciphersuite());
    assert_eq!(Some(expect_suite), server.get_negotiated_ciphersuite());

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    assert_eq!(false, client.is_handshaking());
    assert_eq!(false, server.is_handshaking());
    assert_eq!(Some(expect_version), client.get_protocol_version());
    assert_eq!(Some(expect_version), server.get_protocol_version());
    assert_eq!(Some(expect_suite), client.get_negotiated_ciphersuite());
    assert_eq!(Some(expect_suite), server.get_negotiated_ciphersuite());
}

fn find_suite(suite: CipherSuite) -> &'static SupportedCipherSuite {
    for scs in ALL_CIPHERSUITES.iter() {
        if scs.suite == suite {
            return scs;
        }
    }

    panic!("find_suite given unsuppported suite");
}

static TEST_CIPHERSUITES: [(ProtocolVersion, KeyType, CipherSuite); 9] = [
    (ProtocolVersion::TLSv1_3, KeyType::RSA, CipherSuite::TLS13_CHACHA20_POLY1305_SHA256),
    (ProtocolVersion::TLSv1_3, KeyType::RSA, CipherSuite::TLS13_AES_256_GCM_SHA384),
    (ProtocolVersion::TLSv1_3, KeyType::RSA, CipherSuite::TLS13_AES_128_GCM_SHA256),
    (ProtocolVersion::TLSv1_2, KeyType::ECDSA, CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256),
    (ProtocolVersion::TLSv1_2, KeyType::RSA,   CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256),
    (ProtocolVersion::TLSv1_2, KeyType::ECDSA, CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384),
    (ProtocolVersion::TLSv1_2, KeyType::ECDSA, CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256),
    (ProtocolVersion::TLSv1_2, KeyType::RSA,   CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384),
    (ProtocolVersion::TLSv1_2, KeyType::RSA,   CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)
];

#[test]
fn negotiated_ciphersuite_default() {
    for kt in ALL_KEY_TYPES.iter() {
        do_suite_test(make_client_config(*kt),
                      make_server_config(*kt),
                      find_suite(CipherSuite::TLS13_CHACHA20_POLY1305_SHA256),
                      ProtocolVersion::TLSv1_3);
    }
}

#[test]
fn all_suites_covered() {
    assert_eq!(ALL_CIPHERSUITES.len(), TEST_CIPHERSUITES.len());
}

#[test]
fn negotiated_ciphersuite_client() {
    for item in TEST_CIPHERSUITES.iter() {
        let (version, kt, suite) = *item;
        let scs = find_suite(suite);
        let mut client_config = make_client_config(kt);
        client_config.ciphersuites = vec![scs];
        client_config.versions = vec![version];

        do_suite_test(client_config,
                      make_server_config(kt),
                      scs,
                      version);
    }
}

#[test]
fn negotiated_ciphersuite_server() {
    for item in TEST_CIPHERSUITES.iter() {
        let (version, kt, suite) = *item;
        let scs = find_suite(suite);
        let mut server_config = make_server_config(kt);
        server_config.ciphersuites = vec![scs];
        server_config.versions = vec![version];

        do_suite_test(make_client_config(kt),
                      server_config,
                      scs,
                      version);
    }
}

#[derive(Debug, PartialEq)]
struct KeyLogItem {
    label: String,
    client_random: Vec<u8>,
    secret: Vec<u8>,
}

struct KeyLogToVec(Mutex<Vec<KeyLogItem>>);

impl KeyLogToVec {
    fn new() -> Self {
        KeyLogToVec(Mutex::new(vec![]))
    }

    fn take(&self) -> Vec<KeyLogItem> {
        mem::replace(&mut self.0.lock()
                         .unwrap(),
                     vec![])
    }
}

impl KeyLog for KeyLogToVec {
    fn log(&self, label: &str, client: &[u8], secret: &[u8]) {
        let value = KeyLogItem {
            label: label.into(),
            client_random: client.into(),
            secret: secret.into()
        };

        self.0.lock()
            .unwrap()
            .push(value);
    }
}

#[test]
fn key_log_for_tls12() {
    let client_key_log = Arc::new(KeyLogToVec::new());
    let server_key_log = Arc::new(KeyLogToVec::new());

    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.versions = vec![ ProtocolVersion::TLSv1_2 ];
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
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
    let client_key_log = Arc::new(KeyLogToVec::new());
    let server_key_log = Arc::new(KeyLogToVec::new());

    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.versions = vec![ ProtocolVersion::TLSv1_3 ];
    client_config.key_log = client_key_log.clone();
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
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
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_full_log[2].label);
    assert_eq!("EXPORTER_SECRET", client_full_log[3].label);
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_full_log[4].label);

    assert_eq!(client_full_log[0], server_full_log[1]);
    assert_eq!(client_full_log[1], server_full_log[0]);
    assert_eq!(client_full_log[2], server_full_log[2]);
    assert_eq!(client_full_log[3], server_full_log[3]);
    assert_eq!(client_full_log[4], server_full_log[4]);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    do_handshake(&mut client, &mut server);

    let client_resume_log = client_key_log.take();
    let server_resume_log = server_key_log.take();

    assert_eq!(5, client_resume_log.len());
    assert_eq!("CLIENT_HANDSHAKE_TRAFFIC_SECRET", client_resume_log[0].label);
    assert_eq!("SERVER_HANDSHAKE_TRAFFIC_SECRET", client_resume_log[1].label);
    assert_eq!("SERVER_TRAFFIC_SECRET_0", client_resume_log[2].label);
    assert_eq!("EXPORTER_SECRET", client_resume_log[3].label);
    assert_eq!("CLIENT_TRAFFIC_SECRET_0", client_resume_log[4].label);

    assert_eq!(client_resume_log[0], server_resume_log[1]);
    assert_eq!(client_resume_log[1], server_resume_log[0]);
    assert_eq!(client_resume_log[2], server_resume_log[2]);
    assert_eq!(client_resume_log[3], server_resume_log[3]);
    assert_eq!(client_resume_log[4], server_resume_log[4]);
}

#[test]
fn vectored_write_for_server_appdata() {
    let (mut client, mut server) = make_pair(KeyType::RSA);
    do_handshake(&mut client, &mut server);

    server.write(b"01234567890123456789").unwrap();
    server.write(b"01234567890123456789").unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.writev_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(&mut client, b"0123456789012345678901234567890123456789");
}

#[test]
fn vectored_write_for_client_appdata() {
    let (mut client, mut server) = make_pair(KeyType::RSA);
    do_handshake(&mut client, &mut server);

    client.write(b"01234567890123456789").unwrap();
    client.write(b"01234567890123456789").unwrap();
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.writev_tls(&mut pipe).unwrap();
        assert_eq!(84, wrlen);
        assert_eq!(pipe.writevs, vec![vec![42, 42]]);
    }
    check_read(&mut server, b"0123456789012345678901234567890123456789");
}

#[test]
fn vectored_write_for_server_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    server.write(b"01234567890123456789").unwrap();
    server.write(b"0123456789").unwrap();

    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.writev_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 4000); // its pretty big (contains cert chain)
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() > 3); // at least a server hello/cert/serverkx
    }

    client.process_new_packets().unwrap();
    transfer(&mut client, &mut server);
    server.process_new_packets().unwrap();
    {
        let mut pipe = OtherSession::new(&mut client);
        let wrlen = server.writev_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 177);
        assert_eq!(pipe.writevs, vec![vec![103, 42, 32]]);
    }

    assert_eq!(server.is_handshaking(), false);
    assert_eq!(client.is_handshaking(), false);
    check_read(&mut client, b"012345678901234567890123456789");
}

#[test]
fn vectored_write_for_client_handshake() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    client.write(b"01234567890123456789").unwrap();
    client.write(b"0123456789").unwrap();
    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.writev_tls(&mut pipe).unwrap();
        // don't assert exact sizes here, to avoid a brittle test
        assert!(wrlen > 200); // just the client hello
        assert_eq!(pipe.writevs.len(), 1); // only one writev
        assert!(pipe.writevs[0].len() == 1); // only a client hello
    }

    transfer(&mut server, &mut client);
    client.process_new_packets().unwrap();

    {
        let mut pipe = OtherSession::new(&mut server);
        let wrlen = client.writev_tls(&mut pipe).unwrap();
        assert_eq!(wrlen, 138);
        // CCS, finished, then two application datas
        assert_eq!(pipe.writevs, vec![vec![6, 58, 42, 32]]);
    }

    assert_eq!(server.is_handshaking(), false);
    assert_eq!(client.is_handshaking(), false);
    check_read(&mut server, b"012345678901234567890123456789");
}

#[test]
fn vectored_write_with_slow_client() {
    let (mut client, mut server) = make_pair(KeyType::RSA);

    client.set_buffer_limit(32);

    do_handshake(&mut client, &mut server);
    server.write(b"01234567890123456789").unwrap();

    {
        let mut pipe = OtherSession::new(&mut client);
        pipe.short_writes = true;
        let wrlen = server.writev_tls(&mut pipe).unwrap() +
            server.writev_tls(&mut pipe).unwrap() +
            server.writev_tls(&mut pipe).unwrap() +
            server.writev_tls(&mut pipe).unwrap() +
            server.writev_tls(&mut pipe).unwrap() +
            server.writev_tls(&mut pipe).unwrap();
        assert_eq!(42, wrlen);
        assert_eq!(pipe.writevs, vec![vec![21], vec![10], vec![5], vec![3], vec![3]]);
    }
    check_read(&mut client, b"01234567890123456789");
}

struct ServerStorage {
    storage: Arc<dyn rustls::StoresServerSessions>,
    put_count: AtomicUsize,
    get_count: AtomicUsize,
    take_count: AtomicUsize,
}

impl ServerStorage {
    fn new() -> ServerStorage {
        ServerStorage {
            storage: rustls::ServerSessionMemoryCache::new(1024),
            put_count: AtomicUsize::new(0),
            get_count: AtomicUsize::new(0),
            take_count: AtomicUsize::new(0),
        }
    }

    fn puts(&self) -> usize { self.put_count.load(Ordering::SeqCst) }
    fn gets(&self) -> usize { self.get_count.load(Ordering::SeqCst) }
    fn takes(&self) -> usize { self.take_count.load(Ordering::SeqCst) }
}

impl fmt::Debug for ServerStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(put: {:?}, get: {:?}, take: {:?})",
               self.put_count, self.get_count, self.take_count)
    }
}

impl rustls::StoresServerSessions for ServerStorage {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> bool {
        self.put_count.fetch_add(1, Ordering::SeqCst);
        self.storage.put(key, value)
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        self.storage.get(key)
    }

    fn take(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.take_count.fetch_add(1, Ordering::SeqCst);
        self.storage.take(key)
    }
}

#[test]
fn tls13_stateful_resumption() {
    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.versions = vec![ ProtocolVersion::TLSv1_3 ];
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(storage.puts(), 1);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server);
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 2);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 1);

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 3);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 2);
}

#[test]
fn tls13_stateless_resumption() {
    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.versions = vec![ ProtocolVersion::TLSv1_3 ];
    let client_config = Arc::new(client_config);

    let mut server_config = make_server_config(kt);
    server_config.ticketer = rustls::Ticketer::new();
    let storage = Arc::new(ServerStorage::new());
    server_config.session_storage = storage.clone();
    let server_config = Arc::new(server_config);

    // full handshake
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (full_c2s, full_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);

    // resumed
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume_c2s, resume_s2c) = do_handshake(&mut client, &mut server);
    assert!(resume_c2s > full_c2s);
    assert!(resume_s2c < full_s2c);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);

    // resumed again
    let (mut client, mut server) = make_pair_for_arc_configs(&client_config, &server_config);
    let (resume2_c2s, resume2_s2c) = do_handshake(&mut client, &mut server);
    assert_eq!(resume_s2c, resume2_s2c);
    assert_eq!(resume_c2s, resume2_c2s);
    assert_eq!(storage.puts(), 0);
    assert_eq!(storage.gets(), 0);
    assert_eq!(storage.takes(), 0);
}

#[test]
#[cfg(feature = "quic")]
fn quic_handshake() {
    // Returns the sender's next secrets to use, or the receiver's error.
    fn step(send: &mut dyn Session, recv: &mut dyn Session) -> Result<Option<quic::Secrets>, TLSError> {
        let mut buf = Vec::new();
        let secrets = loop {
            let prev = buf.len();
            if let Some(x) = send.write_hs(&mut buf) {
                break Some(x);
            }
            if prev == buf.len() {
                break None;
            }
        };
        if let Err(e) = recv.read_hs(&buf) {
            return Err(e);
        } else {
            assert_eq!(recv.get_alert(), None);
        }
        Ok(secrets)
    }

    let kt = KeyType::RSA;
    let mut client_config = make_client_config(kt);
    client_config.versions = vec![ProtocolVersion::TLSv1_3];
    client_config.enable_early_data = true;
    let client_config = Arc::new(client_config);
    let mut server_config = make_server_config(kt);
    server_config.versions = vec![ProtocolVersion::TLSv1_3];
    server_config.max_early_data_size = 0xffffffff;
    server_config.alpn_protocols = vec!["foo".into()];
    let server_config = Arc::new(server_config);
    let client_params = &b"client params"[..];
    let server_params = &b"server params"[..];

    // full handshake
    let mut client =
        ClientSession::new_quic(&client_config, dns_name("localhost"), client_params.into());
    let mut server = ServerSession::new_quic(&server_config, server_params.into());
    let client_initial = step(&mut client, &mut server).unwrap();
    assert!(client_initial.is_none());
    assert!(client.get_early_secret().is_none());
    assert_eq!(server.get_quic_transport_parameters(), Some(client_params));
    let server_hs = step(&mut server, &mut client).unwrap().unwrap();
    assert!(server.get_early_secret().is_none());
    let client_hs = step(&mut client, &mut server).unwrap().unwrap();
    assert_eq!(server_hs, client_hs);
    assert!(client.is_handshaking());
    let server_1rtt = step(&mut server, &mut client).unwrap().unwrap();
    assert!(!client.is_handshaking());
    assert_eq!(client.get_quic_transport_parameters(), Some(server_params));
    assert!(server.is_handshaking());
    let client_1rtt = step(&mut client, &mut server).unwrap().unwrap();
    assert!(!server.is_handshaking());
    assert_eq!(server_1rtt, client_1rtt);
    assert_ne!(server_hs, server_1rtt);
    assert!(step(&mut client, &mut server).unwrap().is_none());
    assert!(step(&mut server, &mut client).unwrap().is_none());

    // key update
    let initial = quic::Secrets {
        // Constant dummy values for reproducibility
        client: vec![
            0xb8, 0x76, 0x77, 0x08, 0xf8, 0x77, 0x23, 0x58, 0xa6, 0xea, 0x9f, 0xc4, 0x3e, 0x4a,
            0xdd, 0x2c, 0x96, 0x1b, 0x3f, 0x52, 0x87, 0xa6, 0xd1, 0x46, 0x7e, 0xe0, 0xae, 0xab,
            0x33, 0x72, 0x4d, 0xbf,
        ],
        server: vec![
            0x42, 0xdc, 0x97, 0x21, 0x40, 0xe0, 0xf2, 0xe3, 0x98, 0x45, 0xb7, 0x67, 0x61, 0x34,
            0x39, 0xdc, 0x67, 0x58, 0xca, 0x43, 0x25, 0x9b, 0x87, 0x85, 0x06, 0x82, 0x4e, 0xb1,
            0xe4, 0x38, 0xd8, 0x55,
        ],
    };
    let updated = client.update_secrets(&initial.client, &initial.server);
    // The expected values will need to be updated if the negotiated hash function changes.
    assert_eq!(
        updated.client,
        &[
            101, 250, 130, 179, 97, 208, 160, 166, 213, 90, 196, 212, 96, 49, 190, 24, 237, 225,
            68, 97, 141, 123, 162, 108, 231, 21, 255, 184, 49, 245, 178, 148
        ]
    );
    assert_eq!(
        updated.server,
        &[
            171, 127, 244, 22, 119, 205, 252, 100, 179, 94, 91, 45, 99, 82, 236, 124, 44, 251, 63,
            57, 94, 215, 175, 138, 178, 161, 97, 80, 51, 250, 107, 85
        ]
    );

    // 0-RTT handshake
    let mut client =
        ClientSession::new_quic(&client_config, dns_name("localhost"), client_params.into());
    let mut server = ServerSession::new_quic(&server_config, server_params.into());
    step(&mut client, &mut server).unwrap();
    assert_eq!(client.get_quic_transport_parameters(), Some(server_params));
    {
        let client_early = client.get_early_secret().unwrap();
        let server_early = server.get_early_secret().unwrap();
        assert_eq!(client_early, server_early);
    }
    step(&mut server, &mut client).unwrap().unwrap();
    step(&mut client, &mut server).unwrap().unwrap();
    step(&mut server, &mut client).unwrap().unwrap();
    assert!(client.is_early_data_accepted());

    // 0-RTT rejection
    {
        let mut client_config = (*client_config).clone();
        client_config.alpn_protocols = vec!["foo".into()];
        let mut client =
            ClientSession::new_quic(&Arc::new(client_config), dns_name("localhost"), client_params.into());
        let mut server = ServerSession::new_quic(&server_config, server_params.into());
        step(&mut client, &mut server).unwrap();
        assert_eq!(client.get_quic_transport_parameters(), Some(server_params));
        assert!(client.get_early_secret().is_some());
        assert!(server.get_early_secret().is_none());
        step(&mut server, &mut client).unwrap().unwrap();
        step(&mut client, &mut server).unwrap().unwrap();
        step(&mut server, &mut client).unwrap().unwrap();
        assert!(!client.is_early_data_accepted());
    }

    // failed handshake
    let mut client = ClientSession::new_quic(
        &client_config,
        dns_name("example.com"),
        client_params.into(),
    );
    let mut server = ServerSession::new_quic(&server_config, server_params.into());
    step(&mut client, &mut server).unwrap();
    step(&mut server, &mut client).unwrap().unwrap();
    step(&mut server, &mut client).unwrap_err();
    assert_eq!(
        client.get_alert(),
        Some(rustls::internal::msgs::enums::AlertDescription::BadCertificate)
    );
}

#[test]
fn test_client_does_not_offer_sha1() {
    use rustls::internal::msgs::{message::Message, message::MessagePayload,
        handshake::HandshakePayload, enums::HandshakeType, codec::Codec};

    for kt in ALL_KEY_TYPES.iter() {
        for client_config in AllClientVersions::new(make_client_config(*kt)) {
            let (mut client, _) = make_pair_for_configs(client_config,
                                                        make_server_config(*kt));

            assert!(client.wants_write());
            let mut buf = [0u8; 262144];
            let sz = client.write_tls(&mut buf.as_mut())
                .unwrap();
            let mut msg = Message::read_bytes(&buf[..sz])
                .unwrap();
            assert!(msg.decode_payload());
            assert!(msg.is_handshake_type(HandshakeType::ClientHello));

            let client_hello = match msg.payload {
                MessagePayload::Handshake(hs) => match hs.payload {
                    HandshakePayload::ClientHello(ch) => ch,
                    _ => unreachable!()
                }
                _ => unreachable!()
            };

            let sigalgs = client_hello.get_sigalgs_extension()
                .unwrap();
            assert_eq!(sigalgs.contains(&SignatureScheme::RSA_PKCS1_SHA1), false,
                       "sha1 unexpectedly offered");
        }
    }
}

#[test]
fn test_client_mtu_reduction() {
    fn collect_write_lengths(client: &mut ClientSession) -> Vec<usize> {
        let mut r = Vec::new();
        let mut buf = [0u8; 128];

        loop {
            let sz = client.write_tls(&mut buf.as_mut())
                .unwrap();
            r.push(sz);
            assert!(sz <= 64);
            if sz < 64 {
                break;
            }
        }

        r
    }

    for kt in ALL_KEY_TYPES.iter() {
        let mut client_config = make_client_config(*kt);
        client_config.set_mtu(&Some(64));

        let mut client = ClientSession::new(&Arc::new(client_config), dns_name("localhost"));
        let writes = collect_write_lengths(&mut client);
        assert!(writes.iter().all(|x| *x <= 64));
        assert!(writes.len() > 1);
    }
}

#[test]
fn exercise_key_log_file_for_client() {
    let server_config = Arc::new(make_server_config(KeyType::RSA));
    let mut client_config = make_client_config(KeyType::RSA);
    env::set_var("SSLKEYLOGFILE", "./sslkeylogfile.txt");
    client_config.key_log = Arc::new(rustls::KeyLogFile::new());

    for client_config in AllClientVersions::new(client_config) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(5, client.write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
    }
}

#[test]
fn exercise_key_log_file_for_server() {
    let mut server_config = make_server_config(KeyType::RSA);

    env::set_var("SSLKEYLOGFILE", "./sslkeylogfile.txt");
    server_config.key_log = Arc::new(rustls::KeyLogFile::new());

    let server_config = Arc::new(server_config);

    for client_config in AllClientVersions::new(make_client_config(KeyType::RSA)) {
        let (mut client, mut server) = make_pair_for_arc_configs(&Arc::new(client_config),
                                                                 &server_config);

        assert_eq!(5, client.write(b"hello").unwrap());

        do_handshake(&mut client, &mut server);
        transfer(&mut client, &mut server);
        server.process_new_packets().unwrap();
    }
}

fn assert_lt(left: usize, right: usize) {
    if left >= right {
        panic!("expected {} < {}", left, right);
    }
}

#[test]
fn session_types_are_not_huge() {
    // Arbitrary sizes
    assert_lt(mem::size_of::<ServerSession>(), 1536);
    assert_lt(mem::size_of::<ClientSession>(), 1536);
}
