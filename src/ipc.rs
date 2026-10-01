use crate::tmux::snapshot::Snapshot;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// `v` is the client's build ID (`crate::BUILD_ID`); a daemon of another
/// build replies `Restart` and exits.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// A fresh read, then every change.
    Subscribe { v: String, client: String },
    /// The daemon's current snapshot.
    Query { v: String },
    /// Re-read tmux now (after the client's own write) and reply with the
    /// result; subscribers get it too. Its `seq` is the floor below which
    /// the client ignores pushes.
    Refresh { v: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// `seq` grows with every change one daemon publishes; `epoch` names
    /// that daemon instance (a restarted daemon counts from 1 again).
    Snapshot {
        epoch: u64,
        seq: u64,
        data: Snapshot,
    },
    Restart,
    Error {
        msg: String,
    },
}

impl Request {
    pub fn version(&self) -> &str {
        match self {
            Request::Subscribe { v, .. } | Request::Query { v } | Request::Refresh { v } => v,
        }
    }
}

/// `write_msg` for a blocking stream.
pub fn write_msg_sync<W: std::io::Write, T: Serialize>(w: &mut W, msg: &T) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// `read_msg` for a blocking stream.
pub fn read_msg_sync<R: std::io::BufRead, T: DeserializeOwned>(
    r: &mut R,
) -> anyhow::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_msg<R: AsyncBufRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> anyhow::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(line.trim_end())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn round_trip() {
        let (a, b) = tokio::io::duplex(4096);
        let (_, mut w) = tokio::io::split(a);
        let (r, _) = tokio::io::split(b);
        let mut r = tokio::io::BufReader::new(r);
        write_msg(
            &mut w,
            &Request::Subscribe {
                v: "1".into(),
                client: "popup".into(),
            },
        )
        .await
        .unwrap();
        let got: Request = read_msg(&mut r).await.unwrap().unwrap();
        assert!(matches!(got, Request::Subscribe { ref client, .. } if client == "popup"));
        drop(w);
        assert!(read_msg::<_, Request>(&mut r).await.unwrap().is_none());
    }
    #[test]
    fn sync_round_trip() {
        let mut buf = Vec::new();
        write_msg_sync(&mut buf, &Request::Refresh { v: "1".into() }).unwrap();
        let mut r = std::io::BufReader::new(&buf[..]);
        let got: Request = read_msg_sync(&mut r).unwrap().unwrap();
        assert!(matches!(got, Request::Refresh { ref v } if v == "1"));
        assert!(read_msg_sync::<_, Request>(&mut r).unwrap().is_none());
    }
    #[test]
    fn wire_shape() {
        let s = serde_json::to_string(&Request::Query { v: "0.1.0".into() }).unwrap();
        assert_eq!(s, r#"{"op":"query","v":"0.1.0"}"#);
        let s = serde_json::to_string(&Request::Refresh { v: "0.1.0".into() }).unwrap();
        assert_eq!(s, r#"{"op":"refresh","v":"0.1.0"}"#);
        assert_eq!(
            serde_json::to_string(&Reply::Restart).unwrap(),
            r#"{"type":"restart"}"#
        );
    }
}
