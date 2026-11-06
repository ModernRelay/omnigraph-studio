#!/usr/bin/env python3
"""A TCP proxy that delays traffic, for measuring Studio on a slow network.

Every chunk is held for half the round trip before it is passed on, each way, so a request
and its answer take `--rtt` longer than they would. `--bandwidth` caps each direction of each
connection (bytes per second); `--drop` closes that fraction of new connections at once, as a
lossy link that refuses some does.

    script/latency-proxy.py --listen 127.0.0.1:9787 --to 127.0.0.1:8787 --rtt 100
    OMNIGRAPH_STUDIO_SERVER=http://127.0.0.1:9787 script/measure-frames --scenarios

Standard library only.
"""

import argparse
import asyncio
import random
import time


def address(text):
    host, port = text.rsplit(":", 1)
    return host, int(port)


async def pipe(reader, writer, delay, bandwidth):
    """Copies `reader` to `writer`, each chunk passed on `delay` seconds after it arrived, in
    order, at no more than `bandwidth` bytes a second."""
    queue = asyncio.Queue()

    async def receive():
        while True:
            chunk = await reader.read(65536)
            await queue.put((time.monotonic() + delay, chunk))
            if not chunk:
                return

    async def send():
        while True:
            due, chunk = await queue.get()
            wait = due - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            if not chunk:
                break
            writer.write(chunk)
            await writer.drain()
            if bandwidth:
                await asyncio.sleep(len(chunk) / bandwidth)
        try:
            writer.write_eof()
        except (OSError, RuntimeError):
            pass

    await asyncio.gather(receive(), send())


async def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--listen", type=address, default=("127.0.0.1", 9787))
    parser.add_argument("--to", type=address, default=("127.0.0.1", 8787))
    parser.add_argument("--rtt", type=float, default=100, help="added round trip, ms")
    parser.add_argument("--bandwidth", type=int, default=0, help="bytes per second each way; 0 is unlimited")
    parser.add_argument("--drop", type=float, default=0, help="fraction of connections closed at once")
    args = parser.parse_args()
    delay = args.rtt / 2000

    async def serve(client_reader, client_writer):
        if random.random() < args.drop:
            client_writer.close()
            return
        try:
            server_reader, server_writer = await asyncio.open_connection(*args.to)
        except OSError:
            client_writer.close()
            return
        try:
            await asyncio.gather(
                pipe(client_reader, server_writer, delay, args.bandwidth),
                pipe(server_reader, client_writer, delay, args.bandwidth),
            )
        except (ConnectionError, OSError):
            pass
        finally:
            for writer in (client_writer, server_writer):
                writer.close()

    server = await asyncio.start_server(serve, *args.listen)
    print(
        f"latency-proxy: {args.listen[0]}:{args.listen[1]} -> {args.to[0]}:{args.to[1]}, "
        f"+{args.rtt:g} ms round trip",
        flush=True,
    )
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
