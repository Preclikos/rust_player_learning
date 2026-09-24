#!/usr/bin/env python3
"""Mock of the wrapped ClearKey licence endpoint (docs/CLEARKEY_WRAPPED_LICENCE.md).

An independent implementation of the server side (python `cryptography`, not the
player's Rust crates), so a passing run proves the wire protocol, not just that
the client agrees with itself. Serves the shared test-stream keys by default;
pass `--key KID_HEX:KEY_HEX` for others.

    py -3.12 scripts/wrapped_licence_mock.py --port 8090
    # demo:  http://localhost:8080/?licence=http://localhost:8090/licence/wrapped

Requires `pip install cryptography`. CORS is wide open — it is a dev mock.
"""
import argparse
import base64
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

ALG = "ECDH-HKDF-A256GCM"
DEFAULT_INFO = b"rustplayer-clearkey-wrap-v1"
MAX_KIDS = 16

TEST_KEYS = {
    "0fd37dac41c0e987e68d43b801b1210c": "fd8d9f408c2bd702970afcd3b219e791",
    "519af81ab2d284f52aa8257d96b5e4bd": "627ef72b42d98770dec20ecab46cd1f4",
}


def b64u_encode(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def b64u_decode(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def wrap_licence(keys: dict, kids: list, epk: dict, info: bytes) -> dict:
    if not 1 <= len(kids) <= MAX_KIDS:
        raise ValueError(f"1..{MAX_KIDS} kids per request")
    if epk.get("kty") != "EC" or epk.get("crv") != "P-256":
        raise ValueError("epk must be an EC P-256 JWK")
    x, y = b64u_decode(epk["x"]), b64u_decode(epk["y"])
    if len(x) != 32 or len(y) != 32:
        raise ValueError("P-256 coordinates must be 32 bytes")
    # from_encoded_point validates that the point is on the curve.
    client_pub = ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), b"\x04" + x + y)

    server_key = ec.generate_private_key(ec.SECP256R1())
    shared = server_key.exchange(ec.ECDH(), client_pub)  # raw x-coordinate, like WebCrypto deriveBits
    salt = os.urandom(32)
    wrapping_key = HKDF(algorithm=hashes.SHA256(), length=32, salt=salt, info=info).derive(shared)
    gcm = AESGCM(wrapping_key)

    out = []
    for kid in kids:
        if len(kid) != 16:
            continue
        key = keys.get(kid.hex())
        if key is None:
            continue
        iv = os.urandom(12)
        # AESGCM.encrypt returns ciphertext || 16-byte tag; aad = raw KID.
        wrapped = gcm.encrypt(iv, key, kid)
        out.append({"kty": "oct", "kid": b64u_encode(kid), "alg": ALG, "iv": b64u_encode(iv), "k": b64u_encode(wrapped)})

    nums = server_key.public_key().public_numbers()
    return {
        "epk": {"kty": "EC", "crv": "P-256", "x": b64u_encode(nums.x.to_bytes(32, "big")), "y": b64u_encode(nums.y.to_bytes(32, "big"))},
        "salt": b64u_encode(salt),
        "keys": out,
    }


class Handler(BaseHTTPRequestHandler):
    keys: dict = {}
    info: bytes = DEFAULT_INFO
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
            return self._reply(404, {"error": "not found"})
        try:
            n = int(self.headers.get("Content-Length", "0"))
            req = json.loads(self.rfile.read(n))
            kids = [b64u_decode(k) for k in req["kids"]]
            resp = wrap_licence(self.keys, kids, req["epk"], self.info)
            served = [k["kid"] for k in resp["keys"]]
            print(f"[mock] {len(kids)} kid(s) requested, {len(served)} wrapped: {served}", flush=True)
            self._reply(200, resp)
        except (KeyError, ValueError, TypeError, json.JSONDecodeError) as e:
            print(f"[mock] 400: {e}", flush=True)
            self._reply(400, {"error": str(e)})

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


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8090)
    ap.add_argument("--info", default=DEFAULT_INFO.decode(), help="HKDF info string (must match the player)")
    ap.add_argument("--key", action="append", default=[], metavar="KID_HEX:KEY_HEX", help="extra/override key (repeatable)")
    ap.add_argument("--no-test-keys", action="store_true", help="serve only --key entries")
    a = ap.parse_args()

    keys = {} if a.no_test_keys else dict(TEST_KEYS)
    for item in a.key:
        kid, key = item.split(":")
        keys[kid.lower()] = key.lower()
    Handler.keys = {kid: bytes.fromhex(key) for kid, key in keys.items()}
    Handler.info = a.info.encode()

    srv = ThreadingHTTPServer(("127.0.0.1", a.port), Handler)
    print(f"[mock] wrapped licence endpoint on http://localhost:{a.port}{Handler.path_suffix} "
          f"({len(Handler.keys)} key(s), info={a.info!r})", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
