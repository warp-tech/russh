use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use russh_keys::encoding::Encoding;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender};

use super::*;
use crate::channels::{Channel, ChannelMsg};
use crate::msg;

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn get_session_id() -> usize {
    SESSION_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// A connected server session. This type is unique to a client.
pub struct Session {
    pub(crate) session_id: usize,
    pub(crate) common: CommonSession<Arc<Config>>,
    pub(crate) sender: Handle,
    pub(crate) receiver: Receiver<Msg>,
    pub(crate) target_window_size: u32,
    pub(crate) pending_reads: Vec<CryptoVec>,
    pub(crate) pending_len: u32,
    pub(crate) channels: HashMap<ChannelId, UnboundedSender<ChannelMsg>>,
}
#[derive(Debug)]
pub enum Msg {
    ChannelOpenSession {
        sender: UnboundedSender<ChannelMsg>,
    },
    ChannelOpenDirectTcpIp {
        host_to_connect: String,
        port_to_connect: u32,
        originator_address: String,
        originator_port: u32,
        sender: UnboundedSender<ChannelMsg>,
    },
    ChannelOpenForwardedTcpIp {
        connected_address: String,
        connected_port: u32,
        originator_address: String,
        originator_port: u32,
        sender: UnboundedSender<ChannelMsg>,
    },
    Channel(ChannelId, ChannelMsg),
}

impl From<(ChannelId, ChannelMsg)> for Msg {
    fn from((id, msg): (ChannelId, ChannelMsg)) -> Self {
        Msg::Channel(id, msg)
    }
}

#[derive(Clone)]
/// Handle to a session, used to send messages to a client outside of
/// the request/response cycle.
pub struct Handle {
    pub(crate) sender: Sender<Msg>,
}

impl Handle {
    /// Send data to the session referenced by this handler.
    pub async fn data(&self, id: ChannelId, data: CryptoVec) -> Result<(), CryptoVec> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::Data { data }))
            .await
            .map_err(|e| match e.0 {
                Msg::Channel(_, ChannelMsg::Data { data }) => data,
                _ => unreachable!(),
            })
    }

    /// Send data to the session referenced by this handler.
    pub async fn extended_data(
        &self,
        id: ChannelId,
        ext: u32,
        data: CryptoVec,
    ) -> Result<(), CryptoVec> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::ExtendedData { ext, data }))
            .await
            .map_err(|e| match e.0 {
                Msg::Channel(_, ChannelMsg::ExtendedData { data, .. }) => data,
                _ => unreachable!(),
            })
    }

    /// Send EOF to the session referenced by this handler.
    pub async fn eof(&self, id: ChannelId) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::Eof))
            .await
            .map_err(|_| ())
    }

    /// Send success to the session referenced by this handler.
    pub async fn channel_success(&self, id: ChannelId) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::Success))
            .await
            .map_err(|_| ())
    }

    /// Send failure to the session referenced by this handler.
    pub async fn channel_failure(&self, id: ChannelId) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::Failure))
            .await
            .map_err(|_| ())
    }

    /// Close a channel.
    pub async fn close(&self, id: ChannelId) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::Close))
            .await
            .map_err(|_| ())
    }

    /// Inform the client of whether they may perform
    /// control-S/control-Q flow control. See
    /// [RFC4254](https://tools.ietf.org/html/rfc4254#section-6.8).
    pub async fn xon_xoff_request(&self, id: ChannelId, client_can_do: bool) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::XonXoff { client_can_do }))
            .await
            .map_err(|_| ())
    }

    /// Send the exit status of a program.
    pub async fn exit_status_request(&self, id: ChannelId, exit_status: u32) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(id, ChannelMsg::ExitStatus { exit_status }))
            .await
            .map_err(|_| ())
    }

    /// Request a session channel (the most basic type of
    /// channel). This function returns `Some(..)` immediately if the
    /// connection is authenticated, but the channel only becomes
    /// usable when it's confirmed by the server, as indicated by the
    /// `confirmed` field of the corresponding `Channel`.
    pub async fn channel_open_session(&self) -> Result<Channel<Msg>, Error> {
        let (sender, receiver) = unbounded_channel();
        self.sender
            .send(Msg::ChannelOpenSession { sender })
            .await
            .map_err(|_| Error::SendError)?;
        self.wait_channel_confirmation(receiver).await
    }

    /// Open a TCP/IP forwarding channel. This is usually done when a
    /// connection comes to a locally forwarded TCP/IP port. See
    /// [RFC4254](https://tools.ietf.org/html/rfc4254#section-7). The
    /// TCP/IP packets can then be tunneled through the channel using
    /// `.data()`.
    pub async fn channel_open_direct_tcpip<A: Into<String>, B: Into<String>>(
        &self,
        host_to_connect: A,
        port_to_connect: u32,
        originator_address: B,
        originator_port: u32,
    ) -> Result<Channel<Msg>, Error> {
        let (sender, receiver) = unbounded_channel();
        self.sender
            .send(Msg::ChannelOpenDirectTcpIp {
                host_to_connect: host_to_connect.into(),
                port_to_connect,
                originator_address: originator_address.into(),
                originator_port,
                sender,
            })
            .await
            .map_err(|_| Error::SendError)?;
        self.wait_channel_confirmation(receiver).await
    }

    pub async fn channel_open_forwarded_tcpip<A: Into<String>, B: Into<String>>(
        &self,
        connected_address: A,
        connected_port: u32,
        originator_address: B,
        originator_port: u32,
    ) -> Result<Channel<Msg>, Error> {
        let (sender, receiver) = unbounded_channel();
        self.sender
            .send(Msg::ChannelOpenForwardedTcpIp {
                connected_address: connected_address.into(),
                connected_port,
                originator_address: originator_address.into(),
                originator_port,
                sender,
            })
            .await
            .map_err(|_| Error::SendError)?;
        self.wait_channel_confirmation(receiver).await
    }
    async fn wait_channel_confirmation(
        &self,
        mut receiver: UnboundedReceiver<ChannelMsg>,
    ) -> Result<Channel<Msg>, Error> {
        loop {
            match receiver.recv().await {
                Some(ChannelMsg::Open {
                    id,
                    max_packet_size,
                    window_size,
                }) => {
                    return Ok(Channel {
                        id,
                        sender: self.sender.clone(),
                        receiver,
                        max_packet_size,
                        window_size,
                    });
                }
                None => {
                    return Err(Error::Disconnect);
                }
                msg => {
                    debug!("msg = {:?}", msg);
                }
            }
        }
    }

    /// If the program was killed by a signal, send the details about the signal to the client.
    pub async fn exit_signal_request(
        &self,
        id: ChannelId,
        signal_name: Sig,
        core_dumped: bool,
        error_message: String,
        lang_tag: String,
    ) -> Result<(), ()> {
        self.sender
            .send(Msg::Channel(
                id,
                ChannelMsg::ExitSignal {
                    signal_name,
                    core_dumped,
                    error_message,
                    lang_tag,
                },
            ))
            .await
            .map_err(|_| ())
    }
}

