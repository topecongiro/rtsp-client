use std::{collections::HashMap, io};
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, bail};
use http_auth::{PasswordClient, PasswordParams};
use log::trace;
use rand::Rng;
use tokio::{net::{TcpStream, UdpSocket}, io::AsyncWriteExt};
use tokio::sync::RwLock;
use url::{Host, Url};

use crate::rtp::RtpStream;
use crate::rtsp::data_stream::{DataStream, RtpRtcpPorts};

mod data_stream;

const RTSP_DEFAULT_PORT: u16 = 554;

pub(crate) struct RtspClient {
    config: Config,
    video_streams: VideoStreamTable,
}

pub(crate) struct RtspCredential {
    username: String,
    password: String,
}

impl RtspClient {
    pub(crate) async fn new(config: Config) -> anyhow::Result<RtspClient> {
        Ok(RtspClient {
            config,
            video_streams: VideoStreamTable::default(),
        })
    }

    pub(crate) async fn connect(&self, url: Url) -> anyhow::Result<Arc<VideoStream>> {
       let mut video_streams = self.video_streams.video_streams.write().await;
        match video_streams.entry(url.host().ok_or(anyhow!("no host in url"))?.to_owned()) {
            Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
            Entry::Vacant(vacant) => {
                let video_stream = Arc::new(VideoStream::connect(url, &self.config).await?);
                vacant.insert(Arc::clone(&video_stream));
                Ok(video_stream)
            }
        }
    }
}

#[derive(Default)]
struct VideoStreamTable {
    video_streams: RwLock<BTreeMap<Host, Arc<VideoStream>>>,
}

/// A video stream on top of RTSP connection.
pub(crate) struct VideoStream {
    rtsp_stream: RtspStream,
    data_stream: DataStream,
}

impl VideoStream {
    pub(crate) async fn connect(url: Url, config: &Config) -> anyhow::Result<VideoStream> {
        let mut rtsp_stream = RtspStream::new(url, config).await?;

        let local_ip_addr = rtsp_stream.stream.local_addr()?.ip();
        let peer_ip_addr = rtsp_stream.stream.peer_addr()?.ip();

        let ((rtp_socket, rtcp_socket), local_ports) = DataStream::bind_udp_pair(local_ip_addr).await?;
        let setup = rtsp_stream.setup(local_ports.0, local_ports.1).await?;
        rtp_socket.connect(SocketAddr::new(peer_ip_addr, setup.rtp_port())).await?;
        rtcp_socket.connect(SocketAddr::new(peer_ip_addr, setup.rtcp_port())).await?;

        let data_stream = DataStream::new(rtp_socket, rtcp_socket, RtpRtcpPorts::new(local_ports, setup.peer_ports));

        Ok(VideoStream {
            rtsp_stream,
            data_stream,
        })
    }
}

pub(crate) struct RtspStream {
    cseq: AtomicUsize,
    track_id: String,
    url: Url,
    stream: TcpStream,
    version: RtspVersion,
    buf_size: usize,
    credential: RtspCredential,
}

impl RtspStream {
    pub(crate) async fn new(url: Url, config: &Config) -> anyhow::Result<RtspStream> {
        let cseq = AtomicUsize::new(0);
        let host_str = url.host_str().ok_or(anyhow!("No hostname"))?;
        let port = url.port().unwrap_or(RTSP_DEFAULT_PORT);
        let addr = format!("{}:{}", host_str, port);
        let stream = TcpStream::connect(addr).await?;

        let mut rtsp_stream = RtspStream {
            cseq,
            stream,
            url,
            track_id: String::new(),
            version: RtspVersion::Rtsp10,
            buf_size: config.buf_size,
            credential: RtspCredential {
                username: config.username.clone(),
                password: config.password.clone(),
            },
        };

        let describe_response = rtsp_stream.describe().await?;
        let track_id = describe_response
            .sdp_session.medias.into_iter().find(|m| m.media == "video")
            .and_then(|m| m.attributes.into_iter().find(|a| a.attribute == "control"))
            .and_then(|a| a.value)
            .ok_or(anyhow!("No video media found in describe response"))?;
        rtsp_stream.track_id = track_id;

        Ok(rtsp_stream)
    }

    pub(crate) async fn describe(&mut self) -> anyhow::Result<DescribeResponse> {
        let req = self.make_req(RtspMethod::Describe, HashMap::new(), None);
        let raw_resp = self.send(req).await?;
        Ok(DescribeResponse::try_from(raw_resp)?)
    }

    pub(crate) async fn setup(&mut self, rtp_port: u16, rtcp_port: u16) -> anyhow::Result<SetupResponse> {
        let headers = HashMap::from([
            ("Transport".to_string(), format!("RTP/AVP/UDP;unicast;client_port={}-{}", rtp_port, rtcp_port)),
        ]);
        let req = self.make_req(RtspMethod::Setup, headers, None);
        let raw_resp = self.send(req).await?;
        let resp = SetupResponse::try_from(raw_resp)?;
        Ok(resp)
    }

