use std::{collections::HashMap, io};

use anyhow::{anyhow, bail};
use clap::Parser;
use http_auth::{PasswordClient, PasswordParams};
use tokio::{net::TcpStream, io::AsyncWriteExt};
use url::Url;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    username: String,

    #[clap(short, long)]
    password: String,

    url: String,
}

struct RtspClient {
    cseq: usize,
    config: Config,
    stream: TcpStream,
    auth: Option<String>,
}

impl RtspClient {
    async fn new(config: Config) -> anyhow::Result<Self> {
        let host_str = config.url.host_str().ok_or(anyhow!("No hostname"))?;
        let port = config.url.port().unwrap_or(554);
        let addr = format!("{}:{}", host_str, port);
        println!("Connecting to {}", host_str);
        let stream = TcpStream::connect(addr).await?;
        Ok(RtspClient { cseq: 1, config, stream, auth: None })
    }

    async fn describe(&mut self) -> anyhow::Result<RtspResponse> {
        let req = self.make_req(RtspMethod::Describe, HashMap::new(), None);
        Ok(self.send(req).await?)
    }

    fn make_req(&mut self, method: RtspMethod, mut headers: HashMap<String, String>, body: Option<String>) -> RtspRequest {
        headers.insert("CSeq".to_string(), self.cseq.to_string());
        self.cseq += 1;

        if let Some(auth) = &self.auth {
            headers.insert("Authorization".to_string(), auth.clone());
        }
        RtspRequest {
            first_line: RtspRequestLine {
                method,
                uri: self.config.url.clone(),
                version: self.config.version,
            },
            headers,
            body,
        }
    }

    fn try_auth(&mut self, headers: &HashMap<String, String>) -> anyhow::Result<String> {
        let mut pw_client = PasswordClient::try_from(headers.get("WWW-Authenticate").unwrap().as_str()).map_err(|e| anyhow!(e))?;
        let response = pw_client.respond(&PasswordParams {
            username: &self.config.username,
            password: &self.config.password,
            uri: self.config.url.as_str(),
            method: "DESCRIBE",
            body: None,
        }).map_err(|e| anyhow!(e))?;
        Ok(response)
    }

    async fn send(&mut self, mut request: RtspRequest) -> anyhow::Result<RtspResponse> {
        let res = self.send_inner(&mut request).await?;
        if res.first_line.status_code == 401 && self.auth.is_none() {
            let auth = self.try_auth(&res.headers)?;
            self.auth = Some(auth);
            self.send_inner(&mut request).await
        } else {
            Ok(res)
        }
    }

    async fn send_inner(&mut self, req: &mut RtspRequest) -> anyhow::Result<RtspResponse> {
        if let Some(auth) = &self.auth {
            req.headers.entry("Authorization".to_string()).or_insert_with(|| auth.clone());
        }

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

#[derive(Debug, Clone, Copy)]
enum RtspMethod {
    Describe,
    Options,
    Play,
}

impl ToString for RtspMethod {
    fn to_string(&self) -> String {
        String::from(match self {
            RtspMethod::Describe => "DESCRIBE",
            RtspMethod::Options => "OPTIONS",
            RtspMethod::Play => "PLAY",
        })
    }
}

#[derive(Debug, Clone)]
struct RtspRequestLine {
    method: RtspMethod,
    uri: Url,
    version: RtspVersion,    
}

trait RtspFirstLine: Sized {
    fn parse(line: &str) -> anyhow::Result<Self>;
}

#[derive(Debug, Clone)]
struct RtspMessage<FirstLine> {
    first_line: FirstLine,
    headers: HashMap<String, String>,
    body: Option<String>,
}

type RtspRequest = RtspMessage<RtspRequestLine>;

impl ToString for RtspRequestLine {
    fn to_string(&self) -> String {
        format!("{} {} {}", self.method.to_string(), self.uri, self.version.to_string())
    }
}

struct Config {
    url: Url,
    username: String,
    password: String,
    buf_size: usize,
    version: RtspVersion,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();
    let url = Url::parse(&args.url)?;

    let config = Config {
        url,
        username: args.username,
        password: args.password,
        buf_size: 1024,
        version: RtspVersion::Rtsp10,
    };

    let mut client = RtspClient::new(config).await?;
    let resp = client.describe().await?;
    println!("{:?}", resp);
    Ok(())
}

async fn describe<W: AsyncWriteExt + Unpin>(url: &Url, cseq: usize, auth: Option<String>, mut w: W) -> anyhow::Result<()> {
    let mut buf = format!("DESCRIBE {} RTSP/1.0\r\nCSeq: {}\r\n", url, cseq);
    if let Some(auth) = auth {
        buf.push_str(&format!("Authorization: {}\r\n", auth));
    }
    buf.push_str("\r\n");
    w.write_all(buf.as_bytes()).await?;
    Ok(())
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
struct RtspStatusLine {
    version: RtspVersion,
    status_code: u32,
    msg: Option<String>,
}

impl RtspFirstLine for RtspStatusLine {
    fn parse(first_line: &str) -> anyhow::Result<Self> {
        let first_line_elems = first_line.split_whitespace().collect::<Vec<_>>();
        if !(first_line_elems.len() == 2  || first_line_elems.len() == 3) {
            bail!("Invalid RTSP status line: {}", first_line);
        }
        let version = match first_line_elems[0] {
            "RTSP/1.0" => RtspVersion::Rtsp10,
            "RTSP/2.0" => RtspVersion::Rtsp20,
            _ => bail!("Invalid RTSP version: {}", first_line_elems[0]),
        };
        let status_code = first_line_elems[1].parse()?;
        let msg = first_line_elems.get(2).map(|s| s.to_string());

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

type RtspResponse = RtspMessage<RtspStatusLine>;

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
        if lines.next().is_none() {
            bail!("No empty line after headers")
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