impl Session {
    pub(crate) fn is_rekeying(&self) -> bool {
        if let Some(ref enc) = self.common.encrypted {
            enc.rekey.is_some()
        } else {
            true
        }
    }

    pub(crate) async fn run<H, R>(
        mut self,
        mut stream: SshRead<R>,
        mut handler: H,
    ) -> Result<(), H::Error>
    where
        H: Handler + Send + 'static,
        R: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.flush()?;
        stream
            .write_all(&self.common.write_buffer.buffer)
            .await
            .map_err(crate::Error::from)?;
        self.common.write_buffer.buffer.clear();

        let (stream_read, mut stream_write) = stream.split();
        let buffer = SSHBuffer::new();

        // Allow handing out references to the cipher
        let mut opening_cipher = Box::new(clear::Key) as Box<dyn OpeningKey + Send>;
        std::mem::swap(&mut opening_cipher, &mut self.common.cipher.remote_to_local);

        let reading = start_reading(stream_read, buffer, opening_cipher);
        pin!(reading);
        let mut is_reading = None;
        let mut decomp = CryptoVec::new();
        let delay = self.common.config.connection_timeout;

        #[allow(clippy::panic)] // false positive in macro
        while !self.common.disconnected {
            tokio::select! {
                r = &mut reading => {
                    let (stream_read, buffer, mut opening_cipher) = match r {
                        Ok((_, stream_read, buffer, opening_cipher)) => (stream_read, buffer, opening_cipher),
                        Err(e) => return Err(e.into())
                    };
                    if buffer.buffer.len() < 5 {
                        is_reading = Some((stream_read, buffer, opening_cipher));
                        break
                    }
                    #[allow(clippy::indexing_slicing)] // length checked
                    let buf = if let Some(ref mut enc) = self.common.encrypted {
                        let d = enc.decompress.decompress(
                            &buffer.buffer[5..],
                            &mut decomp,
                        );
                        if let Ok(buf) = d {
                            buf
                        } else {
                            debug!("err = {:?}", d);
                            is_reading = Some((stream_read, buffer, opening_cipher));
                            break
                        }
                    } else {
                        &buffer.buffer[5..]
                    };
                    if !buf.is_empty() {
                        #[allow(clippy::indexing_slicing)] // length checked
                        if buf[0] == crate::msg::DISCONNECT {
                            debug!("break");
                            is_reading = Some((stream_read, buffer, opening_cipher));
                            break;
                        } else if buf[0] > 4 {
                            std::mem::swap(&mut opening_cipher, &mut self.common.cipher.remote_to_local);
                            // TODO it'd be cleaner to just pass cipher to reply()
                            match reply(self, handler, buf).await {
                                Ok((h, s)) => {
                                    handler = h;
                                    self = s;
                                },
                                Err(e) => return Err(e),
                            }
                            std::mem::swap(&mut opening_cipher, &mut self.common.cipher.remote_to_local);
                        }
                    }
                    reading.set(start_reading(stream_read, buffer, opening_cipher));
                }
                _ = timeout(delay) => {
                    debug!("timeout");
                    break
                },
                msg = self.receiver.recv(), if !self.is_rekeying() => {
                    match msg {
                        Some(Msg::Channel(id, ChannelMsg::Data { data })) => {
                            self.data(id, data);
                        }
                        Some(Msg::Channel(id, ChannelMsg::ExtendedData { ext, data })) => {
                            self.extended_data(id, ext, data);
                        }
                        Some(Msg::Channel(id, ChannelMsg::Eof)) => {
                            self.eof(id);
                        }
                        Some(Msg::Channel(id, ChannelMsg::Close)) => {
                            self.close(id);
                        }
                        Some(Msg::Channel(id, ChannelMsg::Success)) => {
                            self.channel_success(id);
                        }
                        Some(Msg::Channel(id, ChannelMsg::Failure)) => {
                            self.channel_failure(id);
                        }
                        Some(Msg::Channel(id, ChannelMsg::XonXoff { client_can_do })) => {
                            self.xon_xoff_request(id, client_can_do);
                        }
                        Some(Msg::Channel(id, ChannelMsg::ExitStatus { exit_status })) => {
                            self.exit_status_request(id, exit_status);
                        }
                        Some(Msg::Channel(id, ChannelMsg::ExitSignal { signal_name, core_dumped, error_message, lang_tag })) => {
                            self.exit_signal_request(id, signal_name, core_dumped, &error_message, &lang_tag);
                        }
                        Some(Msg::Channel(id, ChannelMsg::WindowAdjusted { new_size })) => {
                            debug!("window adjusted to {:?} for channel {:?}", new_size, id);
                        }
                        Some(Msg::ChannelOpenSession { sender }) => {
                            let id = self.channel_open_session()?;
                            self.channels.insert(id, sender);
                        }
                        Some(Msg::ChannelOpenDirectTcpIp { host_to_connect, port_to_connect, originator_address, originator_port, sender }) => {
                            let id = self.channel_open_direct_tcpip(&host_to_connect, port_to_connect, &originator_address, originator_port)?;
                            self.channels.insert(id, sender);
                        }
                        Some(Msg::ChannelOpenForwardedTcpIp { connected_address, connected_port, originator_address, originator_port, sender }) => {
                            let id = self.channel_open_forwarded_tcpip(&connected_address, connected_port, &originator_address, originator_port)?;
                            self.channels.insert(id, sender);
                        }
                        Some(_) => {
                            // should be unreachable, since the receiver only gets
                            // messages from methods implemented within russh
                            unimplemented!("unimplemented (client-only?) message: {:?}", msg)
                        }
                        None => {
                            debug!("self.receiver: received None");
                        }
                    }
                }
            }
            self.flush()?;
            stream_write
                .write_all(&self.common.write_buffer.buffer)
                .await
                .map_err(crate::Error::from)?;
            self.common.write_buffer.buffer.clear();
        }
        debug!("disconnected");
        // Shutdown
        stream_write.shutdown().await.map_err(crate::Error::from)?;
        loop {
            if let Some((stream_read, buffer, opening_cipher)) = is_reading.take() {
                reading.set(start_reading(stream_read, buffer, opening_cipher));
            }
            let (n, r, b, opening_cipher) = (&mut reading).await?;
            is_reading = Some((r, b, opening_cipher));
            if n == 0 {
                break;
            }
        }

