"""Verifying onto-intelli webhook deliveries (docs/design/EVENTS.md B.3):
CloudEvents 1.0 structured JSON bodies signed with Standard Webhooks.

The signed content is ``<webhook-id>.<webhook-timestamp>.<body>``, signed with
HMAC-SHA256; ``webhook-signature`` holds one or more ``v1,<base64>`` values
separated by spaces (two during a secret rotation). A ``whsec_`` secret is
base64 after its prefix; any other secret is used as its UTF-8 bytes.

Each change is enqueued once per endpoint and delivered at least once: dedupe
on the event id (``webhook-id``, the envelope's ``id``).

    from onto_intelli.webhooks import verify_webhook

    event = verify_webhook(os.environ["HOOK_SECRET"], request.headers, request.body)
    if seen(event["id"]):
        return 200
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import hmac
import json
import time
from typing import Any, Dict, Mapping, Optional, Sequence, Union

#: The Standard Webhooks default: timestamps further than this from now are refused.
TOLERANCE_SECS = 300

Body = Union[bytes, str]


class WebhookVerificationError(Exception):
    """The delivery is not authentic (or too old to accept)."""


def signing_key(secret: str) -> bytes:
    """The HMAC key of a secret: base64 after ``whsec_``, else the UTF-8 bytes."""
    s = secret.strip()
    if s.startswith("whsec_"):
        try:
            return base64.b64decode(s[len("whsec_"):], validate=True)
        except (binascii.Error, ValueError):
            raise WebhookVerificationError("a whsec_ secret is base64 after its prefix") from None
    if not s:
        raise WebhookVerificationError("an empty signing secret")
    return s.encode("utf-8")


def _bytes(body: Body) -> bytes:
    return body.encode("utf-8") if isinstance(body, str) else bytes(body)


def sign_webhook(secret: str, msg_id: str, timestamp: int, body: Body) -> str:
    """One Standard Webhooks signature: ``v1,`` and base64 of
    HMAC-SHA256(key, ``<id>.<timestamp>.<body>``)."""
    content = ("%s.%d." % (msg_id, timestamp)).encode("utf-8") + _bytes(body)
    mac = hmac.new(signing_key(secret), content, hashlib.sha256).digest()
    return "v1," + base64.b64encode(mac).decode("ascii")


def _header(headers: Mapping[str, Any], name: str) -> str:
    for k, v in headers.items():
        if k.lower() == name:
            if isinstance(v, (list, tuple)):
                v = v[0] if v else ""
            return str(v).strip()
    return ""


def verify_webhook(
    secrets: Union[str, Sequence[str]],
    headers: Mapping[str, Any],
    body: Body,
    tolerance_secs: int = TOLERANCE_SECS,
    now: Optional[int] = None,
) -> Dict[str, Any]:
    """Verify a delivery and return its parsed body (the CloudEvent).

    Pass the raw body exactly as received and every secret that may sign it
    (the current and, during a rotation, the previous). Raises
    :class:`WebhookVerificationError` when the timestamp is outside the
    tolerance, no signature matches, or the envelope's id is not the
    ``webhook-id``. Dedupe on the returned ``id``: deliveries are at least once.
    """
    msg_id = _header(headers, "webhook-id")
    ts_raw = _header(headers, "webhook-timestamp")
    sig = _header(headers, "webhook-signature")
    if not msg_id or not ts_raw or not sig:
        raise WebhookVerificationError("missing webhook-id, webhook-timestamp or webhook-signature")
    try:
        ts = int(ts_raw)
    except ValueError:
        raise WebhookVerificationError("webhook-timestamp is not a number") from None
    current = int(time.time()) if now is None else now
    if abs(current - ts) > tolerance_secs:
        raise WebhookVerificationError("webhook-timestamp is outside the tolerance")
    given = [s[3:] for s in sig.split(" ") if s.startswith("v1,")]
    candidates = [secrets] if isinstance(secrets, str) else list(secrets)
    ok = False
    for secret in candidates:
        expected = sign_webhook(secret, msg_id, ts, body)[3:]
        for g in given:
            ok = hmac.compare_digest(expected, g) or ok
    if not ok:
        raise WebhookVerificationError("no webhook-signature matches")
    try:
        event = json.loads(_bytes(body).decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        raise WebhookVerificationError("the body is not JSON") from None
    if isinstance(event, dict) and "id" in event and event["id"] != msg_id:
        raise WebhookVerificationError("the event id is not the webhook-id")
    return event
