use ring;
use std::io::{Read, Write};
use crate::msgs::message::{BorrowMessage, Message, MessagePayload};
use crate::msgs::deframer::MessageDeframer;
use crate::msgs::fragmenter::{MessageFragmenter, MAX_FRAGMENT_LEN};
use crate::msgs::hsjoiner::HandshakeJoiner;
use crate::msgs::base::Payload;
use crate::msgs::codec::Codec;
use crate::msgs::enums::{ContentType, ProtocolVersion, AlertDescription, AlertLevel};
use crate::msgs::enums::KeyUpdateRequest;
use crate::error::TLSError;
use crate::suites::SupportedCipherSuite;
use crate::cipher::{MessageDecrypter, MessageEncrypter, self};
use crate::vecbuf::{ChunkVecBuffer, WriteV};
use crate::key;
use crate::key_schedule::{SecretKind, KeySchedule};
use crate::prf;
use crate::rand;
use crate::quic;
#[cfg(feature = "logging")]
use crate::log::{warn, debug, error};

use std::io;
use std::collections::VecDeque;

/// Generalises `ClientSession` and `ServerSession`
pub trait Session: quic::QuicExt + Read + Write + Send + Sync {
    /// Read TLS content from `rd`.  This method does internal
    /// buffering, so `rd` can supply TLS messages in arbitrary-
    /// sized chunks (like a socket or pipe might).
    ///
    /// You should call `process_new_packets` each time a call to
    /// this function succeeds.
    ///
    /// The returned error only relates to IO on `rd`.  TLS-level
    /// errors are emitted from `process_new_packets`.
    ///
    /// This function returns `Ok(0)` when the underlying `rd` does
    /// so.  This typically happens when a socket is cleanly closed,
    /// or a file is at EOF.
    fn read_tls(&mut self, rd: &mut dyn Read) -> Result<usize, io::Error>;

    /// Writes TLS messages to `wr`.
    ///
    /// On success the function returns `Ok(n)` where `n` is a number
    /// of bytes written to `wr`, number of bytes after encoding and
    /// encryption.
    ///
    /// Note that after function return the session buffer maybe not
    /// yet fully flushed. [`wants_write`] function can be used
    /// to check if output buffer is not empty.
    ///
    /// [`wants_write`]: #tymethod.wants_write
    fn write_tls(&mut self, wr: &mut dyn Write) -> Result<usize, io::Error>;

    /// Like `write_tls`, but writes potentially many records in one
    /// go via `wr`; a `rustls::WriteV`.  This function has the same semantics
    /// as `write_tls` otherwise.
    fn writev_tls(&mut self, wr: &mut dyn WriteV) -> Result<usize, io::Error>;

    /// Processes any new packets read by a previous call to `read_tls`.
    /// Errors from this function relate to TLS protocol errors, and
    /// are fatal to the session.  Future calls after an error will do
    /// no new work and will return the same error.
    ///
    /// Success from this function can mean new plaintext is available:
    /// obtain it using `read`.
    fn process_new_packets(&mut self) -> Result<(), TLSError>;

    /// Returns true if the caller should call `read_tls` as soon
    /// as possible.
    fn wants_read(&self) -> bool;

    /// Returns true if the caller should call `write_tls` as soon
    /// as possible.
    fn wants_write(&self) -> bool;

    /// Returns true if the session is currently perform the TLS
    /// handshake.  During this time plaintext written to the
    /// session is buffered in memory.
    fn is_handshaking(&self) -> bool;

    /// Sets a limit on the internal buffers used to buffer
    /// unsent plaintext (prior to completing the TLS handshake)
    /// and unsent TLS records.
    ///
    /// By default, there is no limit.  The limit can be set
    /// at any time, even if the current buffer use is higher.
    fn set_buffer_limit(&mut self, limit: usize);

    /// Queues a close_notify fatal alert to be sent in the next
    /// `write_tls` call.  This informs the peer that the
    /// connection is being closed.
    fn send_close_notify(&mut self);

