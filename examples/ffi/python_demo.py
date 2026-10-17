"""Use onto-intelli from Python through the C ABI (no Python package needed).

    cargo build --release -p onto-intelli-ffi
    python3 examples/ffi/python_demo.py            # in-memory demo
    python3 examples/ffi/python_demo.py onto-intelli.toml
"""

import ctypes
import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
NAMES = {"darwin": "libonto_intelli.dylib", "win32": "onto_intelli.dll"}
LIB = os.environ.get("ONTO_LIB") or str(ROOT / "target" / "release" / NAMES.get(sys.platform, "libonto_intelli.so"))

lib = ctypes.CDLL(LIB)
lib.onto_open.restype = ctypes.c_void_p
lib.onto_open.argtypes = [ctypes.c_char_p]
lib.onto_open_memory.restype = ctypes.c_void_p
lib.onto_call.restype = ctypes.c_void_p
lib.onto_call.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
lib.onto_string_free.argtypes = [ctypes.c_void_p]
lib.onto_close.argtypes = [ctypes.c_void_p]
lib.onto_last_error.restype = ctypes.c_char_p
lib.onto_version.restype = ctypes.c_char_p


class Onto:
    def __init__(self, config=None):
        self.h = lib.onto_open(config.encode()) if config else lib.onto_open_memory()
        if not self.h:
            raise RuntimeError(lib.onto_last_error().decode())

    def call(self, action, payload=None):
        raw = lib.onto_call(self.h, action.encode(), json.dumps(payload or {}).encode())
        try:
            out = json.loads(ctypes.string_at(raw).decode())
        finally:
            lib.onto_string_free(raw)
        if not out["ok"]:
            raise RuntimeError(f'{out["error"]["code"]}: {out["error"]["message"]}')
        return out["result"]

    def ask(self, question, **kw):
        return self.call("intelli.ask", {"question": question, **kw})

    def close(self):
        lib.onto_close(self.h)


if __name__ == "__main__":
    print("onto-intelli", lib.onto_version().decode())
    onto = Onto(sys.argv[1] if len(sys.argv) > 1 else None)
    onto.call("graph.apply", {"mutations": [
        {"op": "upsert", "kind": "team", "key": "ml", "title": "ML Team"},
        {"op": "upsert", "kind": "person", "key": "rio", "title": "Rio Tanaka", "links": [{"rel": "member_of", "to": "team:ml"}]},
        {"op": "upsert", "kind": "project", "key": "ranker", "title": "Project Ranker", "state": "active",
         "links": [{"rel": "owned_by", "to": "team:ml"}]},
    ]})
    for q in ["Who owns Project Ranker?", "Who is in the ML Team?"]:
        r = onto.ask(q)
        print(f"Q: {q}\nA: {r['answer']}  ({r['mode']}, {r['timings']['total_ms']} ms)\n")
    onto.close()
