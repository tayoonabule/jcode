use crate::protocol::{ServerEvent, encode_event};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const CLIENT_WRITER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CLIENT_WRITER_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CLIENT_WRITER_CHUNK_BYTES: usize = 64 * 1024;

pub(super) async fn write_client_bytes(
    writer: &Arc<Mutex<crate::transport::WriteHalf>>,
    bytes: &[u8],
) -> Result<()> {
    write_client_bytes_with_timeout(
        writer,
        bytes,
        CLIENT_WRITER_LOCK_TIMEOUT,
        CLIENT_WRITER_WRITE_TIMEOUT,
    )
    .await
}

async fn write_client_bytes_with_timeout(
    writer: &Arc<Mutex<crate::transport::WriteHalf>>,
    bytes: &[u8],
    lock_timeout: std::time::Duration,
    write_timeout: std::time::Duration,
) -> Result<()> {
    let mut guard = tokio::time::timeout(lock_timeout, writer.lock())
        .await
        .map_err(|_| {
            crate::logging::warn(&format!(
                "client writer lock timed out after {}ms ({} bytes); dropping connection",
                lock_timeout.as_millis(),
                bytes.len()
            ));
            anyhow::anyhow!("client writer lock timed out")
        })?;
    for (chunk_index, chunk) in bytes.chunks(CLIENT_WRITER_CHUNK_BYTES).enumerate() {
        tokio::time::timeout(write_timeout, guard.write_all(chunk))
            .await
            .map_err(|_| {
                crate::logging::warn(&format!(
                    "client socket write timed out after {}ms ({} bytes total, chunk {} of {}); dropping connection",
                    write_timeout.as_millis(),
                    bytes.len(),
                    chunk_index + 1,
                    bytes.len().div_ceil(CLIENT_WRITER_CHUNK_BYTES)
                ));
                anyhow::anyhow!("client socket write timed out")
            })??;
    }
    Ok(())
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

pub(super) async fn write_direct_event(
    writer: &Arc<Mutex<crate::transport::WriteHalf>>,
    event: &ServerEvent,
) -> Result<()> {
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
        let (_, writer) = server.into_split();
        let writer = Arc::new(Mutex::new(writer));
        let payload = vec![b'x'; 600 * 1024];
        let expected_len = payload.len();

        let reader = tokio::spawn(async move {
            let mut received = Vec::with_capacity(expected_len);
            let mut buffer = vec![0; 50 * 1024];
            while received.len() < expected_len {
                client.read_exact(&mut buffer).await.unwrap();
                received.extend_from_slice(&buffer);
                if received.len() < expected_len {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            received
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            write_client_bytes_with_timeout(
                &writer,
                &payload,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_secs(5),
            ),
        )
        .await
        .expect("a slow but progressing reader must not hit a whole-event deadline")
        .expect("large event should be fully written");

        let received = reader.await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn stopped_client_reader_times_out_and_releases_writer() {
        let (server, _client_that_does_not_read) = tokio::net::UnixStream::pair().unwrap();
        let (_, writer) = server.into_split();
        let writer = Arc::new(Mutex::new(writer));
        let _held_guard = writer.lock().await;
        let lock_result = write_client_bytes_with_timeout(
            &writer,
            b"lock timeout probe",
            std::time::Duration::from_millis(25),
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(
            lock_result
                .unwrap_err()
                .to_string()
                .contains("lock timed out")
        );
        drop(_held_guard);

        let payload = vec![b'x'; 16 * 1024 * 1024];

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            write_client_bytes_with_timeout(
                &writer,
                &payload,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(100),
            ),
        )
        .await
        .expect("a peer that stopped reading must not block the writer indefinitely");

        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), writer.lock())
                .await
                .is_ok(),
            "timed-out writes must release the writer mutex"
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
