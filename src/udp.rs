use crate::ResultType;
use anyhow::{anyhow, Context};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use protobuf::Message;
use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;
use tokio::net::{lookup_host, ToSocketAddrs, UdpSocket};
use tokio_socks::{udp::Socks5UdpFramed, IntoTargetAddr, TargetAddr, ToProxyAddrs};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};
use std::fmt::Debug;

pub enum FramedSocket {
    Direct(UdpFramed<BytesCodec>),
    ProxySocks(Socks5UdpFramed),
}

fn new_socket(addr: SocketAddr, reuse: bool, buf_size: usize) -> Result<Socket, std::io::Error> {
    let socket = match addr {
        SocketAddr::V4(..) => Socket::new(Domain::ipv4(), Type::dgram(), None),
        SocketAddr::V6(..) => Socket::new(Domain::ipv6(), Type::dgram(), None),
    }?;
    if reuse {
        // windows has no reuse_port, but its reuse_address
        // almost equals to unix's reuse_port + reuse_address,
        // though may introduce nondeterministic behavior
        // illumos has no support for SO_REUSEPORT
        #[cfg(all(unix, not(target_os = "illumos")))]
        socket.set_reuse_port(true).ok();
        socket.set_reuse_address(true).ok();
    }
    // only nonblocking work with tokio, https://stackoverflow.com/questions/64649405/receiver-on-tokiompscchannel-only-receives-messages-when-buffer-is-full
    socket.set_nonblocking(true)?;
    if buf_size > 0 {
        socket.set_recv_buffer_size(buf_size).ok();
    }
    log::debug!(
        "Receive buf size of udp {}: {:?}",
        addr,
        socket.recv_buffer_size()
    );
    if addr.is_ipv6() && addr.ip().is_unspecified() && addr.port() > 0 {
        socket.set_only_v6(false).ok();
    }
    match socket.bind(&addr.into()) {
        Ok(_) => {
            log::info!("UDP socket bound to {}", addr);
        }
        Err(e) => {
            log::error!("Failed to bind UDP socket to {}: {}", addr, e);
            return Err(e);
        }
    }

    Ok(socket)
}

impl FramedSocket {
    pub async fn new<T: ToSocketAddrs>(addr: T) -> ResultType<Self> {
        Self::new_reuse(addr, false, 0).await
    }

    pub async fn new_reuse<T: ToSocketAddrs>(
		addr: T,
		reuse: bool,
		buf_size: usize,
	) -> ResultType<Self> {
		let resolved = match lookup_host(&addr).await?.next() {
			Some(a) => a,
			None => {
				let addr_log = match lookup_host(&addr).await {
					Ok(mut iter) => iter.next().map(|a| a.to_string()).unwrap_or_else(|| "<empty iterator>".to_string()),
					Err(_) => "<failed to resolve>".to_string(),
				};
			
				log::info!("UDP attempting bind to: {}", addr_log);
			
				return Err(anyhow!("Could not resolve any address"));
			}
			
		};
	
		log::debug!("Resolved UDP bind address: {}", resolved);
	
		let std_socket = match new_socket(resolved, reuse, buf_size) {
			Ok(sock) => sock,
			Err(e) => {
				log::error!("Failed to create new UDP socket: {}", e);
				return Err(e.into());
			}
		};
	
		let std_udp = std_socket.into_udp_socket(); // ✅ Extract std socket
		let udp_socket = UdpSocket::from_std(std_udp)?; // ✅ Use from_std and apply `?`
		log::info!("UDP socket created on {}", udp_socket.local_addr()?); // ✅ This now works
	
		Ok(Self::Direct(UdpFramed::new(udp_socket, BytesCodec::new())))
	}
	
	

    pub async fn new_proxy<'a, 't, P: ToProxyAddrs, T: ToSocketAddrs>(
        proxy: P,
        local: T,
        username: &'a str,
        password: &'a str,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let framed = if username.trim().is_empty() {
            super::timeout(ms_timeout, Socks5UdpFramed::connect(proxy, Some(local))).await??
        } else {
            super::timeout(
                ms_timeout,
                Socks5UdpFramed::connect_with_password(proxy, Some(local), username, password),
            )
            .await??
        };
        log::trace!(
            "Socks5 udp connected, local addr: {:?}, target addr: {}",
            framed.local_addr(),
            framed.socks_addr()
        );
        Ok(Self::ProxySocks(framed))
    }

    #[inline]
    pub async fn send(
        &mut self,
        msg: &impl Message,
        addr: impl IntoTargetAddr<'_>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();
        let send_data = Bytes::from(msg.write_to_bytes()?);
        match self {
            Self::Direct(f) => {
                if let TargetAddr::Ip(addr) = addr {
                    f.send((send_data, addr)).await?
                }
            }
            Self::ProxySocks(f) => f.send((send_data, addr)).await?,
        };
        Ok(())
    }

    // https://stackoverflow.com/a/68733302/1926020
    #[inline]
    pub async fn send_raw(
        &mut self,
        msg: &'static [u8],
        addr: impl IntoTargetAddr<'static>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();

        match self {
            Self::Direct(f) => {
                if let TargetAddr::Ip(addr) = addr {
                    f.send((Bytes::from(msg), addr)).await?
                }
            }
            Self::ProxySocks(f) => f.send((Bytes::from(msg), addr)).await?,
        };
        Ok(())
    }

    #[inline]
    pub async fn next(&mut self) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        match self {
            Self::Direct(f) => match f.next().await {
                Some(Ok((data, addr))) => {
                    Some(Ok((data, addr.into_target_addr().ok()?.to_owned())))
                }
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
            Self::ProxySocks(f) => match f.next().await {
                Some(Ok((data, _))) => Some(Ok((data.data, data.dst_addr))),
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
        }
    }

    #[inline]
    pub async fn next_timeout(
        &mut self,
        ms: u64,
    ) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        if let Ok(res) =
            tokio::time::timeout(std::time::Duration::from_millis(ms), self.next()).await
        {
            res
        } else {
            None
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        if let FramedSocket::Direct(x) = self {
            if let Ok(v) = x.get_ref().local_addr() {
                return Some(v);
            }
        }
        None
    }
}
