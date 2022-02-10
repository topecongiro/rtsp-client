#![feature(maybe_uninit_uninit_array)]

use std::time::Duration;
use clap::Parser;
use log::{LevelFilter, trace};
use tokio::select;
use tokio::io::AsyncReadExt;
use tokio::time::interval;
use tokio_stream::StreamExt;
use url::Url;

use crate::rtsp::{Config, RtspClient};

mod rtp;
mod rtsp;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    username: String,

    #[clap(short, long)]
    password: String,

    #[clap(short, long)]
    verbose: bool,

    url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();

    if args.verbose {
        env_logger::builder()
            .filter_module(env!("CARGO_PKG_NAME"), LevelFilter::Trace)
            .try_init();
    } else {
        env_logger::init();
    }

    let url = Url::parse(&args.url)?;
    let config = Config::new(args.username, args.password, 1024)?;
    trace!("url: {}", url);
    let mut client = RtspClient::new(config).await?;

    let video_stream = client.connect(url).await?;

    let mut buf = vec![0u8; 1500];
    let mut buf2 = vec![0u8; 1500];
    let mut interval = interval(Duration::from_secs(5));
    /*
    loop {
        select! {
            rtsp_data = video_stream.read(&mut buf2) => {
                println!("received RTSP data\n{}", String::from_utf8_lossy(&buf2));
            }
            rtp_packet = stream.next() => {
                if let Some(maybe_packet) = rtp_packet {
                    match maybe_packet {
                        Ok(p) => println!("Received RTP packet: {:?}", p),
                        Err(e) => eprintln!("Error from RTP stream: {:?}", e),
                    }
                } else {
                    eprintln!("RTP stream has finished");
                    break;
                }
            }
            rtcp_packet = client.rtcp_socket.recv(&mut buf) => {
                println!("received RTCP packet\n{}", String::from_utf8_lossy(&buf));
            }
            _ = interval.tick() => {
                println!("Timeout");
            }
        }
    }
     */

    Ok(())
}