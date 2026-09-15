//! Shared `text/event-stream` framing for the MCP transports.
//!
//! Both remote transports read SSE bodies: Streamable HTTP takes the response
//! to a single `POST`, while the legacy transport holds one long-lived stream
//! that also carries `endpoint` events. They previously each carried their own
//! decoder, which drifted apart in how they handled multi-line `data:` fields
//! and trailing events. Keep the framing here and let each transport decide
//! what its events mean.

/// One decoded SSE event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RawEvent {
    /// The `event:` field, empty when the server omits it.
    pub(crate) name: String,
    /// The accumulated `data:` payload, with multi-line values newline-joined.
    pub(crate) data: String,
}

/// Incremental SSE line decoder.
///
/// Holds at most one partial line plus the current event's payload, so peak
/// memory is bounded by a single event rather than the whole stream.
#[derive(Default)]
pub(crate) struct SseDecoder {
    pending: String,
    event: String,
    data: String,
}

impl SseDecoder {
    /// Feed a chunk and return every event it completed.
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<RawEvent> {
        self.pending.push_str(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending[..newline].trim_end_matches('\r').to_string();
            self.pending.drain(..=newline);

            if line.is_empty() {
                if let Some(event) = self.take_event() {
                    events.push(event);
                }
            } else if let Some(value) = line.strip_prefix("event:") {
                self.event = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                // A single event may carry several `data:` lines; the wire
                // format joins them with newlines.
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value.trim_start());
            }
        }
        events
    }

    /// Flush an event that arrived without its terminating blank line.
    pub(crate) fn finish(&mut self) -> Option<RawEvent> {
        let pending = std::mem::take(&mut self.pending);
        let line = pending.trim_end_matches(['\r', '\n']);
        if let Some(value) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value.trim_start());
        } else if let Some(value) = line.strip_prefix("event:") {
            self.event = value.trim().to_string();
        }
        self.take_event()
    }

    fn take_event(&mut self) -> Option<RawEvent> {
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        Some(RawEvent {
            name: std::mem::take(&mut self.event),
            data: std::mem::take(&mut self.data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn splits_events_on_blank_lines_across_chunk_boundaries() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("event: message\ndata: {\"a\":").is_empty());
        let events = decoder.push("1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "message");
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn joins_multi_line_data_fields() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push("data: first\ndata: second\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn finish_flushes_an_event_without_a_trailing_blank_line() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("data: {\"done\":true}").is_empty());
        let event = decoder.finish().expect("trailing event should flush");
        assert_eq!(event.data, "{\"done\":true}");
        assert!(decoder.finish().is_none(), "nothing left to flush");
    }

    #[test]
    fn events_without_data_are_dropped_and_do_not_leak_their_name() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("event: ping\n\n").is_empty());
        let events = decoder.push("data: {}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "", "the dropped name must not carry over");
    }
}
