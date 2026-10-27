"""A Server-Sent Events parser (the WHATWG EventSource framing) over byte
chunks: ``event:``, ``data:`` (joined with newlines), ``id:``, ``retry:``,
comments, and events ended by a blank line. Chunks may split anywhere,
including inside a multi-byte character or between ``\\r`` and ``\\n``."""

from __future__ import annotations

import codecs
import re
from typing import Dict, Iterable, Iterator, List, Optional, TypedDict


_EOL = re.compile(r"\r\n|\r|\n")


class _SseFields(TypedDict):
    event: str
    data: str
    id: Optional[str]


class SseMessage(_SseFields, total=False):
    """One message; ``event`` is ``message`` when the stream names none.
    ``retry`` (milliseconds) is present when the stream set a reconnection
    time (``retry:``) before this message."""

    retry: int


def parse_sse(chunks: Iterable[bytes]) -> Iterator[SseMessage]:
    """Yield the messages of an SSE stream given as byte chunks."""
    decoder = codecs.getincrementaldecoder("utf-8")()
    buf = ""
    state = {"event": "", "id": None}  # type: Dict[str, Optional[str]]
    retry = [None]  # type: List[Optional[int]]
    data = []  # type: List[str]
    saw = [False]

    def line(text: str) -> Optional[SseMessage]:
        if text == "":
            out = None  # type: Optional[SseMessage]
            if saw[0]:
                out = {"event": state["event"] or "message", "data": "\n".join(data), "id": state["id"]}  # type: SseMessage
                if retry[0] is not None:
                    out["retry"] = retry[0]
            state["event"] = ""
            del data[:]
            saw[0] = False
            return out
        if text.startswith(":"):
            return None
        field, sep, value = text.partition(":")
        if sep and value.startswith(" "):
            value = value[1:]
        if field == "event":
            state["event"] = value
        elif field == "data":
            data.append(value)
            saw[0] = True
        elif field == "id":
            state["id"] = value
        elif field == "retry" and value.isascii() and value.isdigit():
            retry[0] = int(value)
        return None

    def drain(final: bool) -> Iterator[SseMessage]:
        nonlocal buf
        while True:
            m = _EOL.search(buf)
            if m is None or (m.group() == "\r" and m.start() == len(buf) - 1 and not final):
                return
            text, buf = buf[: m.start()], buf[m.end() :]
            msg = line(text)
            if msg is not None:
                yield msg

    for chunk in chunks:
        buf += decoder.decode(chunk)
        for msg in drain(False):
            yield msg
    buf += decoder.decode(b"", final=True)
    for msg in drain(True):
        yield msg
    # A stream that ends without the final blank line still delivers its event.
    if buf:
        msg = line(buf)
        buf = ""
        if msg is not None:
            yield msg
    msg = line("")
    if msg is not None:
        yield msg
