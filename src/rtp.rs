use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use thiserror::Error;
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

use crate::rtp::packet::{RtpPacket, RtpPacketParseError};

pub(crate) mod packet;

pub(crate) struct RtpStream {
    socket: Arc<UdpSocket>,
}

#[derive(Debug, Error)]
pub(crate) enum RtpStreamError {
    #[error("Failed to parse RTP packet: {0}")]
    ParseError(#[from] RtpPacketParseError),
    #[error("Io error: {0}")]
    Io(#[from] io::Error)
}

impl RtpStream {
    pub(crate) fn new(socket: Arc<UdpSocket>) -> Self {
        RtpStream { socket }
    }
}

impl tokio_stream::Stream for RtpStream {
    type Item = Result<RtpPacket, RtpStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut buf: [MaybeUninit<u8>; 1500] = MaybeUninit::uninit_array();
        let mut buf = ReadBuf::uninit(&mut buf);
        println!("start polling");
        match self.socket.poll_recv(cx, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_)) =>  {
                let bytes = Bytes::copy_from_slice(buf.filled());
                Poll::Ready(Some(RtpPacket::try_from(bytes).map_err(RtpStreamError::ParseError)))
            }
            Poll::Ready(Err(e)) => match e.kind() {
                io::ErrorKind::NotConnected | io::ErrorKind::ConnectionAborted => Poll::Ready(None),
                _ => Poll::Ready(Some(Err(RtpStreamError::Io(e)))),
            }
        }
    }
}

