#!/usr/bin/env python3
"""Mock of the wrapped ClearKey licence endpoint, protocol v2
(docs/CLEARKEY_WRAPPED_LICENCE.md).

An independent implementation of the server side (python `cryptography` and
`hmac`, not the player's Rust crates), so a passing run proves the wire
protocol, not just that the client agrees with itself. It serves the shared
test-stream keys by default (`--key KID_HEX:KEY_HEX` for others) and holds a
table of client secrets with active / deprecated / revoked states.

    py -3.12 scripts/wrapped_licence_mock.py --port 8090
    # demo, scenario A:  http://localhost:8080/?licence=http://localhost:8090/licence/wrapped&secret=<B64>&sid=dev
    # demo, scenario B:  the same without &sid=
    py -3.12 scripts/wrapped_licence_mock.py --gen-secret     # a fresh 32-byte secret
    py -3.12 scripts/wrapped_licence_mock.py --self-test      # known-answer vectors

The built-in table holds PUBLIC dev secrets (they are printed at start and are
in this public repository). A real backend must never accept them. Requires
`pip install cryptography`. CORS is wide open: it is a dev mock.
"""
import argparse
import base64
import hashlib
import hmac
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

ALG = "ECDH-HKDF-A256GCM-v2"
KDF_INFO = b"rustplayer-clearkey-wrap-v2"
PROOF_LABEL = b"rustplayer-clearkey-proof-v2"
MIN_SECRET_LEN = 16
MAX_KIDS = 16

TEST_KEYS = {
    "0fd37dac41c0e987e68d43b801b1210c": "fd8d9f408c2bd702970afcd3b219e791",
    "519af81ab2d284f52aa8257d96b5e4bd": "627ef72b42d98770dec20ecab46cd1f4",
}

# PUBLIC dev secrets: 32 bytes derived from a fixed label, so the demo URLs in
# the docs keep working. Never accept these on a real backend.
DEV_SECRETS = [
    ("dev", hashlib.sha256(b"rustplayer public dev secret: active").digest(), "active"),
    ("dev-deprecated", hashlib.sha256(b"rustplayer public dev secret: deprecated").digest(), "deprecated"),
    ("dev-revoked", hashlib.sha256(b"rustplayer public dev secret: revoked").digest(), "revoked"),
]


def b64u_encode(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def b64u_decode(s: str) -> bytes:
    s = s.replace("+", "-").replace("/", "_").rstrip("=")
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def proof_message(kids: list, x: bytes, y: bytes) -> bytes:
    return PROOF_LABEL + b"\x00" + x + y + b"".join(kids)


def compute_proof(secret: bytes, message: bytes) -> bytes:
    return hmac.new(secret, message, hashlib.sha256).digest()


def wrapping_key(shared: bytes, secret: bytes, salt: bytes) -> bytes:
    return HKDF(algorithm=hashes.SHA256(), length=32, salt=salt, info=KDF_INFO).derive(shared + secret)


class HttpError(Exception):
    def __init__(self, status: int, code: str, detail: str):
        super().__init__(detail)
        self.status, self.code = status, code


def authenticate(table: list, sid, proof: bytes, message: bytes):
    """Return the matching row or raise 401/403.

    Scenario A (sid given): one lookup, one HMAC. Scenario B (no sid): one HMAC
    per row over the WHOLE table, no early exit, compare_digest for each, so
    the time does not depend on which row matched."""
    if sid is not None:
        rows = [r for r in table if r["id"] == sid]
        found = rows[0] if rows and hmac.compare_digest(compute_proof(rows[0]["secret"], message), proof) else None
    else:
        found = None
        for r in table:
            if hmac.compare_digest(compute_proof(r["secret"], message), proof) and found is None:
                found = r
    if found is None:
        raise HttpError(401, "unknown_client", "no client secret matches this proof" + (f" for sid {sid!r}" if sid else ""))
    if found["status"] == "revoked":
        raise HttpError(403, "client_revoked", f"client secret {found['id']!r} is revoked")
    return found


def wrap_licence(keys: dict, table: list, req: dict):
    if req.get("v") != 2:
        raise HttpError(400, "bad_request", "only protocol v2 is served")
    kids = [b64u_decode(k) for k in req["kids"]]
    if not 1 <= len(kids) <= MAX_KIDS:
        raise HttpError(400, "bad_request", f"1..{MAX_KIDS} kids per request")
    if any(len(k) != 16 for k in kids):
        raise HttpError(400, "bad_request", "kids must be 16 bytes")
    epk = req["epk"]
    if epk.get("kty") != "EC" or epk.get("crv") != "P-256":
        raise HttpError(400, "bad_request", "epk must be an EC P-256 JWK")
    x, y = b64u_decode(epk["x"]), b64u_decode(epk["y"])
    if len(x) != 32 or len(y) != 32:
        raise HttpError(400, "bad_request", "P-256 coordinates must be 32 bytes")
    proof = b64u_decode(req.get("proof") or "")
    if len(proof) != 32:
        raise HttpError(401, "unknown_client", "missing or malformed proof")

    # Authenticate BEFORE any ECDH or key lookup: a revoked or unknown client
    # costs the server only the HMACs.
    row = authenticate(table, req.get("sid") or None, proof, proof_message(kids, x, y))

    # from_encoded_point validates that the point is on the curve.
    client_pub = ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), b"\x04" + x + y)
    server_key = ec.generate_private_key(ec.SECP256R1())
    shared = server_key.exchange(ec.ECDH(), client_pub)  # raw x-coordinate, like WebCrypto deriveBits
    salt = os.urandom(32)
    gcm = AESGCM(wrapping_key(shared, row["secret"], salt))

    out = []
    for kid in kids:
        key = keys.get(kid.hex())
        if key is None:
            continue
        iv = os.urandom(12)
        # AESGCM.encrypt returns ciphertext || 16-byte tag; aad = raw KID.
        out.append({"kty": "oct", "kid": b64u_encode(kid), "alg": ALG, "iv": b64u_encode(iv),
                    "k": b64u_encode(gcm.encrypt(iv, key, kid))})

    nums = server_key.public_key().public_numbers()
    resp = {
        "epk": {"kty": "EC", "crv": "P-256", "x": b64u_encode(nums.x.to_bytes(32, "big")),
                "y": b64u_encode(nums.y.to_bytes(32, "big"))},
        "salt": b64u_encode(salt),
        "keys": out,
    }
    if row["status"] == "deprecated":
        resp["notice"] = "deprecated"
    return row, resp


