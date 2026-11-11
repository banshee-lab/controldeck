//! Newline-delimited JSON framing with a hard size limit.

use serde::{Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};

/// Largest accepted frame (one line). Bounds memory for a misbehaving peer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Writes `value` as one JSON line and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await
}

/// Reads one JSON line. `Ok(None)` on a clean end of stream before any byte.
/// Lines longer than `max` bytes are rejected without buffering them all.
pub async fn read_frame<R, T>(r: &mut R, max: usize) -> io::Result<Option<T>>
where
    R: AsyncBufRead + AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = Vec::new();
    loop {
        let (done, used) = {
            let buf = r.fill_buf().await?;
            if buf.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame"));
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    line.extend_from_slice(&buf[..i]);
                    (true, i + 1)
                }
                None => {
                    line.extend_from_slice(buf);
                    (false, buf.len())
                }
            }
        };
        r.consume(used);
        if line.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        if done {
            break;
        }
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid frame: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_and_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"a": 1})).await.unwrap();
        write_frame(&mut buf, &json!([2])).await.unwrap();
        let mut r = BufReader::new(&buf[..]);
        assert_eq!(read_frame::<_, Value>(&mut r, 1024).await.unwrap(), Some(json!({"a": 1})));
        assert_eq!(read_frame::<_, Value>(&mut r, 1024).await.unwrap(), Some(json!([2])));
        assert_eq!(read_frame::<_, Value>(&mut r, 1024).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_and_truncated_and_garbage() {
        let big = format!("\"{}\"\n", "x".repeat(100));
        let mut r = BufReader::new(big.as_bytes());
        assert!(read_frame::<_, Value>(&mut r, 50).await.is_err());
        let mut r = BufReader::new(&b"{\"a\":1"[..]);
        assert_eq!(read_frame::<_, Value>(&mut r, 50).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let mut r = BufReader::new(&b"not json\n"[..]);
        assert_eq!(read_frame::<_, Value>(&mut r, 50).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
