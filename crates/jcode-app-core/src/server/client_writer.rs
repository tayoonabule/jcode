use crate::protocol::{ServerEvent, encode_event};
use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

const CLIENT_WRITER_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CLIENT_WRITER_CHUNK_BYTES: usize = 64 * 1024;
const CLIENT_WRITER_QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
pub(super) struct ClientWriter {
    tx: mpsc::Sender<Vec<u8>>,
    failed_tx: watch::Sender<bool>,
}

impl ClientWriter {
    pub(super) fn spawn(
        mut stream: crate::transport::WriteHalf,
        failed_tx: watch::Sender<bool>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CLIENT_WRITER_QUEUE_CAPACITY);
        let task_failed_tx = failed_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                for (chunk_index, chunk) in bytes.chunks(CLIENT_WRITER_CHUNK_BYTES).enumerate() {
                    match tokio::time::timeout(CLIENT_WRITER_WRITE_TIMEOUT, stream.write_all(chunk))
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            crate::logging::warn(&format!(
                                "client socket write failed ({} bytes total, chunk {} of {}): {}; dropping connection",
                                bytes.len(),
                                chunk_index + 1,
                                bytes.len().div_ceil(CLIENT_WRITER_CHUNK_BYTES),
                                error
                            ));
                            let _ = task_failed_tx.send(true);
                            return;
                        }
                        Err(_) => {
                            crate::logging::warn(&format!(
                                "client socket write made no progress for {}ms ({} bytes total, chunk {} of {}); dropping connection",
                                CLIENT_WRITER_WRITE_TIMEOUT.as_millis(),
                                bytes.len(),
                                chunk_index + 1,
                                bytes.len().div_ceil(CLIENT_WRITER_CHUNK_BYTES)
                            ));
                            let _ = task_failed_tx.send(true);
                            return;
                        }
                    }
                }
            }
        });
        Self { tx, failed_tx }
    }
}

pub(super) async fn write_client_bytes(writer: &ClientWriter, bytes: &[u8]) -> Result<()> {
    match writer.tx.try_send(bytes.to_vec()) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => {
            crate::logging::warn(&format!(
                "client writer queue overflow ({} bytes; capacity {}); dropping connection",
                bytes.len(),
                CLIENT_WRITER_QUEUE_CAPACITY
            ));
            let _ = writer.failed_tx.send(true);
            Err(anyhow::anyhow!(
                "client writer queue overflow; connection is closing"
            ))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            let _ = writer.failed_tx.send(true);
            Err(anyhow::anyhow!(
                "client writer task stopped; connection is closing"
            ))
        }
    }
}

/// Project wire copies only. Persisted state and opted-in API bridges retain
/// PDF bytes, while old native clients can decode the complete History event.
pub(super) fn side_panel_for_client(
    mut snapshot: crate::side_panel::SidePanelSnapshot,
    supports_pdf_panels: bool,
) -> crate::side_panel::SidePanelSnapshot {
    if !supports_pdf_panels {
        for page in &mut snapshot.pages {
            if page.format == crate::side_panel::SidePanelPageFormat::Pdf {
                page.format = crate::side_panel::SidePanelPageFormat::Markdown;
                // This is a generated fallback, not a linked Markdown file.
                // Otherwise old TUIs re-read binary PDF bytes as text on change.
                page.source = crate::side_panel::SidePanelPageSource::Ephemeral;
            }
            page.pdf_data = None;
        }
    }
    snapshot
}