class Handler(BaseHTTPRequestHandler):
    keys: dict = {}
    table: list = []
    path_suffix = "/licence/wrapped"

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, Authorization")

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.end_headers()

    def do_POST(self):
        if not self.path.endswith(self.path_suffix):
            return self._reply(404, {"error": "not_found"})
        try:
            n = int(self.headers.get("Content-Length", "0"))
            req = json.loads(self.rfile.read(n))
            row, resp = wrap_licence(self.keys, self.table, req)
            scenario = "A" if req.get("sid") else "B"
            served = [k["kid"] for k in resp["keys"]]
            print(f"[mock] scenario {scenario}, client {row['id']!r} ({row['status']}): "
                  f"{len(req['kids'])} kid(s) requested, {len(served)} wrapped: {served}", flush=True)
            self._reply(200, resp)
        except HttpError as e:
            print(f"[mock] {e.status} {e.code}: {e}", flush=True)
            self._reply(e.status, {"error": e.code, "detail": str(e)})
        except (KeyError, ValueError, TypeError, json.JSONDecodeError) as e:
            print(f"[mock] 400: {e}", flush=True)
            self._reply(400, {"error": "bad_request", "detail": str(e)})

    def _reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self._cors()
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):  # quieter than the default access log
        pass


def self_test() -> int:
    """The known-answer vectors from the doc (the Rust tests check the same)."""
    secret = bytes(range(32))
    msg = proof_message([b"\x33" * 16], b"\x11" * 32, b"\x22" * 32)
    proof = compute_proof(secret, msg).hex()
    wk = wrapping_key(b"\x44" * 32, secret, b"\x55" * 32).hex()
    ok = (proof == "e387a32563d9f7c997fd213c9852ba906da5f7e5ec215fe95ec49c461716f6a2"
          and wk == "b5303419aaaa933b7a1816889f946502febc8c0b139177da79defe606954386b")
    print(f"proof        {proof}\nwrapping key {wk}\n{'OK' if ok else 'MISMATCH'}")
    return 0 if ok else 1


def load_table(a) -> list:
    table = [] if a.no_dev_secrets else [{"id": i, "secret": s, "status": st} for i, s, st in DEV_SECRETS]
    if a.secrets:
        with open(a.secrets, encoding="utf-8") as f:
            for r in json.load(f):
                table.append({"id": r["id"], "secret": b64u_decode(r["secret"]), "status": r.get("status", "active")})
    for item in a.secret:
        parts = item.split(":")
        table.append({"id": parts[0], "secret": b64u_decode(parts[1]), "status": parts[2] if len(parts) > 2 else "active"})
    for r in table:
        if len(r["secret"]) < MIN_SECRET_LEN:
            sys.exit(f"secret {r['id']!r} is shorter than {MIN_SECRET_LEN} bytes")
        if r["status"] not in ("active", "deprecated", "revoked"):
            sys.exit(f"secret {r['id']!r}: status must be active, deprecated or revoked")
    return table


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8090)
    ap.add_argument("--key", action="append", default=[], metavar="KID_HEX:KEY_HEX", help="extra/override key (repeatable)")
    ap.add_argument("--no-test-keys", action="store_true", help="serve only --key entries")
    ap.add_argument("--secret", action="append", default=[], metavar="ID:B64URL[:STATUS]",
                    help="client secret row (repeatable); STATUS active|deprecated|revoked")
    ap.add_argument("--secrets", metavar="FILE", help='JSON list of {"id", "secret", "status"}')
    ap.add_argument("--no-dev-secrets", action="store_true", help="drop the built-in public dev secrets")
    ap.add_argument("--gen-secret", action="store_true", help="print a fresh random 32-byte secret and exit")
    ap.add_argument("--self-test", action="store_true", help="check the known-answer vectors and exit")
    a = ap.parse_args()

    if a.gen_secret:
        print(b64u_encode(os.urandom(32)))
        return 0
    if a.self_test:
        return self_test()

    keys = {} if a.no_test_keys else dict(TEST_KEYS)
    for item in a.key:
        kid, key = item.split(":")
        keys[kid.lower()] = key.lower()
    Handler.keys = {kid: bytes.fromhex(key) for kid, key in keys.items()}
    Handler.table = load_table(a)

    srv = ThreadingHTTPServer(("127.0.0.1", a.port), Handler)
    print(f"[mock] wrapped licence v2 endpoint on http://localhost:{a.port}{Handler.path_suffix} "
          f"({len(Handler.keys)} key(s), {len(Handler.table)} client secret(s))", flush=True)
    for r in Handler.table:
        public = " (PUBLIC dev secret)" if any(r["id"] == d[0] for d in DEV_SECRETS) else ""
        shown = b64u_encode(r["secret"]) if public else "<hidden>"
        print(f"[mock]   {r['id']:<16} {r['status']:<10} {shown}{public}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
