use crate::{CONTROL_PORT, LOCAL_IP};
use alvr_common::{ConResult, ConnectionError, HandleTryAgain, ToCon, anyhow::Result, con_bail};
use alvr_session::{DscpTos, SocketBufferConfig};
use bincode::config;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    marker::PhantomData,
    mem,
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    time::{Duration, Instant},
};

// This corresponds to the length of the payload
const FRAMED_PREFIX_LENGTH: usize = mem::size_of::<u32>();

// Lets a sequence of socket reads wait until a deadline, whatever the socket read timeout. The read
// timeout is shortened only for the last wait, so that it doesn't block past the deadline, and is
// restored on drop
struct ReadDeadline<'a> {
    socket: &'a TcpStream,
    deadline: Instant,
    original_read_timeout: Option<Option<Duration>>,
}

impl<'a> ReadDeadline<'a> {
    fn new(socket: &'a TcpStream, timeout: Duration) -> Self {
        Self {
            socket,
            deadline: Instant::now() + timeout,
            original_read_timeout: None,
        }
    }

    // Must be called after a read returned without completing the packet (timed out or partial
    // data). Returns `TryAgain` if the deadline has passed, otherwise makes sure that the next read
    // cannot block past the deadline
    fn prepare_retry(&mut self) -> ConResult {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return alvr_common::try_again();
        }

        let read_timeout = match self.original_read_timeout {
            Some(read_timeout) => read_timeout,
            None => *self
                .original_read_timeout
                .insert(self.socket.read_timeout().to_con()?),
        };
        if read_timeout.is_none_or(|read_timeout| remaining < read_timeout) {
            self.socket.set_read_timeout(Some(remaining)).to_con()?;
        }

        Ok(())
    }
}

impl Drop for ReadDeadline<'_> {
    fn drop(&mut self) {
        if let Some(read_timeout) = self.original_read_timeout {
            self.socket.set_read_timeout(read_timeout).ok();
        }
    }
}

pub fn bind(
    timeout: Duration,
    port: u16,
    dscp: Option<DscpTos>,
    buffer_config: SocketBufferConfig,
) -> Result<TcpListener> {
    let socket = TcpListener::bind((LOCAL_IP, port))?.into();

    crate::set_socket_buffers(&socket, buffer_config).ok();

    crate::set_dscp(&socket, dscp);

    socket.set_read_timeout(Some(timeout))?;

    Ok(socket.into())
}

pub fn accept_from_server(
    listener: &TcpListener,
    server_ip: Option<IpAddr>,
    timeout: Duration,
) -> ConResult<(TcpStream, TcpStream)> {
    // Uses timeout set during bind()
    let (socket, server_address) = listener.accept().handle_try_again()?;

    if let Some(ip) = server_ip
        && server_address.ip() != ip
    {
        con_bail!(
            "Connected to wrong client: Expected: {ip}, Found {}",
            server_address.ip()
        );
    }

    socket.set_read_timeout(Some(timeout)).to_con()?;
    socket.set_nodelay(true).to_con()?;

    Ok((socket.try_clone().to_con()?, socket))
}

pub fn connect_to_client(
    timeout: Duration,
    client_ips: &[IpAddr],
    port: u16,
    buffer_config: SocketBufferConfig,
) -> ConResult<(TcpStream, TcpStream)> {
    let split_timeout = timeout / client_ips.len() as u32;

    let mut res = alvr_common::try_again();
    for ip in client_ips {
        res = TcpStream::connect_timeout(&SocketAddr::new(*ip, port), split_timeout)
            .handle_try_again();

        if res.is_ok() {
            break;
        }
    }
    let socket = res?.into();

    crate::set_socket_buffers(&socket, buffer_config).ok();
    socket.set_read_timeout(Some(timeout)).to_con()?;

    let socket = TcpStream::from(socket);

    socket.set_nodelay(true).to_con()?;

    Ok((socket.try_clone().to_con()?, socket))
}

