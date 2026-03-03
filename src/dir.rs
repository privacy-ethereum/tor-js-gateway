//! Raw directory protocol requests over BEGINDIR streams.

use anyhow::{Context, Result, bail};
use futures::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tor_circmgr::DirInfo;
use tor_netdir::Timeliness;

use arti_client::TorClient;

/// Fetch raw bytes from a directory cache via a BEGINDIR stream.
///
/// Uses the TorClient's circuit manager to get a managed dir circuit,
/// opens a BEGINDIR stream, and sends a raw HTTP/1.0 GET request.
/// The response body is decompressed automatically.
pub async fn get(
    client: &TorClient<tor_rtcompat::PreferredRuntime>,
    path: &str,
) -> Result<Vec<u8>> {
    let netdir = client
        .dirmgr()
        .netdir(Timeliness::Timely)
        .map_err(|e| anyhow::anyhow!("getting network directory: {}", e))?;

    let dir_tunnel = client
        .circmgr()
        .get_or_launch_dir(DirInfo::Directory(&netdir))
        .await
        .map_err(|e| anyhow::anyhow!("getting dir circuit: {}", e))?;

    let mut stream = dir_tunnel
        .begin_dir_stream()
        .await
        .map_err(|e| anyhow::anyhow!("opening BEGINDIR stream: {}", e))?;

    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Accept-Encoding: deflate, identity, x-tor-lzma, x-zstd\r\n\
         \r\n",
        path
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing request")?;
    stream.flush().await.context("flushing request")?;

    // Parse HTTP/1.0 response
    let mut reader = BufReader::new(stream);
    let mut header_buf = String::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading header line")?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        header_buf.push_str(&line);
    }

    let status: u16 = header_buf
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    if status != 200 {
        bail!("GET {} returned status {}", path, status);
    }

    let encoding = header_buf
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_string());

    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body).await;

    decompress(encoding.as_deref(), &body).await
}

async fn decompress(encoding: Option<&str>, data: &[u8]) -> Result<Vec<u8>> {
    use async_compression::futures::bufread::*;

    let mut out = Vec::new();
    match encoding {
        None | Some("identity") => {
            out = data.to_vec();
        }
        Some("deflate") => {
            let mut decoder = ZlibDecoder::new(data);
            decoder
                .read_to_end(&mut out)
                .await
                .context("deflate decode")?;
        }
        Some("x-tor-lzma") => {
            let mut decoder = XzDecoder::new(data);
            decoder.read_to_end(&mut out).await.context("xz decode")?;
        }
        Some("x-zstd") => {
            let mut decoder = ZstdDecoder::new(data);
            decoder
                .read_to_end(&mut out)
                .await
                .context("zstd decode")?;
        }
        Some(other) => bail!("unsupported encoding: {}", other),
    }
    Ok(out)
}
