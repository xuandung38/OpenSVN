#!/usr/bin/env python3
"""TLS proxy that breaks connections the way Apache on Windows, IIS and middleboxes do.

Terminates TLS for the client with the test server certificate and forwards every byte to an
upstream HTTPS server (the VisualSVN-like Apache). It never sends a TLS close_notify: every
close is a plain TCP close (FIN) or, where asked, a reset (RST). Faults, per client connection:

  --close-after N    after N exchanges, close (no close_notify) as soon as the client starts
                     the next request on the reused connection, before any response byte
  --reset-after N    the same, with a TCP reset
  --idle-reset S     reset a connection idle for S seconds: between requests, or in the middle
                     of a response the client stopped reading (not while the server works)
  --truncate-every K cut every K-th response bigger than --truncate-at bytes after that many
                     bytes and close without close_notify (a broken connection mid-body)

  scripts/test-env/flaky-tls-proxy.py --listen 18450 --upstream svn.opensvn.test:18443 \
      --cert $VSVN_DIR/certs/server.crt --key $VSVN_DIR/certs/server.key \
      --ca $VSVN_DIR/certs/ca.crt --close-after 3 --truncate-every 7

Test tool only: one thread per direction, no HTTP parsing. An exchange ends when the upstream
has sent bytes and the client then sends again. See docs/visualsvn-test-env.md.
"""

import argparse
import itertools
import socket
import ssl
import struct
import sys
import threading
import time

COUNTER = itertools.count(1)
COUNTER_LOCK = threading.Lock()


def log(msg):
    print(f"flaky-tls-proxy: {msg}", file=sys.stderr, flush=True)


def hard_close(sock, reset):
    """Prepares the end of the TCP connection under `sock`: with `reset`, the final close sends
    an RST (SO_LINGER 0). The close itself happens once both pump threads have stopped (they
    poll the `closed` flag), from one thread, without a TLS close_notify: OpenSSL objects must
    not be shut down under a thread that still uses them."""
    if not reset:
        return
    try:
        dup = socket.fromfd(sock.fileno(), socket.AF_INET, socket.SOCK_STREAM)
    except OSError:
        return
    try:
        dup.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    except OSError:
        pass
    finally:
        dup.close()


class Serialized:
    """A TLS socket shared by the two pump threads. OpenSSL objects must not be used by two
    threads at once, so every TLS call holds a lock; reads poll with a short timeout so a
    waiting reader does not hold the lock against a writer."""

    POLL = 0.05

    def __init__(self, sock):
        self.sock = sock
        self.lock = threading.Lock()
        sock.settimeout(self.POLL)

    def recv(self, stop):
        while not stop():
            with self.lock:
                try:
                    return self.sock.recv(65536)
                except (socket.timeout, ssl.SSLWantReadError):
                    pass
            time.sleep(0)
        return b""

    def sendall(self, data):
        with self.lock:
            self.sock.settimeout(None)
            try:
                self.sock.sendall(data)
            finally:
                self.sock.settimeout(self.POLL)

    def fileno(self):
        return self.sock.fileno()


