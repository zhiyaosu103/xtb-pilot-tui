//! NDJSON 帧编解码：一行一个 JSON 请求/响应。
//!
//! 刻意不引 jsonrpsee/tarpc（规划文档 §2.2）：协议只有「一行一请求」，
//! 手写帧约百行。

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// 写出一个 NDJSON 帧（JSON 一行 + 换行，并 flush）。
pub async fn write_frame<W, T>(writer: &mut W, payload: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let line = serde_json::to_string(payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// 读入一个 NDJSON 帧；EOF 时返回 `None`（空行视为 keep-alive，循环跳过）。
pub async fn read_frame<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str(trimmed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        return Ok(Some(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tokio::io::BufReader;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Ping {
        method: String,
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Ping {
                method: "ping".into(),
            },
        )
        .await
        .unwrap();
        assert!(buf.ends_with(b"\n"));
        let mut reader = BufReader::new(&buf[..]);
        let got: Ping = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(
            got,
            Ping {
                method: "ping".into()
            }
        );
    }

    #[tokio::test]
    async fn frame_eof_returns_none() {
        let mut reader = BufReader::new(&b""[..]);
        let got: Option<Ping> = read_frame(&mut reader).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn frame_tolerates_blank_lines() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\n");
        write_frame(
            &mut buf,
            &Ping {
                method: "ping".into(),
            },
        )
        .await
        .unwrap();
        let mut reader = BufReader::new(&buf[..]);
        let got: Ping = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(got.method, "ping");
    }
}
