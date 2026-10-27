"""The sign-in bed of `script/dev-sign-in`: an Omnigraph server's OIDC trust, a TLS front and a
stand-in authorization server, for trying Studio's sign-in (spec 016) on this machine.

  front  https://127.0.0.1:FRONT   TLS from the bed's own CA. Proxies to the Omnigraph server,
                                    and serves the authorization server's metadata, token and
                                    revocation endpoints under /oauth.
  login  http://127.0.0.1:LOGIN    The authorization endpoint, plain HTTP on loopback so a
                                    browser opens it without a certificate warning. Approves at
                                    once as the bed's current subject (see /oauth/_bed).

The issuer is https://127.0.0.1:FRONT/oauth and the resource https://127.0.0.1:FRONT/mcp.
The trust file the server reads is rewritten every 30 s with a new revision, as a publisher
would, because a snapshot lives at most 300 s.

Control (front): GET /oauth/_bed answers the state and the log of requests; POST /oauth/_bed
with JSON {"subject", "access_ttl", "refuse_refresh"} changes how it answers.
"""

import base64
import hashlib
import http.client
import http.server
import json
import os
import secrets
import ssl
import subprocess
import sys
import threading
import time
import urllib.parse

FRONT = int(os.environ.get("BED_FRONT", "8791"))
LOGIN = int(os.environ.get("BED_LOGIN", "8792"))
SERVER = int(os.environ.get("BED_SERVER", "8790"))
DATA = os.path.realpath(os.environ["BED_DATA"])
KEYS = os.environ["BED_KEYS"]
TRUST = os.environ["BED_TRUST"]

ORIGIN = f"https://127.0.0.1:{FRONT}"
ISSUER = f"{ORIGIN}/oauth"
RESOURCE = f"{ORIGIN}/mcp"
ORG = "org_bed"
KID = "bed-1"
# The subjects the server admits, and the principal each is. `user_mallory` signs in but is not
# admitted.
ADMITTED = {"user_alice": "alice"}

