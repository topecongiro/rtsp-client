#![feature(maybe_uninit_uninit_array)]

use std::time::Duration;
use clap::Parser;
use tokio::select;
use tokio::io::AsyncReadExt;
use tokio::time::interval;
use tokio_stream::StreamExt;

use crate::rtsp::{Config, RtspClient};

mod rtp;
mod rtsp;

#[derive(Debug, Parser)]
struct Args {
    #[clap(short, long)]
    username: String,

    #[clap(short, long)]
    password: String,

    url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Args = Args::parse();
    let config = Config::new(&args.url, &args.username, &args.password, 1024)?;

    let mut client = RtspClient::new(config).await?;
    let resp = client.describe().await?;
    if let Some(video_media) = resp.session().medias.iter().find(|m| m.media == "video") {
        if let Some(control_attr) = video_media.attributes.iter().find(|attr| attr.attribute == "control") {
            let track_id = control_attr.value.as_ref().map_or("", String::as_str);
            println!("track ID for video is {}", track_id);
        }
    }

    println!("{:#?}", resp);
    let resp = client.setup().await?;
    println!("{:#?}", resp);
    let (resp, mut stream) = client.play(resp.session_id()).await?;
    println!("{:#?}", resp);

    let mut buf = vec![0u8; 1500];
    let mut buf2 = vec![0u8; 1500];
    let mut interval = interval(Duration::from_secs(5));
    loop {
        select! {
            rtsp_data = client.stream.read(&mut buf2) => {
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

    Ok(())
}