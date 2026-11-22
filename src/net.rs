use std::{
    io::{self, Read},
    net::{Ipv4Addr, TcpListener, TcpStream},
    time::{Duration, Instant},
};

const MAX_CLIENTS: usize = 8;
const MAX_PAYLOAD: usize = 16 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) struct Listener {
    socket: TcpListener,
    clients: Vec<Client>,
    discard_backlog: bool,
}

struct Client {
    socket: TcpStream,
    accepted: Instant,
    payload: Vec<u8>,
}

impl Listener {
    pub(crate) fn bind(port: u16) -> io::Result<Self> {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            clients: Vec::new(),
            discard_backlog: false,
        })
    }

    pub(crate) fn poll(&mut self, now: Instant, deliver: bool) -> Vec<String> {
        // If the accept budget runs out while blocked, keep discarding until the kernel
        // queue has been drained. Never replay a client that waited through a modal state.
        self.discard_backlog |= !deliver;
        if self.discard_backlog {
            self.clients.clear();
        } else {
            self.clients
                .retain(|client| now.duration_since(client.accepted) < FRAME_TIMEOUT);
        }
        for _ in 0..MAX_CLIENTS {
            match self.socket.accept() {
                Ok((socket, _)) => {
                    if !self.discard_backlog
                        && self.clients.len() < MAX_CLIENTS
                        && socket.set_nonblocking(true).is_ok()
                    {
                        self.clients.push(Client {
                            socket,
                            accepted: now,
                            payload: Vec::new(),
                        });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.discard_backlog = false;
                    break;
                }
                Err(_) => break,
            }
        }
        let mut lines = Vec::new();
        self.clients
            .retain_mut(|client| client.read_line(&mut lines));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::Shutdown};

    #[test]
    fn local_listener_delivers_a_fragmented_frame_only_after_lf() -> io::Result<()> {
        let mut listener = Listener::bind(0)?;
        let addr = listener.socket.local_addr()?;
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
        let now = Instant::now();
        let mut stream = TcpStream::connect_timeout(&addr, FRAME_TIMEOUT)?;
        stream.write_all(b"part")?;
        assert!(listener.poll(now, true).is_empty());
        stream.write_all(b"ial\n")?;
        let deadline = Instant::now() + FRAME_TIMEOUT;
        loop {
            let lines = listener.poll(now, true);
            if !lines.is_empty() {
                assert_eq!(lines, ["partial"]);
                break;
            }
            assert!(Instant::now() < deadline, "no complete frame");
            std::thread::yield_now();
        }
        Ok(())
    }

    #[test]
    fn local_listener_drops_invalid_and_expired_frames_but_accepts_the_payload_limit()
    -> io::Result<()> {
        let mut listener = Listener::bind(0)?;
        let addr = listener.socket.local_addr()?;
        let now = Instant::now();
        let mut stalled = TcpStream::connect_timeout(&addr, FRAME_TIMEOUT)?;
        stalled.write_all(b"expired")?;
        assert!(listener.poll(now, true).is_empty());
        assert!(listener.poll(now + FRAME_TIMEOUT, true).is_empty());
        assert!(listener.clients.is_empty());

        let mut over_limit = vec![b'a'; MAX_PAYLOAD + 1];
        over_limit.push(b'\n');
        let mut at_limit = vec![b'b'; MAX_PAYLOAD];
        at_limit.push(b'\n');
        for payload in [
            b"\xff\n".as_slice(),
            b"cr\r\n",
            b"nul\0\n",
            b"no LF",
            &over_limit,
            &at_limit,
        ] {
            let mut stream = TcpStream::connect_timeout(&addr, FRAME_TIMEOUT)?;
            stream.write_all(payload)?;
            stream.shutdown(Shutdown::Write)?;
        }
        let deadline = Instant::now() + FRAME_TIMEOUT;
        loop {
            let lines = listener.poll(now + FRAME_TIMEOUT, true);
            if !lines.is_empty() {
                assert_eq!(lines, ["b".repeat(MAX_PAYLOAD)]);
                break;
            }
            assert!(Instant::now() < deadline, "no valid frame");
            std::thread::yield_now();
        }
        assert!(listener.poll(now + FRAME_TIMEOUT, true).is_empty());
        Ok(())
    }
}

impl Client {
    fn read_line(&mut self, lines: &mut Vec<String>) -> bool {
        let mut buffer = [0; MAX_PAYLOAD + 1];
        let room = MAX_PAYLOAD + 1 - self.payload.len();
        match self.socket.read(&mut buffer[..room]) {
            Ok(0) => false,
            Ok(n) => {
                let bytes = &buffer[..n];
                if let Some(end) = bytes.iter().position(|&byte| byte == b'\n') {
                    self.payload.extend_from_slice(&bytes[..end]);
                    if !self.payload.contains(&b'\r')
                        && !self.payload.contains(&0)
                        && let Ok(line) = String::from_utf8(std::mem::take(&mut self.payload))
                    {
                        lines.push(line);
                    }
                    false
                } else {
                    self.payload.extend_from_slice(bytes);
                    self.payload.len() <= MAX_PAYLOAD
                }
            }
            Err(error) => matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        }
    }
}
