#!/usr/bin/env python3
"""Raw TCP proxy that adds a one-way delay to every chunk in both directions, plus one RTT
before the first byte of a connection (the TCP handshake a remote server would cost).
TLS stays end to end. Usage: latency-proxy.py LISTEN_PORT UPSTREAM_HOST UPSTREAM_PORT ONE_WAY_MS"""

import asyncio
import sys
import time

LISTEN, UP_HOST, UP_PORT, ONE_WAY = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), float(sys.argv[4]) / 1000


async def pump(reader, writer, first_extra):
    queue = asyncio.Queue()

    async def sender():
        while True:
            due, data = await queue.get()
            delay = due - time.monotonic()
            if delay > 0:
                await asyncio.sleep(delay)
            if data is None:
                writer.close()
                return
            writer.write(data)
            await writer.drain()

    task = asyncio.create_task(sender())
    extra = first_extra
    try:
        while True:
            data = await reader.read(65536)
            queue.put_nowait((time.monotonic() + ONE_WAY + extra, data or None))
            extra = 0
            if not data:
                break
    except (ConnectionError, OSError):
        queue.put_nowait((time.monotonic(), None))
    await task


async def handle(cr, cw):
    try:
        ur, uw = await asyncio.open_connection(UP_HOST, UP_PORT)
    except OSError:
        cw.close()
        return
    await asyncio.gather(pump(cr, uw, 2 * ONE_WAY), pump(ur, cw, 0), return_exceptions=True)


async def main():
    server = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    async with server:
        await server.serve_forever()


asyncio.run(main())