    /// Retrieves the certificate chain used by the peer to authenticate.
    ///
    /// For clients, this is the certificate chain of the server.
    ///
    /// For servers, this is the certificate chain of the client,
    /// if client authentication was completed.
    ///
    /// The return value is None until this value is available.
    fn get_peer_certificates(&self) -> Option<Vec<key::Certificate>>;

    /// Retrieves the protocol agreed with the peer via ALPN.
    ///
    /// A return value of None after handshake completion
    /// means no protocol was agreed (because no protocols
    /// were offered or accepted by the peer).
    fn get_alpn_protocol(&self) -> Option<&[u8]>;

    /// Retrieves the protocol version agreed with the peer.
    ///
    /// This returns None until the version is agreed.
    fn get_protocol_version(&self) -> Option<ProtocolVersion>;

    /// Derives key material from the agreed session secrets.
    ///
    /// This function fills in `output` with `output.len()` bytes of key
    /// material derived from the master session secret using `label`
    /// and `context` for diversification.
    ///
    /// See RFC5705 for more details on what this does and is for.
    ///
    /// For TLS1.3 connections, this function does not use the
    /// "early" exporter at any point.
    ///
    /// This function fails if called prior to the handshake completing;
    /// check with `is_handshaking()` first.
    fn export_keying_material(&self,
                              output: &mut [u8],
                              label: &[u8],
                              context: Option<&[u8]>) -> Result<(), TLSError>;

    /// Retrieves the ciphersuite agreed with the peer.
    ///
    /// This returns None until the ciphersuite is agreed.
    fn get_negotiated_ciphersuite(&self) -> Option<&'static SupportedCipherSuite>;

