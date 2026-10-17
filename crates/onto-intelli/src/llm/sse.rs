//! Minimal Server-Sent Events reader over a reqwest byte stream.

use futures::{Stream, StreamExt};

use onto_core::{Error, Result};

#[derive(Debug, Clone)]
pub struct SseEvent {
    /// The `event:` name; current providers put the type in `data` instead.
    #[allow(dead_code)]
    pub event: Option<String>,
    pub data: String,
}

pub fn events(resp: reqwest::Response) -> impl Stream<Item = Result<SseEvent>> + Send {
    let bytes = Box::pin(resp.bytes_stream());
    futures::stream::unfold((bytes, Vec::<u8>::new(), false), |(mut bytes, mut buf, mut done)| async move {
        loop {
            if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
                let raw: Vec<u8> = buf.drain(..pos + 2).collect();
                if let Some(ev) = parse(&raw) {
                    return Some((Ok(ev), (bytes, buf, done)));
                }
                continue;
            }
            if done {
                if buf.iter().all(u8::is_ascii_whitespace) {
                    return None;
                }
                let raw = std::mem::take(&mut buf);
                return parse(&raw).map(|ev| (Ok(ev), (bytes, buf, true)));
            }
            match bytes.next().await {
                Some(Ok(chunk)) => buf.extend(chunk.iter().filter(|b| **b != b'\r')),
                Some(Err(e)) => return Some((Err(Error::Llm(format!("stream: {e}"))), (bytes, buf, true))),
                None => done = true,
            }
        }
    })
}

fn parse(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    if data.is_empty() && event.is_none() {
        return None;
    }
    Some(SseEvent { event, data: data.join("\n") })
}
