#!/usr/bin/env python3
"""A stand-in greetd for previewing breadgreet without a real session.

Speaks the greetd IPC wire format (`u32` native-endian length prefix + JSON
body, see the `greetd_ipc` crate) on a Unix socket. It never touches PAM and
never starts anything — `start_session` just acknowledges and the greeter
exits, exactly as it would on a real login.

    ./mock-greetd.py /run/user/1000/breadgreet-preview.sock [password]

Default password is "bread"; any other answer gets the auth-error path so you
can see the shake + red status line.
"""

import json
import os
import socket
import struct
import sys

PASSWORD = sys.argv[2] if len(sys.argv) > 2 else "bread"


def read_frame(conn):
    hdr = b""
    while len(hdr) < 4:
        chunk = conn.recv(4 - len(hdr))
        if not chunk:
            return None
        hdr += chunk
    (length,) = struct.unpack("=I", hdr)
    body = b""
    while len(body) < length:
        chunk = conn.recv(length - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body)


def send(conn, obj):
    body = json.dumps(obj).encode()
    conn.sendall(struct.pack("=I", len(body)) + body)


def handle(conn):
    while True:
        req = read_frame(conn)
        if req is None:
            return
        kind = req.get("type")
        if kind == "create_session":
            print(f"  create_session  username={req.get('username')!r}")
            send(conn, {
                "type": "auth_message",
                "auth_message_type": "secret",
                "auth_message": "Password: ",
            })
        elif kind == "post_auth_message_response":
            if req.get("response") == PASSWORD:
                print("  auth ok -> success")
                send(conn, {"type": "success"})
            else:
                print("  auth bad -> auth_error")
                send(conn, {
                    "type": "error",
                    "error_type": "auth_error",
                    "description": "Login incorrect",
                })
        elif kind == "start_session":
            print(f"  start_session   cmd={req.get('cmd')}")
            send(conn, {"type": "success"})
        elif kind == "cancel_session":
            print("  cancel_session")
            send(conn, {"type": "success"})
        else:
            print(f"  ?? {req}")
            send(conn, {
                "type": "error",
                "error_type": "error",
                "description": f"mock-greetd: unknown request {kind}",
            })


def main():
    sys.stdout.reconfigure(line_buffering=True)
    if len(sys.argv) < 2:
        sys.exit(f"usage: {sys.argv[0]} <socket-path> [password]")
    path = sys.argv[1]
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)
    print(f"mock-greetd listening on {path}  (password: {PASSWORD!r})")
    try:
        while True:
            conn, _ = srv.accept()
            with conn:
                handle(conn)
    except KeyboardInterrupt:
        pass
    finally:
        srv.close()
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass


if __name__ == "__main__":
    main()
