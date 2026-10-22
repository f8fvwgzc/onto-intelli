"""The error every failed call raises."""

from __future__ import annotations

from typing import Optional


class OntoError(Exception):
    """An error answer of the server, or a request that got none.

    ``status`` is the HTTP status (0 without a response: network failure,
    timeout, bad options), ``code`` the server's error code
    (``invalid_argument``, ``unauthenticated``, ``permission_denied``,
    ``not_found``, ``conflict``, ``resource_exhausted``, ``unavailable``,
    ``internal``, ...), ``trace_id`` the ``x-onto-trace-id`` of a traced call
    and ``retry_after`` the seconds the server asked to wait.
    """

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        trace_id: Optional[str] = None,
        retry_after: Optional[float] = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.trace_id = trace_id
        self.retry_after = retry_after

    def __str__(self) -> str:
        return self.message

    def __repr__(self) -> str:
        return "OntoError(status={!r}, code={!r}, message={!r}, trace_id={!r})".format(
            self.status, self.code, self.message, self.trace_id
        )