    /// This function uses `io` to complete any outstanding IO for
    /// this session.
    ///
    /// This is a convenience function which solely uses other parts
    /// of the public API.
    ///
    /// What this means depends on the session state:
    ///
    /// - If the session `is_handshaking()`, then IO is performed until
    ///   the handshake is complete.
    /// - Otherwise, if `wants_write` is true, `write_tls` is invoked
    ///   until it is all written.
    /// - Otherwise, if `wants_read` is true, `read_tls` is invoked
    ///   once.
    ///
    /// The return value is the number of bytes read from and written
    /// to `io`, respectively.
    ///
    /// This function will block if `io` blocks.
    ///
    /// Errors from TLS record handling (ie, from `process_new_packets()`)
    /// are wrapped in an `io::ErrorKind::InvalidData`-kind error.
    fn complete_io<T>(&mut self, io: &mut T) -> Result<(usize, usize), io::Error>
        where Self: Sized, T: Read + Write
    {
        let until_handshaked = self.is_handshaking();
        let mut eof = false;
        let mut wrlen = 0;
        let mut rdlen = 0;

        loop {
            while self.wants_write() {
                wrlen += self.write_tls(io)?;
            }

            if !until_handshaked && wrlen > 0 {
                return Ok((rdlen, wrlen));
            }

            if !eof && self.wants_read() {
                match self.read_tls(io)? {
                    0 => eof = true,
                    n => rdlen += n
                }
            }

            match self.process_new_packets() {
                Ok(_) => {},
                Err(e) => {
                    // In case we have an alert to send describing this error,
                    // try a last-gasp write -- but don't predate the primary
                    // error.
                    let _ignored = self.write_tls(io);

                    return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                },
            };

            match (eof, until_handshaked, self.is_handshaking()) {
                (_, true, false) => return Ok((rdlen, wrlen)),
                (_, false, _) => return Ok((rdlen, wrlen)),
                (true, true, true) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                (..) => ()
            }
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum Protocol {
    Tls13,
    #[cfg(feature = "quic")]
    Quic,
}

pub struct Labels {
    pub client_early_traffic_secret: &'static str,
    pub client_handshake_traffic_secret: &'static str,
    pub server_handshake_traffic_secret: &'static str,
    pub client_traffic_secret_0: &'static str,
    pub server_traffic_secret_0: &'static str,
    pub client_random: &'static str,
    pub exporter_secret: &'static str,
}

impl Protocol {
    pub fn labels(self) -> &'static Labels {
        match self {
            Protocol::Tls13 => &Labels {
                client_early_traffic_secret: "CLIENT_EARLY_TRAFFIC_SECRET",
                client_handshake_traffic_secret: "CLIENT_HANDSHAKE_TRAFFIC_SECRET",
                server_handshake_traffic_secret: "SERVER_HANDSHAKE_TRAFFIC_SECRET",
                client_traffic_secret_0: "CLIENT_TRAFFIC_SECRET_0",
                server_traffic_secret_0: "SERVER_TRAFFIC_SECRET_0",
                client_random: "CLIENT_RANDOM",
                exporter_secret: "EXPORTER_SECRET",
            },
            #[cfg(feature = "quic")]
            Protocol::Quic => &Labels {
                client_early_traffic_secret: "QUIC_CLIENT_EARLY_TRAFFIC_SECRET",
                client_handshake_traffic_secret: "QUIC_CLIENT_HANDSHAKE_TRAFFIC_SECRET",
                server_handshake_traffic_secret: "QUIC_SERVER_HANDSHAKE_TRAFFIC_SECRET",
                client_traffic_secret_0: "QUIC_CLIENT_TRAFFIC_SECRET_0",
                server_traffic_secret_0: "QUIC_SERVER_TRAFFIC_SECRET_0",
                client_random: "QUIC_CLIENT_RANDOM",
                exporter_secret: "QUIC_EXPORTER_SECRET",
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRandoms {
    pub we_are_client: bool,
    pub client: [u8; 32],
    pub server: [u8; 32],
}

static TLS12_DOWNGRADE_SENTINEL: &[u8] = &[0x44, 0x4f, 0x57, 0x4e, 0x47, 0x52, 0x44, 0x01];

impl SessionRandoms {
    pub fn for_server() -> SessionRandoms {
        let mut ret = SessionRandoms {
            we_are_client: false,
            client: [0u8; 32],
            server: [0u8; 32],
        };

        rand::fill_random(&mut ret.server);
        ret
    }

    pub fn for_client() -> SessionRandoms {
        let mut ret = SessionRandoms {
            we_are_client: true,
            client: [0u8; 32],
            server: [0u8; 32],
        };

        rand::fill_random(&mut ret.client);
        ret
    }

    pub fn set_tls12_downgrade_marker(&mut self) {
        assert!(!self.we_are_client);
        self.server[24..]
            .as_mut()
            .write_all(TLS12_DOWNGRADE_SENTINEL)
            .unwrap();
    }

    pub fn has_tls12_downgrade_marker(&mut self) -> bool {
        assert!(self.we_are_client);
        // both the server random and TLS12_DOWNGRADE_SENTINEL are
        // public values and don't require constant time comparison
        &self.server[24..] == TLS12_DOWNGRADE_SENTINEL
    }
}

fn join_randoms(first: &[u8], second: &[u8]) -> [u8; 64] {
    let mut randoms = [0u8; 64];
    randoms.as_mut().write_all(first).unwrap();
    randoms[32..].as_mut().write_all(second).unwrap();
    randoms
}

pub struct SessionSecrets {
    pub randoms: SessionRandoms,
    hash: &'static ring::digest::Algorithm,
    pub master_secret: [u8; 48],
}

impl SessionSecrets {
    pub fn new(randoms: &SessionRandoms,
               hashalg: &'static ring::digest::Algorithm,
               pms: &[u8])
               -> SessionSecrets {
        let mut ret = SessionSecrets {
            randoms: randoms.clone(),
            hash: hashalg,
            master_secret: [0u8; 48],
        };

        let randoms = join_randoms(&ret.randoms.client, &ret.randoms.server);
        prf::prf(&mut ret.master_secret,
                 ret.hash,
                 pms,
                 b"master secret",
                 &randoms);
        ret
    }

    pub fn new_ems(randoms: &SessionRandoms,
                   hs_hash: &[u8],
                   hashalg: &'static ring::digest::Algorithm,
                   pms: &[u8]) -> SessionSecrets {
        let mut ret = SessionSecrets {
            randoms: randoms.clone(),
            hash: hashalg,
            master_secret: [0u8; 48]
        };

        prf::prf(&mut ret.master_secret,
                 ret.hash,
                 pms,
                 b"extended master secret",
                 hs_hash);
        ret
    }

    pub fn new_resume(randoms: &SessionRandoms,
                      hashalg: &'static ring::digest::Algorithm,
                      master_secret: &[u8])
                      -> SessionSecrets {
        let mut ret = SessionSecrets {
            randoms: randoms.clone(),
            hash: hashalg,
            master_secret: [0u8; 48],
        };
        ret.master_secret.as_mut().write_all(master_secret).unwrap();
        ret
    }

    pub fn make_key_block(&self, len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.resize(len, 0u8);

        // NOTE: opposite order to above for no good reason.
        // Don't design security protocols on drugs, kids.
        let randoms = join_randoms(&self.randoms.server, &self.randoms.client);
        prf::prf(&mut out,
                 self.hash,
                 &self.master_secret,
                 b"key expansion",
                 &randoms);

        out
    }

    pub fn get_master_secret(&self) -> Vec<u8> {
        let mut ret = Vec::new();
        ret.extend_from_slice(&self.master_secret);
        ret
    }

    pub fn make_verify_data(&self, handshake_hash: &[u8], label: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.resize(12, 0u8);

        prf::prf(&mut out,
                 self.hash,
                 &self.master_secret,
                 label,
                 handshake_hash);
        out
    }

    pub fn client_verify_data(&self, handshake_hash: &[u8]) -> Vec<u8> {
        self.make_verify_data(handshake_hash, b"client finished")
    }

    pub fn server_verify_data(&self, handshake_hash: &[u8]) -> Vec<u8> {
        self.make_verify_data(handshake_hash, b"server finished")
    }

    pub fn export_keying_material(&self,
                                  output: &mut [u8],
                                  label: &[u8],
                                  context: Option<&[u8]>) {
        let mut randoms = Vec::new();
        randoms.extend_from_slice(&self.randoms.client);
        randoms.extend_from_slice(&self.randoms.server);
        if let Some(context) = context {
            assert!(context.len() <= 0xffff);
            (context.len() as u16).encode(&mut randoms);
            randoms.extend_from_slice(context);
        }

        prf::prf(output,
                 self.hash,
                 &self.master_secret,
                 label,
                 &randoms)
    }
}

// --- Common (to client and server) session functions ---
static SEQ_SOFT_LIMIT: u64 = 0xffff_ffff_ffff_0000u64;
static SEQ_HARD_LIMIT: u64 = 0xffff_ffff_ffff_fffeu64;

enum Limit {
    Yes,
    No
}

pub struct SessionCommon {
    pub negotiated_version: Option<ProtocolVersion>,
    pub is_client: bool,
    message_encrypter: Box<dyn MessageEncrypter>,
    message_decrypter: Box<dyn MessageDecrypter>,
    pub secrets: Option<SessionSecrets>,
    pub key_schedule: Option<KeySchedule>,
    suite: Option<&'static SupportedCipherSuite>,
    write_seq: u64,
    read_seq: u64,
    peer_eof: bool,
    pub peer_encrypting: bool,
    pub we_encrypting: bool,
    pub traffic: bool,
    pub early_traffic: bool,
    pub want_write_key_update: bool,
    pub message_deframer: MessageDeframer,
    pub handshake_joiner: HandshakeJoiner,
    pub message_fragmenter: MessageFragmenter,
    received_plaintext: ChunkVecBuffer,
    sendable_plaintext: ChunkVecBuffer,
    pub sendable_tls: ChunkVecBuffer,
    /// Protocol whose key schedule should be used. Unused for TLS < 1.3.
    pub protocol: Protocol,
    #[cfg(feature = "quic")]
    pub(crate) quic: Quic,
}

impl SessionCommon {
    pub fn new(mtu: Option<usize>, client: bool) -> SessionCommon {
        SessionCommon {
            negotiated_version: None,
            is_client: client,
            suite: None,
            message_encrypter: MessageEncrypter::invalid(),
            message_decrypter: MessageDecrypter::invalid(),
            secrets: None,
            key_schedule: None,
            write_seq: 0,
            read_seq: 0,
            peer_eof: false,
            peer_encrypting: false,
            we_encrypting: false,
            traffic: false,
            early_traffic: false,
            want_write_key_update: false,
            message_deframer: MessageDeframer::new(),
            handshake_joiner: HandshakeJoiner::new(),
            message_fragmenter: MessageFragmenter::new(mtu.unwrap_or(MAX_FRAGMENT_LEN)),
            received_plaintext: ChunkVecBuffer::new(),
            sendable_plaintext: ChunkVecBuffer::new(),
            sendable_tls: ChunkVecBuffer::new(),
            protocol: Protocol::Tls13,
            #[cfg(feature = "quic")]
            quic: Quic::new(),
        }
    }

    pub fn is_tls13(&self) -> bool {
      match self.negotiated_version {
        Some(ProtocolVersion::TLSv1_3) => true,
        _ => false
      }
    }

    pub fn get_suite(&self) -> Option<&'static SupportedCipherSuite> {
        self.suite
    }

    pub fn get_suite_assert(&self) -> &'static SupportedCipherSuite {
        self.suite.as_ref().unwrap()
    }

    pub fn set_suite(&mut self, suite: &'static SupportedCipherSuite) -> bool {
        match self.suite {
            None => {
                self.suite = Some(suite);
                true
            }
            Some(s) if s == suite => {
                self.suite = Some(suite);
                true
            }
            _ => false
        }
    }

    pub fn get_mut_key_schedule(&mut self) -> &mut KeySchedule {
        self.key_schedule.as_mut().unwrap()
    }

    pub fn get_key_schedule(&self) -> &KeySchedule {
        self.key_schedule.as_ref().unwrap()
    }

    pub fn set_key_schedule(&mut self, ks: KeySchedule) {
        self.key_schedule = Some(ks);
    }

    pub fn set_message_encrypter(&mut self,
                                 cipher: Box<dyn MessageEncrypter>) {
        self.message_encrypter = cipher;
        self.write_seq = 0;
        self.we_encrypting = true;
    }

    pub fn set_message_decrypter(&mut self,
                                 cipher: Box<dyn MessageDecrypter>) {
        self.message_decrypter = cipher;
        self.read_seq = 0;
        self.peer_encrypting = true;
    }

    pub fn has_readable_plaintext(&self) -> bool {
        !self.received_plaintext.is_empty()
    }

    pub fn set_buffer_limit(&mut self, limit: usize) {
        self.sendable_plaintext.set_limit(limit);
        self.sendable_tls.set_limit(limit);
    }

    pub fn encrypt_outgoing(&mut self, plain: BorrowMessage) -> Message {
        let seq = self.write_seq;
        self.write_seq += 1;
        self.message_encrypter.encrypt(plain, seq).unwrap()
    }

    pub fn decrypt_incoming(&mut self, encr: Message) -> Result<Message, TLSError> {
        // Perhaps if we send an alert well before their counter wraps, a
        // buggy peer won't make a terrible mistake here?
        // Note that there's no reason to refuse to decrypt: the security
        // failure has already happened.
        if self.read_seq == SEQ_SOFT_LIMIT {
            self.send_close_notify();
        }

        let seq = self.read_seq;
        self.read_seq += 1;
        let ret = self.message_decrypter.decrypt(encr, seq);
        if let Err(TLSError::PeerSentOversizedRecord) = ret {
            self.send_fatal_alert(AlertDescription::RecordOverflow);
        }

        ret
    }

    pub fn process_alert(&mut self, msg: Message) -> Result<(), TLSError> {
        if let MessagePayload::Alert(ref alert) = msg.payload {
            // Reject unknown AlertLevels.
            if let AlertLevel::Unknown(_) = alert.level {
                self.send_fatal_alert(AlertDescription::IllegalParameter);
            }

            // If we get a CloseNotify, make a note to declare EOF to our
            // caller.
            if alert.description == AlertDescription::CloseNotify {
                self.peer_eof = true;
                return Ok(());
            }

            // Warnings are nonfatal for TLS1.2, but outlawed in TLS1.3.
            if alert.level == AlertLevel::Warning {
                if self.is_tls13() {
                    self.send_fatal_alert(AlertDescription::DecodeError);
                } else {
                    warn!("TLS alert warning received: {:#?}", msg);
                    return Ok(());
                }
            }

            error!("TLS alert received: {:#?}", msg);
            Err(TLSError::AlertReceived(alert.description))
        } else {
            Err(TLSError::CorruptMessagePayload(ContentType::Alert))
        }
    }

    fn do_write_key_update(&mut self) {
        // TLS1.3 putting key update triggering here breaks layering
        // between the handshake and record layer.
        let kind = if self.is_client {
            SecretKind::ClientApplicationTrafficSecret
        } else {
            SecretKind::ServerApplicationTrafficSecret
        };

        self.want_write_key_update = false;
        self.send_msg_encrypt(Message::build_key_update_notify());

        let write_key = self.get_key_schedule().derive_next(kind);
        let scs = self.get_suite_assert();
        self.set_message_encrypter(cipher::new_tls13_write(scs, &write_key));

        if self.is_client {
            self.get_mut_key_schedule().current_client_traffic_secret = write_key;
        } else {
            self.get_mut_key_schedule().current_server_traffic_secret = write_key;
        }
    }

    /// Fragment `m`, encrypt the fragments, and then queue
    /// the encrypted fragments for sending.
    pub fn send_msg_encrypt(&mut self, m: Message) {
        if self.want_write_key_update {
            self.do_write_key_update();
        }

        let mut plain_messages = VecDeque::new();
        self.message_fragmenter.fragment(m, &mut plain_messages);

        for m in plain_messages {
            self.send_single_fragment(m.to_borrowed());
        }
    }

    /// Like send_msg_encrypt, but operate on an appdata directly.
    fn send_appdata_encrypt(&mut self,
                            payload: &[u8],
                            limit: Limit) -> usize {
        if self.want_write_key_update {
            self.do_write_key_update();
        }

        // Here, the limit on sendable_tls applies to encrypted data,
        // but we're respecting it for plaintext data -- so we'll
        // be out by whatever the cipher+record overhead is.  That's a
        // constant and predictable amount, so it's not a terrible issue.
        let len = match limit {
            Limit::Yes => self.sendable_tls.apply_limit(payload.len()),
            Limit::No => payload.len()
        };

        let mut plain_messages = VecDeque::new();
        self.message_fragmenter.fragment_borrow(ContentType::ApplicationData,
                                                ProtocolVersion::TLSv1_2,
                                                &payload[..len],
                                                &mut plain_messages);

        for m in plain_messages {
            self.send_single_fragment(m);
        }

        len
    }

    fn send_single_fragment(&mut self, m: BorrowMessage) {
        // Close connection once we start to run out of
        // sequence space.
        if self.write_seq == SEQ_SOFT_LIMIT {
            self.send_close_notify();
        }

        // Refuse to wrap counter at all costs.  This
        // is basically untestable unfortunately.
        if self.write_seq >= SEQ_HARD_LIMIT {
            return;
        }

        let em = self.encrypt_outgoing(m);
        self.queue_tls_message(em);
    }

    /// Are we done? ie, have we processed all received messages,
    /// and received a close_notify to indicate that no new messages
    /// will arrive?
    pub fn connection_at_eof(&self) -> bool {
        self.peer_eof && !self.message_deframer.has_pending()
    }

    /// Read TLS content from `rd`.  This method does internal
    /// buffering, so `rd` can supply TLS messages in arbitrary-
    /// sized chunks (like a socket or pipe might).
    pub fn read_tls(&mut self, rd: &mut dyn Read) -> io::Result<usize> {
        self.message_deframer.read(rd)
    }

    pub fn write_tls(&mut self, wr: &mut dyn Write) -> io::Result<usize> {
        self.sendable_tls.write_to(wr)
    }

    pub fn writev_tls(&mut self, wr: &mut dyn WriteV) -> io::Result<usize> {
        self.sendable_tls.writev_to(wr)
    }

    /// Send plaintext application data, fragmenting and
    /// encrypting it as it goes out.
    ///
    /// If internal buffers are too small, this function will not accept
    /// all the data.
    pub fn send_some_plaintext(&mut self, data: &[u8]) -> io::Result<usize> {
        self.send_plain(data, Limit::Yes)
    }

    pub fn send_early_plaintext(&mut self, data: &[u8]) -> io::Result<usize> {
        debug_assert!(self.early_traffic);
        debug_assert!(self.we_encrypting);

        if data.is_empty() {
            // Don't send empty fragments.
            return Ok(0);
        }

        Ok(self.send_appdata_encrypt(data, Limit::Yes))
    }

    fn send_plain(&mut self, data: &[u8], limit: Limit) -> io::Result<usize> {
        if !self.traffic {
            // If we haven't completed handshaking, buffer
            // plaintext to send once we do.
            let len = match limit {
                Limit::Yes => self.sendable_plaintext.append_limited_copy(data),
                Limit::No => self.sendable_plaintext.append(data.to_vec())
            };
            return Ok(len);
        }

        debug_assert!(self.we_encrypting);

        if data.is_empty() {
            // Don't send empty fragments.
            return Ok(0);
        }

        Ok(self.send_appdata_encrypt(data, limit))
    }

    pub fn start_traffic(&mut self) {
        self.traffic = true;
        self.flush_plaintext();
    }

    /// Send any buffered plaintext.  Plaintext is buffered if
    /// written during handshake.
    pub fn flush_plaintext(&mut self) {
        if !self.traffic {
            return;
        }

        while !self.sendable_plaintext.is_empty() {
            let buf = self.sendable_plaintext.take_one();
            self.send_plain(&buf, Limit::No)
                .unwrap();
        }
    }

    // Put m into sendable_tls for writing.
    fn queue_tls_message(&mut self, m: Message) {
        self.sendable_tls.append(m.get_encoding());
    }

    /// Send a raw TLS message, fragmenting it if needed.
    pub fn send_msg(&mut self, m: Message, must_encrypt: bool) {
        #[cfg(feature = "quic")]
        {
            if let Protocol::Quic = self.protocol {
                if let MessagePayload::Alert(alert) = m.payload {
                    self.quic.alert = Some(alert.description);
                } else {
                    debug_assert!(if let MessagePayload::Handshake(_) = m.payload { true } else { false },
                                  "QUIC uses TLS for the cryptographic handshake only");
                    let mut bytes = Vec::new();
                    m.payload.encode(&mut bytes);
                    self.quic.hs_queue.push_back((self.key_schedule.is_some(), bytes));
                }
                return;
            }
        }
        if !must_encrypt {
            let mut to_send = VecDeque::new();
            self.message_fragmenter.fragment(m, &mut to_send);
            for mm in to_send {
                self.queue_tls_message(mm);
            }
        } else {
            self.send_msg_encrypt(m);
        }
    }

    pub fn take_received_plaintext(&mut self, bytes: Payload) {
        self.received_plaintext.append(bytes.0);
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.received_plaintext.read(buf)?;

        if len == 0 && self.connection_at_eof() && self.received_plaintext.is_empty() {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted,
                                      "CloseNotify alert received"));
        }

        Ok(len)
    }

    pub fn start_encryption_tls12(&mut self, secrets: SessionSecrets) {
        let (dec, enc) = cipher::new_tls12(self.get_suite_assert(), &secrets);
        self.message_encrypter = enc;
        self.message_decrypter = dec;
        self.secrets = Some(secrets);
    }

    pub fn peer_now_encrypting(&mut self) {
        self.peer_encrypting = true;
    }

    pub fn we_now_encrypting(&mut self) {
        self.we_encrypting = true;
    }

    pub fn send_warning_alert(&mut self, desc: AlertDescription) {
        warn!("Sending warning alert {:?}", desc);
        self.send_warning_alert_no_log(desc);
    }

    pub fn send_fatal_alert(&mut self, desc: AlertDescription) {
        warn!("Sending fatal alert {:?}", desc);
        let m = Message::build_alert(AlertLevel::Fatal, desc);
        let enc = self.we_encrypting;
        self.send_msg(m, enc);
    }

    pub fn send_close_notify(&mut self) {
        debug!("Sending warning alert {:?}", AlertDescription::CloseNotify);
        self.send_warning_alert_no_log(AlertDescription::CloseNotify);
    }

    pub fn process_key_update(&mut self,
                              kur: KeyUpdateRequest,
                              read_kind: SecretKind)
                              -> Result<(), TLSError> {
        #[cfg(feature = "quic")]
        {
            if let Protocol::Quic = self.protocol {
                self.send_fatal_alert(AlertDescription::UnexpectedMessage);
                let msg = "KeyUpdate received in QUIC connection".to_string();
                warn!("{}", msg);
                return Err(TLSError::PeerMisbehavedError(msg));
            }
        }

        // Mustn't be interleaved with other handshake messages.
        if !self.handshake_joiner.is_empty() {
            let msg = "KeyUpdate received at wrong time".to_string();
            warn!("{}", msg);
            return Err(TLSError::PeerMisbehavedError(msg));
        }

        match kur {
            KeyUpdateRequest::UpdateNotRequested => {}
            KeyUpdateRequest::UpdateRequested => {
                self.want_write_key_update = true;
            }
            _ => {
                self.send_fatal_alert(AlertDescription::IllegalParameter);
                return Err(TLSError::CorruptMessagePayload(ContentType::Handshake));
            }
        }

        // Update our read-side keys.
        let new_read_key = self.get_key_schedule()
            .derive_next(read_kind);
        let suite = self.get_suite_assert();
        self.set_message_decrypter(cipher::new_tls13_read(suite, &new_read_key));

        if read_kind == SecretKind::ServerApplicationTrafficSecret {
            self.get_mut_key_schedule().current_server_traffic_secret = new_read_key;
        } else {
            self.get_mut_key_schedule().current_client_traffic_secret = new_read_key;
        }

        Ok(())
    }

    pub fn export_keying_material(&self,
                                  output: &mut [u8],
                                  label: &[u8],
                                  context: Option<&[u8]>) -> Result<(), TLSError> {
        if !self.traffic {
            Err(TLSError::HandshakeNotComplete)
        } else if self.is_tls13() {
            self.key_schedule
                .as_ref()
                .unwrap()
                .export_keying_material(output, label, context)
        } else {
            self.secrets
                .as_ref()
                .map(|sec| {
                    sec.export_keying_material(output, label, context)
                })
                .ok_or_else(|| TLSError::HandshakeNotComplete)
        }
    }

    fn send_warning_alert_no_log(&mut self, desc: AlertDescription) {
        let m = Message::build_alert(AlertLevel::Warning, desc);
        let enc = self.we_encrypting;
        self.send_msg(m, enc);
    }
}


#[cfg(feature = "quic")]
pub(crate) struct Quic {
    /// QUIC transport parameters received from the peer during the handshake
    pub params: Option<Vec<u8>>,
    pub alert: Option<AlertDescription>,
    pub hs_queue: VecDeque<(bool, Vec<u8>)>,
    pub early_secret: Option<Vec<u8>>,
    pub hs_secrets: Option<quic::Secrets>,
    pub traffic_secrets: Option<quic::Secrets>,
}

#[cfg(feature = "quic")]
impl Quic {
    pub fn new() -> Self {
        Self {
            params: None,
            alert: None,
            hs_queue: VecDeque::new(),
            early_secret: None,
            hs_secrets: None,
            traffic_secrets: None,
        }
    }
}
