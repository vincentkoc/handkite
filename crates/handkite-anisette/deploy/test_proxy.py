#!/usr/bin/env python3
"""Exercise the shipped Caddyfile with real Caddy and synthetic loopback traffic."""

import base64
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import queue
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import unittest


CADDYFILE = Path(__file__).with_name("Caddyfile")
ALLOWED = (
    ("GET", "/v3/client_info"),
    ("POST", "/v3/get_headers"),
    ("GET", "/v3/provisioning_session"),
)
WEBSOCKET_KEY = base64.b64encode(b"synthetic-client").decode()
WEBSOCKET_HEADERS = {
    "Connection": "Upgrade",
    "Upgrade": "websocket",
    "Sec-WebSocket-Version": "13",
    "Sec-WebSocket-Key": WEBSOCKET_KEY,
}
WEBSOCKET_ACCEPT = base64.b64encode(
    hashlib.sha1(
        (WEBSOCKET_KEY + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
    ).digest()
).decode()


def run(*args, **kwargs):
    return subprocess.run(
        args, check=True, capture_output=True, text=True, timeout=15, **kwargs
    )


def certificates(directory):
    """Create disposable identities without using system trust or private accounts."""
    extensions = directory / "extensions.cnf"
    leaf_extensions = (
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
        "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n"
    )
    extensions.write_text(
        "[server]\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n"
        + leaf_extensions + "[client]\nextendedKeyUsage=clientAuth\n" + leaf_extensions
    )
    for name in ("ca", "untrusted"):
        run(
            "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
            "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1",
            "-subj", f"/CN=synthetic-{name}",
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
            "-addext", "subjectKeyIdentifier=hash",
            "-keyout", str(directory / f"{name}.key"),
            "-out", str(directory / f"{name}.pem"),
        )
    for serial, (name, issuer, usage) in enumerate(
        (("server", "ca", "server"), ("client", "ca", "client"),
         ("stranger", "untrusted", "client")), start=2
    ):
        run(
            "openssl", "req", "-new", "-newkey", "ec", "-pkeyopt",
            "ec_paramgen_curve:prime256v1", "-nodes", "-subj", f"/CN={name}",
            "-keyout", str(directory / f"{name}.key"),
            "-out", str(directory / f"{name}.csr"),
        )
        run(
            "openssl", "x509", "-req", "-days", "1", "-set_serial", str(serial),
            "-in", str(directory / f"{name}.csr"),
            "-CA", str(directory / f"{issuer}.pem"),
            "-CAkey", str(directory / f"{issuer}.key"),
            "-extfile", str(extensions), "-extensions", usage,
            "-out", str(directory / f"{name}.pem"),
        )


class SyntheticUpstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def __getattr__(self, name):
        # Every method is accepted here, so upstream rejection cannot hide a leak.
        if name.startswith("do_"):
            return self.respond
        raise AttributeError(name)

    def respond(self):
        self.connection.settimeout(5)
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.server.requests.put((self.command, self.path, body))
        if self.headers.get("Upgrade", "").lower() == "websocket":
            self.send_response(101)
            self.send_header("Connection", "Upgrade")
            self.send_header("Upgrade", "websocket")
            self.send_header("Sec-WebSocket-Accept", WEBSOCKET_ACCEPT)
            self.end_headers()
            self.wfile.write(b"\x81\x05hello")
            # Require one masked client text frame, then echo its unmasked data.
            header = self.rfile.read(2)
            if len(header) != 2 or header[0] != 0x81 or header[1] != 0x85:
                self.server.errors.put("invalid synthetic client frame")
                return
            mask = self.rfile.read(4)
            payload = self.rfile.read(5)
            if len(mask) != 4 or len(payload) != 5:
                self.server.errors.put("incomplete synthetic client frame")
                return
            payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
            self.wfile.write(b"\x81\x05" + payload)
            self.close_connection = True
            return
        self.send_response(200)
        self.send_header("Content-Length", "9")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(b"synthetic")


class ProxyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        caddy = shutil.which(os.environ.get("CADDY_BIN", "caddy"))
        if caddy is None or shutil.which("openssl") is None:
            raise RuntimeError("real Caddy and OpenSSL are required; set CADDY_BIN")
        cls.temporary = tempfile.TemporaryDirectory(prefix="handkite-proxy-test-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.directory = Path(cls.temporary.name)
        certificates(cls.directory)

        cls.upstream = ThreadingHTTPServer(("127.0.0.1", 0), SyntheticUpstream)
        cls.upstream.requests = queue.Queue()
        cls.upstream.errors = queue.Queue()
        thread = threading.Thread(target=cls.upstream.serve_forever, daemon=True)
        thread.start()
        cls.addClassCleanup(cls.upstream.server_close)
        cls.addClassCleanup(thread.join, 5)
        cls.addClassCleanup(cls.upstream.shutdown)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            cls.port = reservation.getsockname()[1]

        # Change only fixture addresses and certificate files, not route or mTLS rules.
        config = CADDYFILE.read_text()
        substitutions = (
            ("anisette.example.invalid {", f"https://localhost:{cls.port} {{\n    bind 127.0.0.1"),
            ("tls {", f'tls "{cls.directory}/server.pem" "{cls.directory}/server.key" {{'),
            ("/etc/handkite/anisette-client-ca.pem", f'"{cls.directory}/ca.pem"'),
            ("127.0.0.1:6969", f"127.0.0.1:{cls.upstream.server_port}"),
        )
        for old, new in substitutions:
            if old not in config:
                raise RuntimeError("Caddy fixture substitution no longer matches the template")
            config = config.replace(old, new)
        config = (
            "{\n    admin off\n    auto_https off\n    persist_config off\n"
            "    servers {\n        protocols h1\n    }\n}\n" + config
        )
        config_path = cls.directory / "Caddyfile"
        config_path.write_text(config)
        environment = dict(os.environ, XDG_DATA_HOME=str(cls.directory / "data"),
                           XDG_CONFIG_HOME=str(cls.directory / "config"))
        run(caddy, "validate", "--adapter", "caddyfile", "--config", str(config_path),
            env=environment)
        cls.log = (cls.directory / "caddy.log").open("w+")
        cls.addClassCleanup(cls.log.close)
        cls.process = subprocess.Popen(
            [caddy, "run", "--adapter", "caddyfile", "--config", str(config_path)],
            env=environment, stdout=subprocess.DEVNULL, stderr=cls.log,
        )
        cls.addClassCleanup(cls.stop_caddy)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if cls.process.poll() is not None:
                raise RuntimeError("fixture Caddy exited before readiness")
            try:
                with socket.create_connection(("127.0.0.1", cls.port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("fixture Caddy did not listen within ten seconds")

    @classmethod
    def stop_caddy(cls):
        if cls.process.poll() is None:
            cls.process.terminate()
            try:
                cls.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                cls.process.kill()
                cls.process.wait(timeout=5)

    def tearDown(self):
        self.assertTrue(self.upstream.errors.empty(), "synthetic upstream frame error")
        self.assertTrue(self.upstream.requests.empty(), "unexpected upstream request")

    def connect(self, identity="client"):
        context = ssl.create_default_context(cafile=str(self.directory / "ca.pem"))
        context.set_alpn_protocols(["http/1.1"])
        if identity is not None:
            context.load_cert_chain(self.directory / f"{identity}.pem",
                                    self.directory / f"{identity}.key")
        connection = http.client.HTTPSConnection("localhost", self.port, context=context,
                                                 timeout=5)
        # Pin loopback without consulting DNS; keep localhost as the TLS server name.
        connection._create_connection = lambda *_args, **_kwargs: socket.create_connection(
            ("127.0.0.1", self.port), timeout=5
        )
        return connection

    def request(self, method, target, body=b"", headers=None, identity="client"):
        connection = self.connect(identity)
        try:
            connection.request(method, target, body=body, headers=headers or {})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def assert_blocked(self, method, target, headers=None):
        status, _body = self.request(method, target, headers=headers)
        self.assertEqual(status, 404, (method, target))
        self.assertTrue(self.upstream.requests.empty(), (method, target))

    def test_allowed_tuples_and_post_body(self):
        for method, target in ALLOWED:
            with self.subTest(method=method, target=target):
                body = b'{"synthetic":true}' if method == "POST" else b""
                self.assertEqual(self.request(method, target, body), (200, b"synthetic"))
                self.assertEqual(self.upstream.requests.get(timeout=1), (method, target, body))

    def test_forbidden_methods(self):
        for allowed_method, target in ALLOWED:
            for method in ("GET", "POST", "HEAD", "OPTIONS", "PUT", "PATCH", "DELETE",
                           "TRACE", "CONNECT", "get", "post"):
                if method != allowed_method:
                    with self.subTest(method=method, target=target):
                        self.assert_blocked(method, target)

    def test_forbidden_paths(self):
        targets = ["/", "/v1", "/v3", "/v3/", "/v3/unknown"]
        for _method, target in ALLOWED:
            targets.extend((target + "/", target + "/extra", target + "_extra",
                            target.upper(), "/prefix" + target, "/" + target,
                            target.replace("/v3/", "/v3/./"),
                            target.replace("/v3/", "/other/../v3/"),
                            target.replace("/v3/", "/v3%2f"),
                            target.replace("/v3/", "/%763/")))
        for target in targets:
            for method in ("GET", "POST"):
                with self.subTest(method=method, target=target):
                    self.assert_blocked(method, target)

    def test_queries_never_reach_upstream(self):
        queries = (
            "backend=android", "backend=macos", "backend=windows", "backend=",
            "backend", "backend=macos&backend=android", "%62ackend=android",
            "backend=%61ndroid", "Backend=android", "unrelated=value", "unrelated=",
            "x=1&backend=android", "backend=android;x=1", "backend=%zz", "&&", "",
        )
        for method, target in ALLOWED:
            for query in queries:
                with self.subTest(method=method, target=target, query=query):
                    self.assert_blocked(method, target + "?" + query)

    def test_websocket_query_and_wrong_tuple_are_blocked(self):
        for method, target in (
            ("GET", "/v3/provisioning_session?backend=android"),
            ("GET", "/v3/provisioning_session?backend=macos"),
            ("GET", "/v3/provisioning_session?backend=windows"),
            ("POST", "/v3/provisioning_session"),
            ("GET", "/v3/provisioning_session/extra"),
        ):
            with self.subTest(method=method, target=target):
                self.assert_blocked(method, target, WEBSOCKET_HEADERS)

    def test_client_auth_required_for_http_and_websocket(self):
        for identity in (None, "stranger"):
            for target, headers in (("/v3/client_info", {}),
                                    ("/v3/provisioning_session", WEBSOCKET_HEADERS)):
                with self.subTest(identity=identity, target=target):
                    with self.assertRaises(ssl.SSLError) as failure:
                        self.request("GET", target, headers=headers, identity=identity)
                    self.assertRegex(str(failure.exception),
                                     "CERTIFICATE_REQUIRED|UNKNOWN_CA|BAD_CERTIFICATE")
                    self.assertTrue(self.upstream.requests.empty())

    def test_websocket_forwards_frames_both_directions(self):
        connection = self.connect()
        try:
            connection.request("GET", "/v3/provisioning_session", headers=WEBSOCKET_HEADERS)
            response = connection.getresponse()
            self.assertEqual(response.status, 101)
            self.assertEqual(response.getheader("Sec-WebSocket-Accept"), WEBSOCKET_ACCEPT)
            self.assertEqual(response.fp.read(7), b"\x81\x05hello")
            mask = b"test"
            payload = bytes(value ^ mask[index % 4] for index, value in enumerate(b"world"))
            connection.sock.sendall(b"\x81\x85" + mask + payload)
            self.assertEqual(response.fp.read(7), b"\x81\x05world")
            self.assertEqual(self.upstream.requests.get(timeout=1),
                             ("GET", "/v3/provisioning_session", b""))
            response.close()
        finally:
            connection.close()


if __name__ == "__main__":
    unittest.main()