        Ok(())
    }

    /// Application-unqiue session ID. Can be used for identifying the session
    /// across method calls.
    pub fn id(&self) -> usize {
        self.session_id
    }

    /// Get a handle to this session.
    pub fn handle(&self) -> Handle {
        self.sender.clone()
    }

    pub fn writable_packet_size(&self, channel: &ChannelId) -> u32 {
        if let Some(ref enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(channel) {
                return channel
                    .sender_window_size
                    .min(channel.sender_maximum_packet_size);
            }
        }
        0
    }

    pub fn window_size(&self, channel: &ChannelId) -> u32 {
        if let Some(ref enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(channel) {
                return channel.sender_window_size;
            }
        }
        0
    }

    pub fn max_packet_size(&self, channel: &ChannelId) -> u32 {
        if let Some(ref enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(channel) {
                return channel.sender_maximum_packet_size;
            }
        }
        0
    }

    /// Flush the session, i.e. encrypt the pending buffer.
    pub fn flush(&mut self) -> Result<(), Error> {
        if let Some(ref mut enc) = self.common.encrypted {
            if enc.flush(
                &self.common.config.as_ref().limits,
                &mut *self.common.cipher.local_to_remote,
                &mut self.common.write_buffer,
            )? && enc.rekey.is_none()
            {
                debug!("starting rekeying");
                if let Some(exchange) = enc.exchange.take() {
                    let mut kexinit = KexInit::initiate_rekey(exchange, &enc.session_id);
                    kexinit.server_write(
                        self.common.config.as_ref(),
                        &mut *self.common.cipher.local_to_remote,
                        &mut self.common.write_buffer,
                    )?;
                    enc.rekey = Some(Kex::Init(kexinit))
                }
            }
        }
        Ok(())
    }

    pub fn flush_pending(&mut self, channel: ChannelId) -> usize {
        if let Some(ref mut enc) = self.common.encrypted {
            enc.flush_pending(channel)
        } else {
            0
        }
    }

    pub fn sender_window_size(&self, channel: ChannelId) -> usize {
        if let Some(ref enc) = self.common.encrypted {
            enc.sender_window_size(channel)
        } else {
            0
        }
    }

    pub fn has_pending_data(&self, channel: ChannelId) -> bool {
        if let Some(ref enc) = self.common.encrypted {
            enc.has_pending_data(channel)
        } else {
            false
        }
    }

    /// Retrieves the configuration of this session.
    pub fn config(&self) -> &Config {
        &self.common.config
    }

    /// Sends a disconnect message.
    pub fn disconnect(&mut self, reason: Disconnect, description: &str, language_tag: &str) {
        self.common.disconnect(reason, description, language_tag);
    }

    /// Send a "success" reply to a /global/ request (requests without
    /// a channel number, such as TCP/IP forwarding or
    /// cancelling). Always call this function if the request was
    /// successful (it checks whether the client expects an answer).
    pub fn request_success(&mut self) {
        if self.common.wants_reply {
            if let Some(ref mut enc) = self.common.encrypted {
                self.common.wants_reply = false;
                push_packet!(enc.write, enc.write.push(msg::REQUEST_SUCCESS))
            }
        }
    }

    /// Send a "failure" reply to a global request.
    pub fn request_failure(&mut self) {
        if let Some(ref mut enc) = self.common.encrypted {
            self.common.wants_reply = false;
            push_packet!(enc.write, enc.write.push(msg::REQUEST_FAILURE))
        }
    }

    /// Send a "success" reply to a channel request. Always call this
    /// function if the request was successful (it checks whether the
    /// client expects an answer).
    pub fn channel_success(&mut self, channel: ChannelId) {
        if let Some(ref mut enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get_mut(&channel) {
                assert!(channel.confirmed);
                if channel.wants_reply {
                    channel.wants_reply = false;
                    debug!("channel_success {:?}", channel);
                    push_packet!(enc.write, {
                        enc.write.push(msg::CHANNEL_SUCCESS);
                        enc.write.push_u32_be(channel.recipient_channel);
                    })
                }
            }
        }
    }

    /// Send a "failure" reply to a global request.
    pub fn channel_failure(&mut self, channel: ChannelId) {
        if let Some(ref mut enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get_mut(&channel) {
                assert!(channel.confirmed);
                if channel.wants_reply {
                    channel.wants_reply = false;
                    push_packet!(enc.write, {
                        enc.write.push(msg::CHANNEL_FAILURE);
                        enc.write.push_u32_be(channel.recipient_channel);
                    })
                }
            }
        }
    }

    /// Send a "failure" reply to a request to open a channel open.
    pub fn channel_open_failure(
        &mut self,
        channel: ChannelId,
        reason: ChannelOpenFailure,
        description: &str,
        language: &str,
    ) {
        if let Some(ref mut enc) = self.common.encrypted {
            push_packet!(enc.write, {
                enc.write.push(msg::CHANNEL_OPEN_FAILURE);
                enc.write.push_u32_be(channel.0);
                enc.write.push_u32_be(reason as u32);
                enc.write.extend_ssh_string(description.as_bytes());
                enc.write.extend_ssh_string(language.as_bytes());
            })
        }
    }

    /// Close a channel.
    pub fn close(&mut self, channel: ChannelId) {
        self.common.byte(channel, msg::CHANNEL_CLOSE);
    }

    /// Send EOF to a channel
    pub fn eof(&mut self, channel: ChannelId) {
        self.common.byte(channel, msg::CHANNEL_EOF);
    }

    /// Send data to a channel. On session channels, `extended` can be
    /// used to encode standard error by passing `Some(1)`, and stdout
    /// by passing `None`.
    ///
    /// The number of bytes added to the "sending pipeline" (to be
    /// processed by the event loop) is returned.
    pub fn data(&mut self, channel: ChannelId, data: CryptoVec) {
        if let Some(ref mut enc) = self.common.encrypted {
            enc.data(channel, data)
        } else {
            unreachable!()
        }
    }

    /// Send data to a channel. On session channels, `extended` can be
    /// used to encode standard error by passing `Some(1)`, and stdout
    /// by passing `None`.
    ///
    /// The number of bytes added to the "sending pipeline" (to be
    /// processed by the event loop) is returned.
    pub fn extended_data(&mut self, channel: ChannelId, extended: u32, data: CryptoVec) {
        if let Some(ref mut enc) = self.common.encrypted {
            enc.extended_data(channel, extended, data)
        } else {
            unreachable!()
        }
    }

    /// Inform the client of whether they may perform
    /// control-S/control-Q flow control. See
    /// [RFC4254](https://tools.ietf.org/html/rfc4254#section-6.8).
    pub fn xon_xoff_request(&mut self, channel: ChannelId, client_can_do: bool) {
        if let Some(ref mut enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(&channel) {
                assert!(channel.confirmed);
                push_packet!(enc.write, {
                    enc.write.push(msg::CHANNEL_REQUEST);

                    enc.write.push_u32_be(channel.recipient_channel);
                    enc.write.extend_ssh_string(b"xon-xoff");
                    enc.write.push(0);
                    enc.write.push(if client_can_do { 1 } else { 0 });
                })
            }
        }
    }

    /// Send the exit status of a program.
    pub fn exit_status_request(&mut self, channel: ChannelId, exit_status: u32) {
        if let Some(ref mut enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(&channel) {
                assert!(channel.confirmed);
                push_packet!(enc.write, {
                    enc.write.push(msg::CHANNEL_REQUEST);

                    enc.write.push_u32_be(channel.recipient_channel);
                    enc.write.extend_ssh_string(b"exit-status");
                    enc.write.push(0);
                    enc.write.push_u32_be(exit_status)
                })
            }
        }
    }

    /// If the program was killed by a signal, send the details about the signal to the client.
    pub fn exit_signal_request(
        &mut self,
        channel: ChannelId,
        signal: Sig,
        core_dumped: bool,
        error_message: &str,
        language_tag: &str,
    ) {
        if let Some(ref mut enc) = self.common.encrypted {
            if let Some(channel) = enc.channels.get(&channel) {
                assert!(channel.confirmed);
                push_packet!(enc.write, {
                    enc.write.push(msg::CHANNEL_REQUEST);

                    enc.write.push_u32_be(channel.recipient_channel);
                    enc.write.extend_ssh_string(b"exit-signal");
                    enc.write.push(0);
                    enc.write.extend_ssh_string(signal.name().as_bytes());
                    enc.write.push(if core_dumped { 1 } else { 0 });
                    enc.write.extend_ssh_string(error_message.as_bytes());
                    enc.write.extend_ssh_string(language_tag.as_bytes());
                })
            }
        }
    }

    /// Opens a new session channel on the client.
    pub fn channel_open_session(&mut self) -> Result<ChannelId, Error> {
        let result = if let Some(ref mut enc) = self.common.encrypted {
            if !matches!(
                enc.state,
                EncryptedState::Authenticated | EncryptedState::InitCompression
            ) {
                return Err(Error::Inconsistent);
            }

            let sender_channel = enc.new_channel(
                self.common.config.window_size,
                self.common.config.maximum_packet_size,
            );
            push_packet!(enc.write, {
                enc.write.push(msg::CHANNEL_OPEN);
                enc.write.extend_ssh_string(b"session");

                // sender channel id.
                enc.write.push_u32_be(sender_channel.0);

                // window.
                enc.write
                    .push_u32_be(self.common.config.as_ref().window_size);

                // max packet size.
                enc.write
                    .push_u32_be(self.common.config.as_ref().maximum_packet_size);
            });
            sender_channel
        } else {
            return Err(Error::Inconsistent);
        };
        Ok(result)
    }

    /// Opens a direct TCP/IP channel on the client.
    pub fn channel_open_direct_tcpip(
        &mut self,
        host_to_connect: &str,
        port_to_connect: u32,
        originator_address: &str,
        originator_port: u32,
    ) -> Result<ChannelId, Error> {
        let result = if let Some(ref mut enc) = self.common.encrypted {
            if !matches!(
                enc.state,
                EncryptedState::Authenticated | EncryptedState::InitCompression
            ) {
                return Err(Error::Inconsistent);
            }
            let sender_channel = enc.new_channel(
                self.common.config.window_size,
                self.common.config.maximum_packet_size,
            );
            push_packet!(enc.write, {
                enc.write.push(msg::CHANNEL_OPEN);
                enc.write.extend_ssh_string(b"direct-tcpip");

                // sender channel id.
                enc.write.push_u32_be(sender_channel.0);

                // window.
                enc.write
                    .push_u32_be(self.common.config.as_ref().window_size);

                // max packet size.
                enc.write
                    .push_u32_be(self.common.config.as_ref().maximum_packet_size);

                enc.write.extend_ssh_string(host_to_connect.as_bytes());
                enc.write.push_u32_be(port_to_connect); // sender channel id.
                enc.write.extend_ssh_string(originator_address.as_bytes());
                enc.write.push_u32_be(originator_port); // sender channel id.
            });
            sender_channel
        } else {
            return Err(Error::Inconsistent);
        };

        Ok(result)
    }

    /// Open a TCP/IP forwarding channel, when a connection comes to a
    /// local port for which forwarding has been requested. See
    /// [RFC4254](https://tools.ietf.org/html/rfc4254#section-7). The
    /// TCP/IP packets can then be tunneled through the channel using
    /// `.data()`.
    pub fn channel_open_forwarded_tcpip(
        &mut self,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
    ) -> Result<ChannelId, Error> {
        let result = if let Some(ref mut enc) = self.common.encrypted {
            if !matches!(
                enc.state,
                EncryptedState::Authenticated | EncryptedState::InitCompression
            ) {
                return Err(Error::Inconsistent);
            }

            debug!("sending open request");
            let sender_channel = enc.new_channel(
                self.common.config.window_size,
                self.common.config.maximum_packet_size,
            );
            push_packet!(enc.write, {
                enc.write.push(msg::CHANNEL_OPEN);
                enc.write.extend_ssh_string(b"forwarded-tcpip");

                // sender channel id.
                enc.write.push_u32_be(sender_channel.0);

                // window.
                enc.write
                    .push_u32_be(self.common.config.as_ref().window_size);

                // max packet size.
                enc.write
                    .push_u32_be(self.common.config.as_ref().maximum_packet_size);

                enc.write.extend_ssh_string(connected_address.as_bytes());
                enc.write.push_u32_be(connected_port); // sender channel id.
                enc.write.extend_ssh_string(originator_address.as_bytes());
                enc.write.push_u32_be(originator_port); // sender channel id.
            });
            sender_channel
        } else {
            return Err(Error::Inconsistent);
        };
        Ok(result)
    }

    /// Requests that the client forward connections to the given host and port.
    /// See [RFC4254](https://tools.ietf.org/html/rfc4254#section-7). The client
    /// will open forwarded_tcpip channels for each connection.
    pub fn tcpip_forward(&mut self, address: &str, port: u32) {
        if let Some(ref mut enc) = self.common.encrypted {
            push_packet!(enc.write, {
                enc.write.push(msg::GLOBAL_REQUEST);
                enc.write.extend_ssh_string(b"tcpip-forward");
                enc.write.push(0);
                enc.write.extend_ssh_string(address.as_bytes());
                enc.write.push_u32_be(port);
            });
        }
    }

    /// Cancels a previously tcpip_forward request.
    pub fn cancel_tcpip_forward(&mut self, address: &str, port: u32) {
        if let Some(ref mut enc) = self.common.encrypted {
            push_packet!(enc.write, {
                enc.write.push(msg::GLOBAL_REQUEST);
                enc.write.extend_ssh_string(b"cancel-tcpip-forward");
                enc.write.push(0);
                enc.write.extend_ssh_string(address.as_bytes());
                enc.write.push_u32_be(port);
            });
        }
    }
}
