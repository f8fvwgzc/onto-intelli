"""Webhook verification against the Standard Webhooks reference vector (the
same vector as the Rust and TypeScript tests): secret
whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw, id msg_p5jXN8AQM9LWM0D4loKWxJek,
timestamp 1614265330, body {"test": 2432232314}. The expected signature
v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE= is the reference libraries'
test value; it is also recomputed here independently with hmac and base64.
Run: python3 -m unittest discover -s tests"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from onto_intelli import WebhookVerificationError, sign_webhook, verify_webhook  # noqa: E402

SECRET = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"
MSG_ID = "msg_p5jXN8AQM9LWM0D4loKWxJek"
TS = 1614265330
BODY = '{"test": 2432232314}'
EXPECTED = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
OTHER = "whsec_dGVzdC1zZWNyZXQtdHdvLXR3by10d28tdHdvLXR3bw=="


def headers(sig: str, msg_id: str = MSG_ID, ts: int = TS) -> dict:
    return {"webhook-id": msg_id, "webhook-timestamp": str(ts), "webhook-signature": sig}


class Vector(unittest.TestCase):
    def test_reference_vector(self) -> None:
        independent = "v1," + base64.b64encode(
            hmac.new(
                base64.b64decode(SECRET[len("whsec_"):]),
                ("%s.%d.%s" % (MSG_ID, TS, BODY)).encode(),
                hashlib.sha256,
            ).digest()
        ).decode()
        self.assertEqual(independent, EXPECTED)
        self.assertEqual(sign_webhook(SECRET, MSG_ID, TS, BODY), EXPECTED)
        self.assertEqual(sign_webhook(SECRET, MSG_ID, TS, BODY.encode()), EXPECTED)

    def test_verify_returns_the_parsed_body(self) -> None:
        self.assertEqual(verify_webhook(SECRET, headers(EXPECTED), BODY, now=TS), {"test": 2432232314})
        upper = {"Webhook-Id": MSG_ID, "Webhook-Timestamp": str(TS), "Webhook-Signature": EXPECTED}
        self.assertEqual(verify_webhook([SECRET], upper, BODY.encode(), now=TS + 299), {"test": 2432232314})

    def test_rotation(self) -> None:
        both = "%s %s" % (sign_webhook(SECRET, MSG_ID, TS, BODY), sign_webhook(OTHER, MSG_ID, TS, BODY))
        self.assertTrue(both.startswith(EXPECTED + " v1,"))
        verify_webhook(SECRET, headers(both), BODY, now=TS)
        verify_webhook(OTHER, headers(both), BODY, now=TS)
        verify_webhook([OTHER, SECRET], headers(EXPECTED), BODY, now=TS)

    def test_refusals(self) -> None:
        cases = [
            (SECRET, headers(EXPECTED), BODY, TS + 301, "tolerance"),
            (SECRET, headers(EXPECTED), BODY.replace("2432", "2433"), TS, "no webhook-signature"),
            (SECRET, headers(EXPECTED, msg_id="msg_other"), BODY, TS, "no webhook-signature"),
            (OTHER, headers(EXPECTED), BODY, TS, "no webhook-signature"),
            (SECRET, headers("v2,abc"), BODY, TS, "no webhook-signature"),
            # Non-ASCII (headers decoded as latin-1): a mismatch, never a TypeError.
            (SECRET, headers("v1,\u00e9\u00e9"), BODY, TS, "no webhook-signature"),
            (SECRET, headers(EXPECTED + " v1,\udcff"), BODY.replace("2", "3"), TS, "no webhook-signature"),
            (SECRET, {"webhook-id": MSG_ID}, BODY, TS, "missing"),
        ]
        for secret, h, body, now, msg in cases:
            with self.assertRaises(WebhookVerificationError) as cm:
                verify_webhook(secret, h, body, now=now)
            self.assertIn(msg, str(cm.exception))
        # A CloudEvent whose id is not the webhook-id is refused even when signed.
        ev = json.dumps({"specversion": "1.0", "id": "obx_other", "type": "onto.webhook.test", "data": {}})
        sig = sign_webhook(SECRET, "obx_1", TS, ev)
        with self.assertRaises(WebhookVerificationError):
            verify_webhook(SECRET, headers(sig, msg_id="obx_1"), ev, now=TS)
        good = json.dumps({"specversion": "1.0", "id": "obx_1", "type": "onto.webhook.test", "data": {}})
        sig = sign_webhook(SECRET, "obx_1", TS, good)
        self.assertEqual(verify_webhook(SECRET, headers(sig, msg_id="obx_1"), good, now=TS)["id"], "obx_1")


if __name__ == "__main__":
    unittest.main()
