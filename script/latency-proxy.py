#!/usr/bin/env python3
"""Pass HTTP requests to an Omnigraph server after a fixed delay, so a local server answers with
a real server's wire time (spec 030, the scenario runner). Each request waits `--delay-ms` before
it is sent on; the default, 700 ms, is the personal graph's median request (spec 030,
"Measured"). Requests are passed on as they are, many at once, one connection to the server
each; the answer is read whole and sent back with its length.

    script/latency-proxy.py http://127.0.0.1:8787            # prints the address it serves
    script/latency-proxy.py http://127.0.0.1:8787 --delay-ms 0 --port 8788

It prints `http://127.0.0.1:<port>` on its first line once it listens. It logs nothing else:
not the requests, their bodies or their tokens.
"""

import argparse
import http.client
import http.server
import sys
import time
import urllib.parse

# Headers that describe one connection, not the request or the answer (RFC 9110 7.6.1).
HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}


def handler(upstream, delay):
    class Delayed(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def relay(self):
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else None
            time.sleep(delay)
            headers = {
                name: value
                for name, value in self.headers.items()
                if name.lower() not in HOP_BY_HOP and name.lower() != "host"
            }
            connection = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=600)
            try:
                connection.request(self.command, self.path, body=body, headers=headers)
                answer = connection.getresponse()
                data = answer.read()
            except OSError:
                self.send_error(502)
                return
            finally:
                connection.close()
            self.send_response_only(answer.status, answer.reason)
            for name, value in answer.getheaders():
                if name.lower() not in HOP_BY_HOP and name.lower() != "content-length":
                    self.send_header(name, value)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)

        do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = relay

        def log_message(self, format, *args):
            pass

    return Delayed


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("upstream", help="the server's address, http://host:port")
    parser.add_argument("--delay-ms", type=int, default=700)
    parser.add_argument("--port", type=int, default=0, help="0 picks a free one")
    args = parser.parse_args()
    upstream = urllib.parse.urlsplit(args.upstream)
    if upstream.scheme != "http" or not upstream.hostname:
        sys.exit("latency-proxy: the server's address must be http://host:port")
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", args.port), handler(upstream, args.delay_ms / 1000)
    )
    server.daemon_threads = True
    print(f"http://127.0.0.1:{server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