class Conn:
    def __init__(self, args, client, cid):
        self.args = args
        self.client = client
        self.cid = cid
        self.lock = threading.Lock()
        self.exchanges = 0
        self.responding = False
        self.response_bytes = 0
        self.truncate_this = False
        self.last_activity = time.monotonic()
        # A request was forwarded and no response byte came back yet (the server is working).
        self.awaiting = False
        self.closed = False
        self.upstream = None

    def close(self, reset, why):
        with self.lock:
            if self.closed:
                return
            self.closed = True
        log(f"conn {self.cid}: {why}")
        hard_close(self.client, reset)
        if self.upstream is not None:
            hard_close(self.upstream, False)

    def client_to_upstream(self):
        try:
            while True:
                data = self.client_io.recv(lambda: self.closed)
                if not data or self.closed:
                    break
                with self.lock:
                    self.last_activity = time.monotonic()
                    self.awaiting = True
                    new_request = self.responding or self.exchanges == 0 and self.response_bytes == 0
                    if self.responding:
                        self.responding = False
                        self.exchanges += 1
                        self.response_bytes = 0
                    exchanges = self.exchanges
                if new_request and exchanges > 0:
                    if self.args.close_after and exchanges >= self.args.close_after:
                        self.close(False, f"closing before request {exchanges + 1} (no close_notify)")
                        return
                    if self.args.reset_after and exchanges >= self.args.reset_after:
                        self.close(True, f"resetting before request {exchanges + 1}")
                        return
                self.upstream_io.sendall(data)
        except OSError:
            pass
        self.close(False, "client side ended")

    def upstream_to_client(self):
        try:
            while True:
                data = self.upstream_io.recv(lambda: self.closed)
                if not data or self.closed:
                    break
                with self.lock:
                    self.last_activity = time.monotonic()
                    self.awaiting = False
                    if not self.responding:
                        self.responding = True
                        self.response_bytes = 0
                        self.truncate_this = False
                        if self.args.truncate_every:
                            with COUNTER_LOCK:
                                n = next(COUNTER)
                            self.truncate_this = n % self.args.truncate_every == 0
                    before = self.response_bytes
                    self.response_bytes += len(data)
                    cut = self.truncate_this and self.response_bytes > self.args.truncate_at
                if cut:
                    keep = max(0, self.args.truncate_at - before)
                    self.client_io.sendall(data[:keep])
                    self.close(False, f"truncated a response after {self.args.truncate_at} bytes")
                    return
                self.client_io.sendall(data)
        except OSError:
            pass
        # Upstream closed (keep-alive limit): pass it on without close_notify.
        self.close(False, "upstream closed")

    def idle_watch(self):
        while not self.closed:
            time.sleep(0.1)
            with self.lock:
                idle = 0 if self.awaiting else time.monotonic() - self.last_activity
            if idle >= self.args.idle_reset:
                self.close(True, f"reset after {idle:.1f}s idle")
                return


def serve(args):
    server_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server_ctx.load_cert_chain(args.cert, args.key)
    server_ctx.set_alpn_protocols(["http/1.1"])
    up_ctx = ssl.create_default_context(cafile=args.ca)
    # The test CA has no key usage extension, which Python 3.13+ rejects by default.
    up_ctx.verify_flags &= ~getattr(ssl, "VERIFY_X509_STRICT", 0)
    up_host, up_port = args.upstream.rsplit(":", 1)
    listener = socket.create_server(("127.0.0.1", args.listen), reuse_port=False)
    log(f"listening on 127.0.0.1:{args.listen}, upstream {args.upstream}")
    for cid in itertools.count(1):
        raw, _ = listener.accept()
        threading.Thread(target=handle, args=(args, server_ctx, up_ctx, up_host, int(up_port), raw, cid), daemon=True).start()


def handle(args, server_ctx, up_ctx, up_host, up_port, raw, cid):
    try:
        client = server_ctx.wrap_socket(raw, server_side=True)
    except (OSError, ssl.SSLError):
        raw.close()
        return
    conn = Conn(args, client, cid)
    try:
        up_raw = socket.create_connection((up_host, up_port))
        conn.upstream = up_ctx.wrap_socket(up_raw, server_hostname=up_host)
    except OSError as e:
        log(f"conn {cid}: upstream failed: {e}")
        hard_close(client, False)
        client.close()
        return
    conn.client_io = Serialized(client)
    conn.upstream_io = Serialized(conn.upstream)
    threads = [
        threading.Thread(target=conn.client_to_upstream, daemon=True),
        threading.Thread(target=conn.upstream_to_client, daemon=True),
    ]
    if args.idle_reset:
        threads.append(threading.Thread(target=conn.idle_watch, daemon=True))
    for t in threads:
        t.start()
    for t in threads[:2]:
        t.join()
    conn.closed = True
    for sock in (client, conn.upstream):
        try:
            sock.close()
        except OSError:
            pass


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--listen", type=int, required=True)
    p.add_argument("--upstream", required=True, help="host:port of the HTTPS server")
    p.add_argument("--cert", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--ca", required=True, help="CA that signed the upstream certificate")
    p.add_argument("--close-after", type=int, default=0)
    p.add_argument("--reset-after", type=int, default=0)
    p.add_argument("--idle-reset", type=float, default=0.0)
    p.add_argument("--truncate-every", type=int, default=0)
    p.add_argument("--truncate-at", type=int, default=2048)
    serve(p.parse_args())


if __name__ == "__main__":
    main()
