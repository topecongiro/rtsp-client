use std::{collections::HashMap, io};
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use http_auth::{PasswordClient, PasswordParams};
use rand::Rng;
use tokio::{net::{TcpStream, UdpSocket}, io::AsyncWriteExt};
use url::Url;
use crate::rtp::RtpStream;

pub(crate) struct RtspClient<'a> {
    cseq: usize,
    config: Config<'a>,
    pub(crate) stream: TcpStream,
    auth: Option<String>,
    rtp_socket: Arc<UdpSocket>,
    pub(crate) rtcp_socket: UdpSocket,
}

impl<'a> RtspClient<'a> {
    pub(crate) async fn new(config: Config<'a>) -> anyhow::Result<RtspClient<'a>> {
        let host_str = config.url.host_str().ok_or(anyhow!("No hostname"))?;
        let port = config.url.port().unwrap_or(554);
        let addr = format!("{}:{}", host_str, port);
        println!("Connecting to {}", host_str);
        let stream = TcpStream::connect(addr).await?;
        let (rtp_socket, rtcp_socket) = bind_udp_pair(stream.local_addr()?.ip()).await?;
        Ok(RtspClient { cseq: 1, config, stream, auth: None, rtp_socket: Arc::new(rtp_socket), rtcp_socket })
    }

    pub(crate) async fn describe(&mut self) -> anyhow::Result<DescribeResponse> {
        let req = self.make_req(RtspMethod::Describe, HashMap::new(), None);
        let raw_resp = self.send(req).await?;
        Ok(DescribeResponse::try_from(raw_resp)?)
    }

    pub(crate) async fn setup(&mut self) -> anyhow::Result<SetupResponse> {
        let headers = HashMap::from([
            ("Transport".to_string(), format!("RTP/AVP/UDP;unicast;client_port={}-{}",
                                              self.rtp_socket.local_addr()?.port(),
                                              self.rtcp_socket.local_addr()?.port())),
        ]);
        let req = self.make_req(RtspMethod::Setup, headers, None);
        let raw_resp = self.send(req).await?;
        let resp = SetupResponse::try_from(raw_resp)?;
        let rtp_addr = format!("{}:{}", self.config.url.host_str().unwrap(), resp.server_port.0);
        let rtcp_addr = format!("{}:{}", self.config.url.host_str().unwrap(), resp.server_port.1);
        println!("Connecting {} & {} to {} & {}", self.rtp_socket.local_addr().unwrap(), self.rtcp_socket.local_addr().unwrap(), rtp_addr, rtcp_addr);
        self.rtp_socket.connect(&rtp_addr).await?;
        self.rtcp_socket.connect(&rtcp_addr).await?;
        for _ in 0..10 {
            punch_firewall_hole(&self.rtp_socket, &self.rtcp_socket).await?;
        }
        Ok(resp)
    }

    pub(crate) async fn play(&mut self, session: u32) -> anyhow::Result<(PlayResponse, RtpStream)> {
        let headers = HashMap::from([
            ("Session".to_string(), session.to_string()),
        ]);
        let req = self.make_req(RtspMethod::Play, headers, None);
        let raw_resp = self.send(req).await?;
        let resp = PlayResponse{ response: raw_resp };
        let rtp_stream = RtpStream::new(self.rtp_socket.clone());
        Ok((resp, rtp_stream))
    }

    fn make_req(&mut self, method: RtspMethod, mut headers: HashMap<String, String>, body: Option<String>) -> RtspRequest {
        headers.insert("CSeq".to_string(), self.cseq.to_string());
        self.cseq += 1;

        let uri = if method == RtspMethod::Setup {
            Url::parse(&format!("{}/trackID=2", self.config.url.as_str())).unwrap()
        } else {
            self.config.url.clone()
        };
        RtspRequest {
            first_line: RtspRequestLine {
                method,
                uri,
                version: self.config.version,
            },
            headers,
            body,
        }
    }

    fn try_auth(&mut self, method: RtspMethod, headers: &HashMap<String, String>) -> anyhow::Result<String> {
        let mut pw_client = PasswordClient::try_from(headers.get("WWW-Authenticate").unwrap().as_str()).map_err(|e| anyhow!(e))?;
        let response = pw_client.respond(&PasswordParams {
            username: &self.config.username,
            password: &self.config.password,
            uri: self.config.url.as_str(),
            method: &method.to_string(),
            body: None,
        }).map_err(|e| anyhow!(e))?;
        Ok(response)
    }

    async fn send(&mut self, mut request: RtspRequest) -> anyhow::Result<RtspResponse> {
        let res = self.send_inner(&mut request).await?;
        if res.first_line.status_code == 401 && self.auth.is_none() {
            let auth = self.try_auth(request.first_line.method, &res.headers)?;
            self.auth = Some(auth);
            self.send_inner(&mut request).await
        } else {
            Ok(res)
        }
    }

    async fn send_inner(&mut self, req: &mut RtspRequest) -> anyhow::Result<RtspResponse> {
        if let Some(auth) = self.auth.take() {
            req.headers.entry("Authorization".to_string()).or_insert_with(|| auth.clone());
        }

        println!("Request\n{}\n", req.to_string().trim_end());
        self.stream.write_all(req.to_string().as_bytes()).await?;
        let resp_buf = self.read_response().await?;
        let resp_str = std::str::from_utf8(&resp_buf)?;
        let res = RtspResponse::parse(resp_str)?;
        Ok(res)
    }

    async fn read_response(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut size = 0;
        let mut buf = vec![0; self.config.buf_size];

        self.stream.readable().await?;
        loop {
            match self.stream.try_read(&mut buf) {
                Ok(n) => {
                    size += n;
                    if n == self.config.buf_size {
                        buf.extend_from_slice(&vec![0; self.config.buf_size]);
                    } else {
                        buf.truncate(size);
                        break;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                // The response size happens to be multiples of `self.buf_size`.
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof && size > 0 => {
                    buf.truncate(size);
                    break;
                }
                Err(e) => return Err(anyhow!(e)),
            }
        }
        Ok(buf)
    }
}

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
enum RtspMethod {
    Describe,
    Options,
    Play,
    Setup,
}

impl ToString for RtspMethod {
    fn to_string(&self) -> String {
        String::from(match self {
            RtspMethod::Describe => "DESCRIBE",
            RtspMethod::Options => "OPTIONS",
            RtspMethod::Play => "PLAY",
            RtspMethod::Setup => "SETUP",
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RtspRequestLine {
    method: RtspMethod,
    uri: Url,
    version: RtspVersion,
}

pub(crate) trait RtspFirstLine: Sized {
    fn parse(line: &str) -> anyhow::Result<Self>;
}

#[derive(Debug, Clone)]
pub(crate) struct RtspMessage<FirstLine> {
    first_line: FirstLine,
    headers: HashMap<String, String>,
    body: Option<String>,
}

pub(crate) type RtspRequest = RtspMessage<RtspRequestLine>;

#[derive(Debug)]
pub(crate) struct DescribeResponse {
    response: RtspResponse,
    sdp_session: sdp_types::Session,
}

impl DescribeResponse {
    pub(crate) fn session(&self) -> &sdp_types::Session {
        &self.sdp_session
    }
}

impl TryFrom<RtspResponse> for DescribeResponse {
    type Error = sdp_types::ParserError;

    fn try_from(response: RtspResponse) -> Result<Self, Self::Error> {
        if let Some(ref body) = response.body {
            let sdp_session = sdp_types::Session::parse(body.as_bytes())?;
            Ok(DescribeResponse { sdp_session, response })
        } else {
            // Empty body
            Err(sdp_types::ParserError::NoVersion)
        }
    }
}

#[derive(Debug)]
pub(crate) struct PlayResponse {
    response: RtspResponse,
}

#[derive(Debug)]
pub(crate) struct SetupResponse {
    response: RtspResponse,
    server_port: (i32, i32),
    session: (u32, usize), // (Session ID, timeout)
}

impl SetupResponse {
    pub(crate) fn timeout(&self) -> usize {
        self.session.1
    }

    pub(crate) fn session_id(&self) -> u32 {
        self.session.0
    }

    pub(crate) fn rtp_port(&self) -> i32 {
        self.server_port.0
    }

    pub(crate) fn rtcp_port(&self) -> i32 {
        self.server_port.1
    }
}

impl TryFrom<RtspResponse> for SetupResponse {
    type Error = anyhow::Error;

    fn try_from(response: RtspResponse) -> Result<Self, Self::Error> {
        // terrible :(
        let transport = response.headers.get("Transport").ok_or_else(|| anyhow!("No Transport header field"))?;
        let server_port = transport.split(";").find(|s| s.starts_with("server_port=")).ok_or_else(|| anyhow!("No server_port found in Transport header field"))?;
        let mut iter = server_port.trim_start_matches("server_port=").split("-").map(str::parse::<i32>).take(2);
        let rtp_port = iter.next().ok_or_else(|| anyhow!("No rtp port"))??;
        let rtcp_port = iter.next().ok_or_else(|| anyhow!("No rtcp port"))??;
        let session = if let Some(mut session_line) = response.headers.get("Session").map(|s| s.split(";")) {
            let session_id = session_line.next().ok_or(anyhow!("no session id"))?.parse::<u32>()?;
            let timeout = session_line.next().and_then(|s| s.trim_start_matches("timeout=").parse::<usize>().ok()).unwrap_or(60);
            (session_id, timeout)
        } else {
            bail!("no session id");
        };

        Ok(SetupResponse {
            response,
            server_port: (rtp_port, rtcp_port),
            session,
        })
    }
}

impl ToString for RtspRequestLine {
    fn to_string(&self) -> String {
        format!("{} {} {}", self.method.to_string(), self.uri, self.version.to_string())
    }
}

pub(crate) struct Config<'a> {
    url: Url,
    username: &'a str,
    password: &'a str,
    buf_size: usize,
    version: RtspVersion,
}

impl<'a> Config<'a> {
    pub(crate) fn new(url: &'a str, username: &'a str, password: &'a str, buf_size: usize) -> anyhow::Result<Self> {
        Ok(Config {
            url: Url::parse(url)?,
            username,
            password,
            buf_size,
            version: RtspVersion::Rtsp10,
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
enum RtspVersion {
    Rtsp10,
    Rtsp20,
}

impl ToString for RtspVersion {
    fn to_string(&self) -> String {
        match self {
            RtspVersion::Rtsp10 => "RTSP/1.0".to_owned(),
            RtspVersion::Rtsp20 => "RTSP/2.0".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RtspStatusLine {
    version: RtspVersion,
    status_code: u32,
    msg: Option<String>,
}

impl RtspFirstLine for RtspStatusLine {
    fn parse(first_line: &str) -> anyhow::Result<Self> {
        let first_line_elems = first_line.split_whitespace().collect::<Vec<_>>();
        if first_line_elems.len() < 2 {
            bail!("Invalid RTSP status line: {}", first_line);
        }
        let version = match first_line_elems[0] {
            "RTSP/1.0" => RtspVersion::Rtsp10,
            "RTSP/2.0" => RtspVersion::Rtsp20,
            _ => bail!("Invalid RTSP version: {}", first_line_elems[0]),
        };
        let status_code = first_line_elems[1].parse()?;
        let msg = if first_line_elems.len() == 2 {
            None
        } else {
            Some(first_line_elems[2..].join(" "))
        };

        Ok(RtspStatusLine {
            version,
            status_code,
            msg,
        })
    }
}

impl ToString for RtspStatusLine {
    fn to_string(&self) -> String {
        format!("{} {}{}", self.version.to_string(), self.status_code, self.msg.as_ref().map_or_else(String::new, |msg| format!(" {}", msg)))
    }
}

pub(crate) type RtspResponse = RtspMessage<RtspStatusLine>;

impl<FirstLine> RtspMessage<FirstLine>
    where
        FirstLine: RtspFirstLine,
{
    // `buf` must contain the whole RTSP response.
    fn parse(buf: &str) -> anyhow::Result<Self> {
        let mut lines = buf.split("\r\n");
        let first_line = FirstLine::parse(lines.next().ok_or(anyhow!("No CRLF for the first line"))?)?;

        let mut headers = HashMap::new();
        let mut observed_empty_line = false;
        while let Some(line) = lines.next() {
            if line.is_empty() {
                observed_empty_line = true;
                break;
            }

            if let Some(sep_index) = line.find(":") {
                let (key, val) = line.split_at(sep_index);
                headers.insert(key.trim().to_string(), val[1..].trim().to_string());
            } else {
                bail!("Invalid header line: {}", line);
            }
        }

        if !observed_empty_line {
            bail!("No CRLF after headers");
        }
        let body = Some(lines.fold(String::new(), |mut acc, l| { acc.push_str(l); acc.push_str("\r\n"); acc } ));

        Ok(RtspMessage { first_line, headers, body })
    }
}

impl<T> ToString for RtspMessage<T>
    where
        T: ToString,
{
    fn to_string(&self) -> String {
        let mut s = self.first_line.to_string();
        s.push_str("\r\n");
        for (key, val) in &self.headers {
            s.push_str(&format!("{}: {}\r\n", key, val));
        }
        s.push_str("\r\n");
        if let Some(body) = &self.body {
            s.push_str(body);
            s.push_str("\r\n");
        }
        s
    }
}

async fn bind_udp_pair(ip_addr: IpAddr) -> anyhow::Result<(UdpSocket, UdpSocket)> {
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

        return Ok((rtp_socket, rtcp_socket));
    }
}

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
    println!("punch_firewall_hole");
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse() {
        let response = RtspResponse::parse("RTSP/1.0 200 OK\r\nCSeq: 312\r\nAccept: application/sdp, application/rtsl, application/mheg\r\n\r\n").unwrap();
        assert_eq!(response.first_line.version, RtspVersion::Rtsp10);
        assert_eq!(response.first_line.status_code, 200);
        assert_eq!(response.first_line.msg, Some("OK".to_string()));
        assert_eq!(response.headers.len(), 2);

        assert!(RtspResponse::parse("RTSP/1.0 200 OK\r\nCSeq: 312\r\nAccept: application/sdp, application/rtsl, application/mheg\r\n").is_err());
        assert!(RtspResponse::parse("RTSP/1.0 200 OK\r\nCSeq: 312\r\nAccept: application/sdp, application/rtsl, application/mheg").is_err());
        assert!(RtspResponse::parse("RTSP/1.0 200 OK\r\nCSeq: 312\r\nAccept application/sdp, application/rtsl, application/mheg\r\n\r\n").is_err());
        assert!(RtspResponse::parse("RTSP/1.0 OK\r\nCSeq: 312\r\nAccept: application/sdp, application/rtsl, application/mheg").is_err());
    }
}
