"""onto-intelli for Python: ask questions of your ontology, run actions, read
traces. Standard library only (``urllib.request``, ``json``); Python 3.9+.

    from onto_intelli import Onto

    onto = Onto()  # ONTO_URL, ONTO_API_KEY, ONTO_TENANT
    res = onto.ask("Who owns the payments service?", session_id="chat-42")
    print(res["answer"], res["trace_id"])
"""

from __future__ import annotations

from .client import (
    VERSION,
    AskEvent,
    AskRequest,
    AskResponse,
    AskTrace,
    Entity,
    MintedSession,
    Onto,
    Resolved,
    ScoreRow,
    SessionPage,
    SessionRow,
    SpanRow,
    TokenUsage,
    TraceDetail,
    TraceMetrics,
    TracePage,
    TraceRow,
)
from .errors import OntoError
from .sse import SseMessage, parse_sse

__version__ = VERSION

__all__ = [
    "VERSION",
    "AskEvent",
    "AskRequest",
    "AskResponse",
    "AskTrace",
    "Entity",
    "MintedSession",
    "Onto",
    "OntoError",
    "Resolved",
    "ScoreRow",
    "SessionPage",
    "SessionRow",
    "SpanRow",
    "SseMessage",
    "TokenUsage",
    "TraceDetail",
    "TraceMetrics",
    "TracePage",
    "TraceRow",
    "parse_sse",
]
