import base64
import json
import os
import socket
import struct
import sys
import time


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("connection closed")
        buf += chunk
    return buf


def read_frame(sock):
    b0, b1 = recv_exact(sock, 2)
    length = b1 & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(sock, 8))[0]
    mask = recv_exact(sock, 4) if b1 & 0x80 else None
    payload = recv_exact(sock, length)
    if mask:
        payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
    return b0 & 0x0F, payload


def send_text(sock, obj):
    payload = json.dumps(obj).encode()
    mask = os.urandom(4)
    header = bytes([0x81])
    if len(payload) < 126:
        header += bytes([0x80 | len(payload)])
    else:
        header += bytes([0x80 | 126]) + struct.pack(">H", len(payload))
    sock.sendall(header + mask + bytes(c ^ mask[i % 4] for i, c in enumerate(payload)))


def connect(port, deadline):
    while True:
        try:
            return socket.create_connection(("127.0.0.1", port), timeout=10)
        except OSError as err:
            if time.time() > deadline:
                raise SystemExit(f"obs-websocket not reachable on {port}: {err}")
            time.sleep(1)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 4455
    timeout = float(sys.argv[2]) if len(sys.argv) > 2 else 120
    sock = connect(port, time.time() + timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall(
        (
            f"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
            "Sec-WebSocket-Protocol: obswebsocket.json\r\n\r\n"
        ).encode()
    )
    response = b""
    while b"\r\n\r\n" not in response:
        response += recv_exact(sock, 1)
    print(response.split(b"\r\n", 1)[0].decode())
    _, hello = read_frame(sock)
    print("hello:", hello.decode())
    if json.loads(hello).get("op") != 0:
        raise SystemExit("first message is not Hello")
    send_text(sock, {"op": 1, "d": {"rpcVersion": 1}})
    _, identified = read_frame(sock)
    print("identified:", identified.decode())
    send_text(sock, {"op": 6, "d": {"requestType": "GetVersion", "requestId": "v"}})
    _, version = read_frame(sock)
    print("GetVersion:", version.decode())
    send_text(sock, {"op": 6, "d": {"requestType": "GetStreamStatus", "requestId": "s"}})
    _, status = read_frame(sock)
    print("GetStreamStatus:", status.decode())
    print("WEBSOCKET OK")


if __name__ == "__main__":
    main()