fn framed_send<S: Serialize>(
    socket: &mut TcpStream,
    buffer: &mut Vec<u8>,
    packet: &S,
) -> Result<()> {
    buffer.resize(FRAMED_PREFIX_LENGTH, 0);

    let encoded_size = bincode::serde::encode_into_std_write(packet, buffer, config::standard())?;

    buffer[0..FRAMED_PREFIX_LENGTH].copy_from_slice(&(encoded_size as u32).to_le_bytes());

    socket.write_all(buffer)?;

    Ok(())
}

// Waits for a packet until `timeout` expires, even if the socket read timeout is shorter. When the
// caller keeps `buffer` and `recv_cursor` between calls, a partially received packet is completed by
// the next call
fn framed_recv<R: DeserializeOwned>(
    mut socket: &TcpStream,
    buffer: &mut Vec<u8>,
    recv_cursor: &mut Option<usize>,
    timeout: Duration,
) -> ConResult<R> {
    let mut deadline = ReadDeadline::new(socket, timeout);

    let recv_cursor_ref = if let Some(cursor) = recv_cursor {
        cursor
    } else {
        let mut payload_size_bytes = [0; FRAMED_PREFIX_LENGTH];

        // A timed out peek only means that no data arrived yet: the socket read timeout can be
        // shorter than the requested timeout, so keep waiting until the deadline
        loop {
            match socket.peek(&mut payload_size_bytes).handle_try_again() {
                Ok(FRAMED_PREFIX_LENGTH) => break,
                Ok(0) => con_bail!("Connection closed by peer"),
                Ok(_) | Err(ConnectionError::TryAgain(_)) => deadline.prepare_retry()?,
                Err(e) => return Err(e),
            }
        }

        let size = FRAMED_PREFIX_LENGTH + u32::from_le_bytes(payload_size_bytes) as usize;
        buffer.resize(size, 0);

        recv_cursor.insert(0)
    };

    loop {
        match socket
            .read(&mut buffer[*recv_cursor_ref..])
            .handle_try_again()
        {
            Ok(0) => con_bail!("Connection closed by peer"),
            Ok(count) => *recv_cursor_ref += count,
            Err(ConnectionError::TryAgain(_)) => (),
            Err(e) => return Err(e),
        }

        if *recv_cursor_ref == buffer.len() {
            break;
        }

        deadline.prepare_retry()?;
    }

    let (packet, _) =
        bincode::serde::decode_from_slice(&buffer[FRAMED_PREFIX_LENGTH..], config::standard())
            .to_con()?;

    *recv_cursor = None;

    Ok(packet)
}

pub struct ControlSocketSender<T> {
    pub(crate) inner: TcpStream,
    pub(crate) buffer: Vec<u8>,
    pub(crate) _phantom: PhantomData<T>,
}

impl<S: Serialize> ControlSocketSender<S> {
    pub fn send(&mut self, packet: &S) -> Result<()> {
        framed_send(&mut self.inner, &mut self.buffer, packet)
    }
}

pub struct ControlSocketReceiver<T> {
    pub(crate) inner: TcpStream,
    pub(crate) buffer: Vec<u8>,
    pub(crate) recv_cursor: Option<usize>,
    pub(crate) _phantom: PhantomData<T>,
}

impl<R: DeserializeOwned> ControlSocketReceiver<R> {
    pub fn recv(&mut self, timeout: Duration) -> ConResult<R> {
        framed_recv(
            &self.inner,
            &mut self.buffer,
            &mut self.recv_cursor,
            timeout,
        )
    }
}

pub fn get_server_listener(timeout: Duration) -> Result<TcpListener> {
    let listener = bind(timeout, CONTROL_PORT, None, SocketBufferConfig::default())?;

    Ok(listener)
}

