use std::path::Path;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{Instrument, debug, debug_span, trace};

use crate::protocol::{ClientMessage, ServerMessage};

/// IPC client that connects to a pup extension socket.
///
/// Uses newline-delimited JSON over a Unix domain socket. The reader/writer
/// halves are split so recv and send are independent.
#[derive(Debug)]
pub struct IpcClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    line_buf: String,
}

impl IpcClient {
    /// Connect to a pup extension socket at the given path.
    pub async fn connect(path: &Path) -> Result<Self> {
        let span = debug_span!("ipc_connect", path = %path.display());
        async {
            let stream = UnixStream::connect(path)
                .await
                .with_context(|| format!("failed to connect to {}", path.display()))?;
            debug!("connected");
            let (read_half, write_half) = stream.into_split();
            Ok(Self {
                reader: BufReader::new(read_half),
                writer: write_half,
                line_buf: String::with_capacity(4096),
            })
        }
        .instrument(span)
        .await
    }

    /// Read the next message from the extension.
    ///
    /// Returns `None` on EOF (extension disconnected).
    pub async fn recv(&mut self) -> Result<Option<ServerMessage>> {
        loop {
            self.line_buf.clear();
            let bytes_read = self
                .reader
                .read_line(&mut self.line_buf)
                .await
                .context("failed to read from IPC socket")?;

            if bytes_read == 0 {
                return Ok(None); // EOF
            }

            let trimmed = self.line_buf.trim();
            if trimmed.is_empty() {
                continue; // Skip empty lines
            }

            trace!(raw = trimmed, "ipc_recv");

            let msg: ServerMessage =
                serde_json::from_str(trimmed).context("failed to parse IPC message")?;

            return Ok(Some(msg));
        }
    }

    /// Send a command to the extension.
    pub async fn send(&mut self, msg: &ClientMessage) -> Result<()> {
        let json = serde_json::to_string(msg).context("failed to serialize IPC command")?;
        debug!(command = %json, "ipc_send");
        self.writer
            .write_all(json.as_bytes())
            .await
            .context("failed to write to IPC socket")?;
        self.writer
            .write_all(b"\n")
            .await
            .context("failed to write newline to IPC socket")?;
        self.writer
            .flush()
            .await
            .context("failed to flush IPC socket")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn test_connect_recv_send() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("test.sock");

        let listener = UnixListener::bind(&sock_path).expect("bind");

        let client_task = tokio::spawn({
            let path = sock_path.clone();
            async move { IpcClient::connect(&path).await.expect("connect") }
        });

        let (server_stream, _) = listener.accept().await.expect("accept");
        let (server_read, mut server_write) = server_stream.into_split();

        // Server sends a hello event
        server_write
            .write_all(b"{\"type\":\"event\",\"event\":\"hello\",\"data\":{\"session_id\":\"abc\",\"cwd\":\"/tmp\"}}\n")
            .await
            .expect("write");

        let mut client = client_task.await.expect("join");

        // Client should receive the hello
        let msg = client.recv().await.expect("recv").expect("some");
        match msg {
            ServerMessage::Event { event, .. } => assert_eq!(event, "hello"),
            ServerMessage::Response { .. } => panic!("expected event"),
        }

        // Client sends a command
        client
            .send(&ClientMessage::GetInfo {
                id: Some("1".into()),
            })
            .await
            .expect("send");

        // Server reads it
        let mut server_reader = BufReader::new(server_read);
        let mut line = String::new();
        server_reader
            .read_line(&mut line)
            .await
            .expect("server read");
        assert!(line.contains("get_info"));

        drop(server_write);
        drop(server_reader);

        // Client sees EOF
        let eof = client.recv().await.expect("recv eof");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn test_serialization_roundtrip() {
        let messages = vec![
            ClientMessage::Send {
                message: "hello".into(),
                mode: Some(crate::protocol::SendMode::Steer),
                id: Some("1".into()),
            },
            ClientMessage::Abort { id: None },
            ClientMessage::GetInfo { id: None },
            ClientMessage::GetHistory {
                turns: Some(5),
                id: Some("2".into()),
            },
        ];

        for msg in &messages {
            let json = serde_json::to_string(msg).expect("serialize");
            let deserialized: ClientMessage = serde_json::from_str(&json).expect("deserialize");
            let json2 = serde_json::to_string(&deserialized).expect("re-serialize");
            assert_eq!(json, json2);
        }
    }
}