lock = threading.Lock()
state = {"subject": "user_alice", "access_ttl": 300, "refuse_refresh": False}
codes = {}
refresh_tokens = {}
log = []


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def public_key():
    out = subprocess.run(
        ["openssl", "rsa", "-in", os.path.join(KEYS, "signing.pem"), "-noout", "-modulus"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    modulus = bytes.fromhex(out.split("=", 1)[1])
    return {"kid": KID, "kty": "RSA", "alg": "RS256", "use": "sig", "n": b64url(modulus), "e": "AQAB"}


def sign(claims):
    header = b64url(json.dumps({"alg": "RS256", "typ": "at+jwt", "kid": KID}).encode())
    payload = b64url(json.dumps(claims).encode())
    signing_input = f"{header}.{payload}".encode()
    signature = subprocess.run(
        ["openssl", "dgst", "-sha256", "-sign", os.path.join(KEYS, "signing.pem")],
        input=signing_input,
        check=True,
        capture_output=True,
    ).stdout
    return f"{header}.{payload}.{b64url(signature)}"


def publish(revision, key):
    now = int(time.time())
    document = {
        "version": 1,
        "revision": revision,
        "generated_at": now,
        "expires_at": now + 300,
        "issuer": ISSUER,
        "audience": RESOURCE,
        "organization_id": ORG,
        "account_id": "acct_bed",
        "cluster_id": "bed",
        "cluster_incarnation": "inc1",
        "canonical_root": f"file://{DATA}",
        "keys": [key],
        "principals": [{"subject": s, "principal_id": p} for s, p in ADMITTED.items()],
    }
    tmp = TRUST + ".tmp"
    with open(tmp, "w") as f:
        json.dump(document, f)
    os.replace(tmp, TRUST)


def publisher(key):
    revision = int(time.time())
    while True:
        time.sleep(30)
        revision += 1
        publish(revision, key)


def note(entry):
    with lock:
        log.append({"at": round(time.time(), 3), **entry})
        del log[:-200]


def metadata():
    return {
        "issuer": ISSUER,
        "authorization_endpoint": f"http://127.0.0.1:{LOGIN}/authorize",
        "token_endpoint": f"{ISSUER}/token",
        "revocation_endpoint": f"{ISSUER}/revoke",
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": True,
    }


def issue(subject, client_id, resource):
    now = int(time.time())
    with lock:
        ttl = state["access_ttl"]
    access = sign(
        {
            "iss": ISSUER,
            "sub": subject,
            "aud": resource,
            "iat": now,
            "exp": now + ttl,
            "org_id": ORG,
            "client_id": client_id,
            "jti": secrets.token_hex(8),
        }
    )
    refresh = secrets.token_urlsafe(32)
    with lock:
        refresh_tokens[refresh] = {"subject": subject, "client_id": client_id, "resource": resource}
    return {
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ttl,
        "refresh_token": refresh,
        "scope": "offline_access",
    }


class Front(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def answer(self, status, body, content_type="application/json", headers=()):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.send_header("cache-control", "no-store")
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("content-length") or 0)
        return self.rfile.read(length) if length else b""

    def oauth(self, method):
        path = urllib.parse.urlsplit(self.path).path
        if path == "/.well-known/oauth-authorization-server/oauth" and method == "GET":
            note({"route": "as-metadata"})
            return self.answer(200, metadata())
        if path == "/oauth/_bed":
            if method == "POST":
                with lock:
                    state.update(json.loads(self.body() or b"{}"))
            with lock:
                return self.answer(200, {"state": state, "log": log})
        if path == "/oauth/token" and method == "POST":
            form = dict(urllib.parse.parse_qsl(self.body().decode()))
            grant = form.get("grant_type")
            note({"route": "token", "grant": grant, "resource": form.get("resource"), "client_id": form.get("client_id")})
            if grant == "authorization_code":
                with lock:
                    entry = codes.pop(form.get("code", ""), None)
                verifier = form.get("code_verifier", "")
                if (
                    entry is None
                    or entry["redirect_uri"] != form.get("redirect_uri")
                    or entry["client_id"] != form.get("client_id")
                    or entry["resource"] != form.get("resource")
                    or b64url(hashlib.sha256(verifier.encode()).digest()) != entry["challenge"]
                ):
                    return self.answer(400, {"error": "invalid_grant", "error_description": "the code, verifier, redirect or resource does not match"})
                return self.answer(200, issue(entry["subject"], entry["client_id"], entry["resource"]))
            if grant == "refresh_token":
                with lock:
                    refuse = state["refuse_refresh"]
                    entry = refresh_tokens.pop(form.get("refresh_token", ""), None)
                if refuse or entry is None or entry["client_id"] != form.get("client_id"):
                    return self.answer(400, {"error": "invalid_grant", "error_description": "the refresh token is not valid"})
                if entry["resource"] != form.get("resource"):
                    return self.answer(400, {"error": "invalid_target"})
                return self.answer(200, issue(entry["subject"], entry["client_id"], entry["resource"]))
            return self.answer(400, {"error": "unsupported_grant_type"})
        if path == "/oauth/revoke" and method == "POST":
            form = dict(urllib.parse.parse_qsl(self.body().decode()))
            note({"route": "revoke"})
            with lock:
                refresh_tokens.pop(form.get("token", ""), None)
            return self.answer(200, b"", "text/plain")
        return self.answer(404, {"error": "not_found"})

    def proxy(self, method):
        body = self.body()
        upstream = http.client.HTTPConnection("127.0.0.1", SERVER, timeout=60)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("connection", "keep-alive", "transfer-encoding", "content-length")}
        if body:
            headers["content-length"] = str(len(body))
        upstream.request(method, self.path, body=body or None, headers=headers)
        response = upstream.getresponse()
        data = response.read()
        note({"route": "server", "method": method, "path": urllib.parse.urlsplit(self.path).path, "status": response.status, "bearer": "authorization" in (k.lower() for k in self.headers.keys())})
        self.send_response(response.status)
        for name, value in response.getheaders():
            if name.lower() not in ("connection", "keep-alive", "transfer-encoding", "content-length"):
                self.send_header(name, value)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        upstream.close()

    def route(self, method):
        path = urllib.parse.urlsplit(self.path).path
        if path.startswith("/oauth/") or path.startswith("/.well-known/oauth-authorization-server"):
            return self.oauth(method)
        return self.proxy(method)

    def do_GET(self):
        self.route("GET")

    def do_HEAD(self):
        self.route("HEAD")

    def do_POST(self):
        self.route("POST")

    def do_PUT(self):
        self.route("PUT")

    def do_DELETE(self):
        self.route("DELETE")


class Login(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        note({"route": "authorize", "query": {k: v for k, v in query.items() if k != "code_challenge"}})
        redirect = query.get("redirect_uri", "")
        target = urllib.parse.urlsplit(redirect)
        problems = []
        if url.path != "/authorize":
            problems.append("no such page")
        if query.get("response_type") != "code":
            problems.append("response_type is not code")
        if query.get("code_challenge_method") != "S256" or not query.get("code_challenge"):
            problems.append("no S256 code challenge")
        if not query.get("state"):
            problems.append("no state")
        if query.get("resource") != RESOURCE:
            problems.append(f"resource is not {RESOURCE}")
        if target.scheme != "http" or target.hostname != "127.0.0.1" or target.path != "/callback":
            problems.append("redirect is not http://127.0.0.1:<port>/callback")
        if problems:
            data = ("Refused: " + "; ".join(problems)).encode()
            self.send_response(400)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        code = secrets.token_urlsafe(24)
        with lock:
            subject = state["subject"]
            codes[code] = {
                "subject": subject,
                "client_id": query.get("client_id"),
                "redirect_uri": redirect,
                "resource": query.get("resource"),
                "challenge": query.get("code_challenge"),
            }
        location = redirect + "?" + urllib.parse.urlencode({"code": code, "state": query["state"], "iss": ISSUER})
        self.send_response(302)
        self.send_header("location", location)
        self.send_header("content-length", "0")
        self.end_headers()


def serve(server):
    threading.Thread(target=server.serve_forever, daemon=True).start()


def main():
    key = public_key()
    publish(int(time.time()), key)
    threading.Thread(target=publisher, args=(key,), daemon=True).start()
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(os.path.join(KEYS, "front.pem"), os.path.join(KEYS, "front-key.pem"))
    front = http.server.ThreadingHTTPServer(("127.0.0.1", FRONT), Front)
    front.socket = context.wrap_socket(front.socket, server_side=True)
    serve(front)
    serve(http.server.ThreadingHTTPServer(("127.0.0.1", LOGIN), Login))
    print(f"bed: front {ORIGIN}, login http://127.0.0.1:{LOGIN}, issuer {ISSUER}, resource {RESOURCE}", flush=True)
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--publish-once":
        publish(int(time.time()), public_key())
    else:
        main()