// Proto-control-socket that can send and receive any packet. After the split, only the packets of
// the specified types can be exchanged
pub struct ProtoControlSocket {
    pub(crate) inner: TcpStream,
}

pub enum PeerType<'a> {
    AnyClient(Vec<IpAddr>),
    Server(&'a TcpListener),
}

impl ProtoControlSocket {
    pub fn connect_to(timeout: Duration, peer: PeerType<'_>) -> ConResult<(Self, IpAddr)> {
        let socket = match peer {
            PeerType::AnyClient(ips) => {
                connect_to_client(timeout, &ips, CONTROL_PORT, SocketBufferConfig::default())?.0
            }
            PeerType::Server(listener) => accept_from_server(listener, None, timeout)?.0,
        };

        let peer_ip = socket.peer_addr().to_con()?.ip();

        Ok((Self { inner: socket }, peer_ip))
    }

    pub fn send<S: Serialize>(&mut self, packet: &S) -> Result<()> {
        framed_send(&mut self.inner, &mut vec![], packet)
    }

    pub fn recv<R: DeserializeOwned>(&mut self, timeout: Duration) -> ConResult<R> {
        framed_recv(&self.inner, &mut vec![], &mut None, timeout)
    }

    pub fn split<S: Serialize, R: DeserializeOwned>(
        self,
        timeout: Duration,
    ) -> Result<(ControlSocketSender<S>, ControlSocketReceiver<R>)> {
        self.inner.set_read_timeout(Some(timeout))?;

        Ok((
            ControlSocketSender {
                inner: self.inner.try_clone()?,
                buffer: vec![0; FRAMED_PREFIX_LENGTH],
                _phantom: PhantomData,
            },
            ControlSocketReceiver {
                inner: self.inner,
                buffer: vec![0; FRAMED_PREFIX_LENGTH],
                recv_cursor: None,
                _phantom: PhantomData,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn socket_pair(receiver_read_timeout: Duration) -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        receiver
            .set_read_timeout(Some(receiver_read_timeout))
            .unwrap();

        (sender, receiver)
    }

    #[test]
    fn framed_recv_waits_past_socket_read_timeout() {
        let (mut sender, receiver) = socket_pair(Duration::from_millis(100));

        let send_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            framed_send(&mut sender, &mut vec![], &42_u32).unwrap();

            // Keep the connection open until the packet is received
            sender
        });

        let start = Instant::now();
        let maybe_packet =
            framed_recv::<u32>(&receiver, &mut vec![], &mut None, Duration::from_secs(2));
        let elapsed = start.elapsed();

        assert!(matches!(maybe_packet, Ok(42)));
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
        assert_eq!(
            receiver.read_timeout().unwrap(),
            Some(Duration::from_millis(100))
        );

        send_thread.join().unwrap();
    }

    #[test]
    fn framed_recv_times_out_at_deadline() {
        // The timeout is not a multiple of the socket read timeout: without shortening the last
        // read, recv would return only after 600 ms
        let read_timeout = Duration::from_millis(300);
        let timeout = Duration::from_millis(400);
        let (_sender, receiver) = socket_pair(read_timeout);

        let start = Instant::now();
        let maybe_packet = framed_recv::<u32>(&receiver, &mut vec![], &mut None, timeout);
        let elapsed = start.elapsed();

        assert!(matches!(maybe_packet, Err(ConnectionError::TryAgain(_))));
        assert!(elapsed >= timeout, "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(550), "{elapsed:?}");
        assert_eq!(receiver.read_timeout().unwrap(), Some(read_timeout));
    }

    #[test]
    fn framed_recv_fails_fast_when_peer_closes() {
        let (sender, receiver) = socket_pair(Duration::from_millis(100));
        drop(sender);

        let start = Instant::now();
        let maybe_packet =
            framed_recv::<u32>(&receiver, &mut vec![], &mut None, Duration::from_secs(5));

        assert!(matches!(maybe_packet, Err(ConnectionError::Other(_))));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
