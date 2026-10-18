//! Helpers shared by the crate's unit tests.
//!
//! Both helpers are deliberately minimal: a SOCKS5 endpoint that records what
//! the client asked it to reach, and a one-shot HTTP origin.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

/// Where a client asked the SOCKS5 endpoint to connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Socks5Target {
    Domain { host: String, port: u16 },
    Addr(SocketAddr),
}

/// A single-connection SOCKS5 endpoint that forwards to the requested target.
pub(crate) struct Socks5Proxy {
    pub address: SocketAddr,
    observed: oneshot::Receiver<Socks5Target>,
}

impl Socks5Proxy {
    /// Starts an endpoint that forwards wherever the client asks it to.
    ///
    /// It requires RFC 1929 credentials when given.
    pub(crate) async fn start(credentials: Option<(&'static str, &'static str)>) -> Self {
        Self::spawn(credentials, None).await
    }

    /// Starts an endpoint that forwards every requested host name to `upstream`.
    ///
    /// A remote-resolving client names a host that only the proxy can reach,
    /// which is what makes the observed target meaningful.
    pub(crate) async fn start_resolving_domains(
        credentials: Option<(&'static str, &'static str)>,
        upstream: SocketAddr,
    ) -> Self {
        Self::spawn(credentials, Some(upstream)).await
    }

    async fn spawn(
        credentials: Option<(&'static str, &'static str)>,
        domain_upstream: Option<SocketAddr>,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (observed_sender, observed) = oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(socket, credentials, domain_upstream, observed_sender).await;
        });
        Self { address, observed }
    }

    /// Resolves with the target of the first `CONNECT` request.
    pub(crate) async fn observed_target(&mut self) -> Socks5Target {
        (&mut self.observed)
            .await
            .expect("the SOCKS5 endpoint observed a request")
    }
}

async fn serve(
    mut socket: TcpStream,
    credentials: Option<(&'static str, &'static str)>,
    domain_upstream: Option<SocketAddr>,
    observed: oneshot::Sender<Socks5Target>,
) {
    let mut greeting = [0_u8; 2];
    socket.read_exact(&mut greeting).await.unwrap();
    assert_eq!(greeting[0], 5);
    let mut methods = vec![0_u8; greeting[1] as usize];
    socket.read_exact(&mut methods).await.unwrap();

    let method = if credentials.is_some() { 2 } else { 0 };
    assert!(methods.contains(&method), "the client offered {methods:?}");
    socket.write_all(&[5, method]).await.unwrap();

    if let Some((expected_username, expected_password)) = credentials {
        let mut header = [0_u8; 2];
        socket.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 1);
        let mut username = vec![0_u8; header[1] as usize];
        socket.read_exact(&mut username).await.unwrap();
        let mut password_length = [0_u8; 1];
        socket.read_exact(&mut password_length).await.unwrap();
        let mut password = vec![0_u8; password_length[0] as usize];
        socket.read_exact(&mut password).await.unwrap();
        let matches =
            username == expected_username.as_bytes() && password == expected_password.as_bytes();
        socket
            .write_all(&[1, if matches { 0 } else { 1 }])
            .await
            .unwrap();
        assert!(matches, "unexpected proxy credentials");
    }

    let mut request = [0_u8; 4];
    socket.read_exact(&mut request).await.unwrap();
    assert_eq!(request[0], 5);
    assert_eq!(request[1], 1, "only CONNECT is supported");
    let target = match request[3] {
        1 => {
            let mut address = [0_u8; 4];
            socket.read_exact(&mut address).await.unwrap();
            Socks5Target::Addr(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(address)),
                read_port(&mut socket).await,
            ))
        }
        3 => {
            let mut length = [0_u8; 1];
            socket.read_exact(&mut length).await.unwrap();
            let mut host = vec![0_u8; length[0] as usize];
            socket.read_exact(&mut host).await.unwrap();
            Socks5Target::Domain {
                host: String::from_utf8(host).unwrap(),
                port: read_port(&mut socket).await,
            }
        }
        4 => {
            let mut address = [0_u8; 16];
            socket.read_exact(&mut address).await.unwrap();
            Socks5Target::Addr(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(address)),
                read_port(&mut socket).await,
            ))
        }
        address_type => panic!("unsupported address type {address_type}"),
    };
    let _ = observed.send(target.clone());

    let mut upstream = match &target {
        Socks5Target::Domain { host, port } => match domain_upstream {
            Some(address) => TcpStream::connect(address)
                .await
                .expect("the mapped target is reachable"),
            None => TcpStream::connect((host.as_str(), *port))
                .await
                .expect("the requested target is reachable"),
        },
        Socks5Target::Addr(address) => TcpStream::connect(*address)
            .await
            .expect("the requested target is reachable"),
    };
    let bound = upstream.local_addr().unwrap();
    let mut reply = vec![5, 0, 0, 1];
    reply.extend_from_slice(&match bound.ip() {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    });
    reply.extend_from_slice(&bound.port().to_be_bytes());
    socket.write_all(&reply).await.unwrap();

    let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
}

async fn read_port(socket: &mut TcpStream) -> u16 {
    let mut port = [0_u8; 2];
    socket.read_exact(&mut port).await.unwrap();
    u16::from_be_bytes(port)
}

/// Serves one HTTP request with a fixed body, then closes the connection.
pub(crate) async fn spawn_static_http_server(body: &'static str) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        loop {
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
            let mut chunk = [0_u8; 256];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(count) => request.extend_from_slice(&chunk[..count]),
            }
            assert!(request.len() <= 8 * 1024, "the request must stay small");
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });
    (address, task)
}
