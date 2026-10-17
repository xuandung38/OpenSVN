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
  --stale-after S    a connection idle for S seconds or more is reset under the next request,
                     which is not forwarded (a proxy, firewall or NAT that dropped the idle
                     connection without telling the client)
  --log-requests     log the request line of every request the client starts, forwarded or not
                     ("conn N: request METHOD URI"), so a test can count what was sent
  --truncate-every K cut every K-th response bigger than --truncate-at bytes after that many
                     bytes and close without close_notify (a broken connection mid-body). With
                     K > 1 the break is transient: the next request with the request line of a
                     cut one (the client sending it again) is not cut, wherever the count is.
                     Otherwise a client whose retry cycle has a multiple of K requests (OPTIONS
                     plus REPORT, K = 2) would meet the cut at the same byte every time: a server
                     that always breaks at the same point, which is what --truncate-every 1 is for
  --truncate-method M  only count and cut responses to these methods (comma-separated, e.g.
                     REPORT,GET); with --truncate-every 1 every such response is cut at the
                     same offset (a server or proxy that always breaks at the same point)
  --outage-at-bytes B --outage-for S
                     a network outage: once B response bytes went to clients in total, every
                     connection, open or new, goes silent for S seconds (bytes are dropped, no
                     FIN, no RST: Wi-Fi gone, VPN reconnecting, laptop asleep). Connections
                     open during the outage stay dead afterwards; new ones work again
  --outage-file F    the same outage, for as long as the file F exists (a test drives it)

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
# Request lines ("METHOD URI") whose response was cut and that were not sent again yet.
CUT_LINES = set()


class Outage:
    """Shared outage state. A connection that saw the network go down stays dead (its peer's
    TCP state is gone after a real outage); the listener keeps accepting, but a connection
    accepted during the outage gets no byte until it ends, and then also stays dead."""

    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.bytes = 0
        self.until = None
        self.done = False

    def add_bytes(self, n):
        if not self.args.outage_at_bytes:
            return
        with self.lock:
            self.bytes += n
            if not self.done and self.until is None and self.bytes >= self.args.outage_at_bytes:
                self.until = time.monotonic() + self.args.outage_for
                self.done = True
                log(f"network down for {self.args.outage_for:.0f}s after {self.bytes} bytes")
                # Said when the outage ends, whether or not a client is trying at that moment.
                timer = threading.Timer(self.args.outage_for + 0.1, self.down)
                timer.daemon = True
                timer.start()

    def down(self):
        if self.args.outage_file:
            import os

            if os.path.exists(self.args.outage_file):
                return True
        with self.lock:
            if self.until is None:
                return False
            if time.monotonic() < self.until:
                return True
            self.until = None
            log("network up again")
            return False


OUTAGE = None


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
        # Method of the request being answered (first word of the exchange's first bytes).
        self.method = ""
        # Request line ("METHOD URI") of the request being answered.
        self.request_line = ""
        self.started = False
        self.last_activity = time.monotonic()
        # A request was forwarded and no response byte came back yet (the server is working).
        self.awaiting = False
        self.closed = False
        self.upstream = None
        # The network went down under this connection: it never carries a byte again.
        self.dead = False

    def blackholed(self):
        if self.dead:
            return True
        if OUTAGE is not None and OUTAGE.down():
            if not self.dead:
                log(f"conn {self.cid}: network down, dropping its bytes")
            self.dead = True
        return self.dead

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
                if self.blackholed():
                    continue
                with self.lock:
                    idle = time.monotonic() - self.last_activity
                    self.last_activity = time.monotonic()
                    self.awaiting = True
                    new_request = self.responding or self.exchanges == 0 and self.response_bytes == 0
                    starts = self.responding or not self.started
                    if starts:
                        # The first bytes of a request (not more of its body).
                        self.started = True
                        self.method = data.split(b" ", 1)[0].decode("ascii", "replace")
                        line = data.split(b"\r\n", 1)[0].decode("ascii", "replace")
                        self.request_line = " ".join(line.split(" ")[:2])
                    if self.responding:
                        self.responding = False
                        self.exchanges += 1
                        self.response_bytes = 0
                    exchanges = self.exchanges
                if starts and self.args.log_requests:
                    log(f"conn {self.cid}: request {' '.join(line.split(' ')[:2])}")
                if new_request and exchanges > 0:
                    if self.args.close_after and exchanges >= self.args.close_after:
                        self.close(False, f"closing before request {exchanges + 1} ({self.method}, no close_notify)")
                        return
                    if self.args.reset_after and exchanges >= self.args.reset_after:
                        self.close(True, f"resetting before request {exchanges + 1} ({self.method})")
                        return
                    if self.args.stale_after and idle >= self.args.stale_after:
                        self.close(True, f"resetting a connection idle {idle:.1f}s under request {exchanges + 1} ({self.method})")
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
                if self.blackholed():
                    continue
                OUTAGE.add_bytes(len(data))
                with self.lock:
                    self.last_activity = time.monotonic()
                    self.awaiting = False
                    if not self.responding:
                        self.responding = True
                        self.response_bytes = 0
                        self.truncate_this = False
                        methods = self.args.truncate_method
                        if self.args.truncate_every and (not methods or self.method in methods):
                            with COUNTER_LOCK:
                                n = next(COUNTER)
                                # The client sends a cut request again: let it through.
                                again = self.request_line in CUT_LINES
                                CUT_LINES.discard(self.request_line)
                            self.truncate_this = n % self.args.truncate_every == 0 and not (
                                again and self.args.truncate_every > 1
                            )
                    before = self.response_bytes
                    self.response_bytes += len(data)
                    cut = self.truncate_this and self.response_bytes > self.args.truncate_at
                if cut:
                    with COUNTER_LOCK:
                        CUT_LINES.add(self.request_line)
                    keep = max(0, self.args.truncate_at - before)
                    self.client_io.sendall(data[:keep])
                    self.close(False, f"truncated a {self.method} response after {self.args.truncate_at} bytes")
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
    if OUTAGE.down():
        # No handshake while the network is down: the client's connect or TLS handshake hangs
        # until its own timeout. The connection is never served.
        log(f"conn {cid}: accepted during the outage, never answered")
        while True:
            try:
                if not raw.recv(65536):
                    break
            except OSError:
                break
        raw.close()
        return
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
    p.add_argument("--stale-after", type=float, default=0.0)
    p.add_argument("--log-requests", action="store_true")
    p.add_argument("--truncate-every", type=int, default=0)
    p.add_argument("--truncate-at", type=int, default=2048)
    p.add_argument("--outage-at-bytes", type=int, default=0)
    p.add_argument("--outage-for", type=float, default=0.0)
    p.add_argument("--outage-file", default="")
    p.add_argument("--truncate-method", type=lambda v: [m.strip().upper() for m in v.split(",") if m.strip()], default=[])
    global OUTAGE
    args = p.parse_args()
    OUTAGE = Outage(args)
    serve(args)


if __name__ == "__main__":
    main()
