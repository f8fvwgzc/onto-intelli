// A Server-Sent Events parser over a byte stream (the WHATWG
// EventSource framing): `event:`, `data:` (joined with newlines), `id:`,
// `retry:`, comments, and events ended by a blank line. Chunks may split
// anywhere, including inside a multi-byte character or between `\r` and `\n`.

export interface SseMessage {
  /** `message` when the event names none. */
  event: string;
  data: string;
  id?: string;
  /**
   * The reconnection time the stream asked for (`retry:`, milliseconds), as
   * last set before this message; `undefined` when it set none.
   */
  retry?: number;
}

/** The messages of an SSE stream, in order. */
export async function* parseSse(body: ReadableStream<Uint8Array>): AsyncGenerator<SseMessage> {
  const decoder = new TextDecoder();
  const reader = body.getReader();
  let buf = "";
  let event = "";
  let data: string[] = [];
  let id: string | undefined;
  let retry: number | undefined;
  let sawData = false;
  const line = (l: string): SseMessage | undefined => {
    if (l === "") {
      const out: SseMessage | undefined = sawData ? { event: event || "message", data: data.join("\n"), id } : undefined;
      if (out && retry !== undefined) out.retry = retry;
      event = "";
      data = [];
      sawData = false;
      return out;
    }
    if (l.startsWith(":")) return undefined;
    const colon = l.indexOf(":");
    const field = colon < 0 ? l : l.slice(0, colon);
    let value = colon < 0 ? "" : l.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") event = value;
    else if (field === "data") {
      data.push(value);
      sawData = true;
    } else if (field === "id") id = value;
    // Only ASCII digits set it (WHATWG); anything else is ignored.
    else if (field === "retry" && /^[0-9]+$/.test(value)) retry = Number(value);
    return undefined;
  };
  try {
    for (;;) {
      const { value, done } = await reader.read();
      buf += done ? decoder.decode() : decoder.decode(value, { stream: true });
      // A trailing "\r" may be the first half of "\r\n": keep it for the next chunk.
      for (;;) {
        const m = /\r\n|\r|\n/.exec(buf);
        if (!m || (m[0] === "\r" && m.index === buf.length - 1 && !done)) break;
        const l = buf.slice(0, m.index);
        buf = buf.slice(m.index + m[0].length);
        const msg = line(l);
        if (msg) yield msg;
      }
      if (done) {
        // A stream that ends without the final blank line still delivers its event.
        if (buf !== "") {
          const msg = line(buf);
          buf = "";
          if (msg) yield msg;
        }
        const msg = line("");
        if (msg) yield msg;
        return;
      }
    }
  } finally {
    // Stopping early (a `break` in `for await`) closes the connection.
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}
