use std::{
    io,
    net::{IpAddr, SocketAddr},
    ops::RangeInclusive,
    sync::Arc,
};

use anyhow::{anyhow, bail};
use rand::Rng;
use tokio::net::{ToSocketAddrs, UdpSocket};
use url::Url;

use crate::Config;

pub(crate) struct DataStream {
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Arc<UdpSocket>,
    ports: RtpRtcpPorts,
}

pub(crate) struct RtpRtcpPorts {
    local: (u16, u16),
    peer: (u16, u16),
}

impl RtpRtcpPorts {
    pub(crate) fn new(local: (u16, u16), peer: (u16, u16)) -> Self {
        RtpRtcpPorts { local, peer }
    }
}

impl DataStream {
    pub(crate) fn local_rtp_port(&self) -> u16 {
        self.ports.local.0
    }

    pub(crate) fn local_rtcp_port(&self) -> u16 {
        self.ports.local.1
    }

    pub(crate) fn new(rtp_socket: UdpSocket, rtcp_socket: UdpSocket, ports: RtpRtcpPorts) -> DataStream {
        DataStream {
            rtp_socket: Arc::new(rtp_socket),
            rtcp_socket: Arc::new(rtcp_socket),
            ports,
        }
    }

    pub(crate) async fn bind_udp_pair(ip_addr: IpAddr) -> anyhow::Result<((UdpSocket, UdpSocket), (u16, u16))> {
        const PORT_RANGE: RangeInclusive<u16> = 16384..=32767;
        let mut rng = rand::thread_rng();
        loop {
            let rtp_port = rng.gen_range(PORT_RANGE) & !1u16;
            let rtp_socket = match UdpSocket::bind(SocketAddr::new(ip_addr, rtp_port)).await {
                Ok(sock) => sock,
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                    continue;
                }
                Err(e) => bail!(e),
            };

            let rtcp_port = rtp_port + 1;
            let rtcp_socket = match UdpSocket::bind(SocketAddr::new(ip_addr, rtcp_port)).await {
                Ok(sock) => sock,
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => continue,
                Err(e) => bail!(e),
            };

            return Ok(((rtp_socket, rtcp_socket), (rtp_port, rtcp_port)));
        }
    }

}

// https://github.com/scottlamb/retina/blob/d50e7481eef95ab0fbb073dd949a12f81efd34f6/src/client/mod.rs#L1460
async fn punch_firewall_hole(
    rtp_socket: &UdpSocket,
    rtcp_socket: &UdpSocket,
) -> Result<(), std::io::Error> {
    #[rustfmt::skip]
    const DUMMY_RTP: [u8; 12] = [
        2 << 6,     // version=2 + p=0 + x=0 + cc=0
        0,          // m=0 + pt=0
        0, 0,       // sequence number=0
        0, 0, 0, 0, // timestamp=0 0,
        0, 0, 0, 0, // ssrc=0
    ];
    #[rustfmt::skip]
    const DUMMY_RTCP: [u8; 8] = [
        2 << 6,     // version=2 + p=0 + rc=0
        200,        // pt=200 (reception report)
        0, 1,       // length=1 (in 4-byte words minus 1)
        0, 0, 0, 0, // ssrc=0 (bogus but we don't know the ssrc reliably yet)
    ];
    rtp_socket.send(&DUMMY_RTP[..]).await?;
    rtcp_socket.send(&DUMMY_RTCP[..]).await?;
    Ok(())
}