    pub(crate) async fn play(&mut self, session: u32) -> anyhow::Result<PlayResponse> {
        let headers = HashMap::from([
            ("Session".to_string(), session.to_string()),
        ]);
        let req = self.make_req(RtspMethod::Play, headers, None);
        let raw_resp = self.send(req).await?;
        Ok(PlayResponse{ response: raw_resp })
    }

    fn make_req(&mut self, method: RtspMethod, mut headers: HashMap<String, String>, body: Option<String>) -> RtspRequest {
        let cseq = self.cseq.fetch_add(1, Ordering::SeqCst);
        headers.insert("CSeq".to_string(), cseq.to_string());

        let uri = if method == RtspMethod::Setup {
            Url::parse(&format!("{}/{}", self.url.as_str(), self.track_id)).unwrap()
        } else {
            self.url.clone()
        };
        RtspRequest {
            first_line: RtspRequestLine {
                method,
                uri,
                version: self.version,
            },
            headers,
            body,
        }
    }

    fn try_auth(&mut self, request_line: &RtspRequestLine, headers: &HashMap<String, String>) -> anyhow::Result<String> {
        let mut pw_client = PasswordClient::try_from(headers.get("WWW-Authenticate").unwrap().as_str()).map_err(|e| anyhow!(e))?;
        let response = pw_client.respond(&PasswordParams {
            username: &self.credential.username,
            password: &self.credential.password,
            uri: request_line.uri.as_str(),
            method: request_line.method.as_str(),
            body: None,
        }).map_err(|e| anyhow!(e))?;
        Ok(response)
    }

    async fn send(&mut self, mut request: RtspRequest) -> anyhow::Result<RtspResponse> {
        let res = self.send_inner(&mut request, None).await?;
        if res.first_line.status_code == 401 {
            let auth = self.try_auth(&request.first_line, &res.headers)?;
            self.send_inner(&mut request, Some(auth)).await
        } else {
            Ok(res)
        }
    }

    async fn send_inner(&mut self, req: &mut RtspRequest, auth: Option<String>) -> anyhow::Result<RtspResponse> {
        if let Some(auth) = auth {
            req.headers.entry("Authorization".to_string()).or_insert_with(|| auth.clone());
        }

        trace!("Sending request:\n{}\n", req.to_string().trim_end());
        self.stream.write_all(req.to_string().as_bytes()).await?;
        let resp_buf = self.read_response().await?;
        let resp_str = std::str::from_utf8(&resp_buf)?;
        let res = RtspResponse::parse(resp_str)?;
        trace!("Received response:\n{}", res.to_string());
        Ok(res)
    }

    // FIXME: support interleaved connection.
    // FIXME: Support response received in different order.
    async fn read_response(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut size = 0;
        let mut buf = vec![0; self.buf_size];

        self.stream.readable().await?;
        loop {
            match self.stream.try_read(&mut buf) {
                Ok(n) => {
                    size += n;
                    if n == self.buf_size {
                        buf.extend_from_slice(&vec![0; self.buf_size]);
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

impl RtspMethod {
    fn as_str(&self) -> &'static str {
        match self {
            RtspMethod::Describe => "DESCRIBE",
            RtspMethod::Options => "OPTIONS",
            RtspMethod::Play => "PLAY",
            RtspMethod::Setup => "SETUP",
        }
    }
}

impl ToString for RtspMethod {
    fn to_string(&self) -> String {
        String::from(self.as_str())
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
    peer_ports: (u16, u16),
    session: (u32, usize), // (Session ID, timeout)
}

impl SetupResponse {
    pub(crate) fn timeout(&self) -> usize {
        self.session.1
    }

    pub(crate) fn session_id(&self) -> u32 {
        self.session.0
    }

    pub(crate) fn rtp_port(&self) -> u16 {
        self.peer_ports.0
    }

    pub(crate) fn rtcp_port(&self) -> u16 {
        self.peer_ports.1
    }
}

impl TryFrom<RtspResponse> for SetupResponse {
    type Error = anyhow::Error;

    fn try_from(response: RtspResponse) -> Result<Self, Self::Error> {
        // terrible :(
        let transport = response.headers.get("Transport").ok_or_else(|| anyhow!("No Transport header field"))?;
        let server_port = transport.split(";").find(|s| s.starts_with("server_port=")).ok_or_else(|| anyhow!("No server_port found in Transport header field"))?;
        let mut iter = server_port.trim_start_matches("server_port=").split("-").map(str::parse::<u16>).take(2);
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
            peer_ports: (rtp_port, rtcp_port),
            session,
        })
    }
}

impl ToString for RtspRequestLine {
    fn to_string(&self) -> String {
        format!("{} {} {}", self.method.to_string(), self.uri, self.version.to_string())
    }
}

pub(crate) struct Config {
    username: String,
    password: String,
    buf_size: usize,
    version: RtspVersion,
}

impl Config {
    pub(crate) fn new(username: String, password: String, buf_size: usize) -> anyhow::Result<Self> {
        Ok(Config {
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