pub(super) async fn write_direct_event(writer: &ClientWriter, event: &ServerEvent) -> Result<()> {
    let json = encode_event(event);
    write_client_bytes(writer, json.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_panel::{
        SidePanelPage, SidePanelPageFormat, SidePanelPageSource, SidePanelSnapshot,
    };
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn slow_reader_receives_large_event_without_write_timeout() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (_, writer_half) = server.into_split();
        let (failed_tx, mut failed_rx) = watch::channel(false);
        let writer = ClientWriter::spawn(writer_half, failed_tx);
        let payload = vec![b'x'; 600 * 1024];
        let marker = b"small-event\n";
        let expected_len = payload.len() + marker.len() * 50;

        let reader = tokio::spawn(async move {
            let mut received = Vec::with_capacity(expected_len);
            let mut buffer = vec![0; 50 * 1024];
            while received.len() < expected_len {
                let take = buffer.len().min(expected_len - received.len());
                client.read_exact(&mut buffer[..take]).await.unwrap();
                received.extend_from_slice(&buffer[..take]);
                if received.len() < expected_len {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            received
        });

        // Queue the history first, then send small events concurrently while
        // its slow reader drains it. They must not wait on a socket mutex.
        write_client_bytes(&writer, &payload).await.unwrap();
        let mut sends = Vec::new();
        for _ in 0..50 {
            let writer = writer.clone();
            sends.push(tokio::spawn(async move {
                write_client_bytes(&writer, b"small-event\n").await
            }));
        }
        for send in sends {
            send.await.unwrap().unwrap();
        }

        let received = tokio::time::timeout(std::time::Duration::from_secs(30), reader)
            .await
            .expect("progressing slow reader must complete")
            .unwrap();
        assert!(received[..payload.len()].iter().all(|byte| *byte == b'x'));
        assert_eq!(
            received[payload.len()..]
                .windows(marker.len())
                .filter(|window| *window == marker)
                .count(),
            50,
            "all concurrently queued small events must arrive"
        );
        assert!(
            !*failed_rx.borrow(),
            "writer must not classify progress as a stall"
        );
    }

    #[tokio::test]
    async fn stalled_client_reader_is_dropped_by_writer_task() {
        let (server, _client_that_does_not_read) = tokio::net::UnixStream::pair().unwrap();
        let (_, writer_half) = server.into_split();
        let (failed_tx, mut failed_rx) = watch::channel(false);
        let writer = ClientWriter::spawn(writer_half, failed_tx);
        write_client_bytes(&writer, &vec![b'x'; 16 * 1024 * 1024])
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while !*failed_rx.borrow() {
                failed_rx.changed().await.unwrap();
            }
        })
        .await
        .expect("a non-reading peer must be disconnected after no write progress");
        assert!(
            write_client_bytes(&writer, b"after close").await.is_err(),
            "the failed writer task must close its bounded queue"
        );
    }

    #[tokio::test]
    async fn full_client_writer_queue_fails_connection_instead_of_blocking() {
        let (server, _client_that_does_not_read) = tokio::net::UnixStream::pair().unwrap();
        let (_, writer_half) = server.into_split();
        let (failed_tx, mut failed_rx) = watch::channel(false);
        let writer = ClientWriter::spawn(writer_half, failed_tx);

        for _ in 0..CLIENT_WRITER_QUEUE_CAPACITY {
            write_client_bytes(&writer, b"queued-event").await.unwrap();
        }
        let overflow = write_client_bytes(&writer, b"overflow-event")
            .await
            .unwrap_err();
        assert!(overflow.to_string().contains("queue overflow"));
        assert!(
            *failed_rx.borrow(),
            "overflow must signal connection teardown"
        );
    }

    #[test]
    fn pdf_panels_live_projection_preserves_fallback_and_opted_in_payload() {
        let snapshot = SidePanelSnapshot {
            focus_revision: 123,
            focused_page_id: Some("report".into()),
            pages: vec![SidePanelPage {
                id: "report".into(),
                title: "Report".into(),
                format: SidePanelPageFormat::Pdf,
                source: SidePanelPageSource::LinkedFile,
                content: "PDF document fallback".into(),
                pdf_data: Some("JVBERi0xLjQKJSVFT0Y=".into()),
                ..Default::default()
            }],
        };
        let opted = side_panel_for_client(snapshot.clone(), true);
        assert_eq!(opted, snapshot);
        let mut projected = side_panel_for_client(snapshot.clone(), false);
        assert_eq!(projected.focus_revision, 123);
        assert_eq!(projected.focused_page_id, snapshot.focused_page_id);
        assert_eq!(projected.pages[0].content, snapshot.pages[0].content);
        assert_eq!(projected.pages[0].format, SidePanelPageFormat::Markdown);
        assert!(projected.pages[0].pdf_data.is_none());
        assert_eq!(projected.pages[0].source, SidePanelPageSource::Ephemeral);
        assert!(!crate::side_panel::refresh_linked_page_content(
            &mut projected,
            None
        ));

        // The shipped native reader has this closed enum. Unknown fields are
        // tolerated, but a new value in this existing field is not.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyFormat {
            Markdown,
        }
        #[derive(serde::Deserialize)]
        struct LegacyPage {
            format: LegacyFormat,
        }
        #[derive(serde::Deserialize)]
        struct LegacySnapshot {
            pages: Vec<LegacyPage>,
        }
        #[derive(serde::Deserialize)]
        struct LegacyEvent {
            snapshot: LegacySnapshot,
        }
        let wire = encode_event(&ServerEvent::SidePanelState {
            snapshot: projected,
        });
        let legacy: LegacyEvent = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            legacy.snapshot.pages[0].format,
            LegacyFormat::Markdown
        ));
        assert!(!wire.contains("pdf_data"));
        let wire = encode_event(&ServerEvent::SidePanelState { snapshot: opted });
        assert!(serde_json::from_str::<LegacyEvent>(&wire).is_err());
        assert!(wire.contains("JVBERi0xLjQKJSVFT0Y="));
    }
}
