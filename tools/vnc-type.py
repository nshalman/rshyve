#!/usr/bin/env python3
"""Send keystrokes to the guest over RFB.

usage: vnctype.py <sock> <action> [arg]
  shiftf10        Shift+F10 (opens the WinPE command prompt in Setup)
  type "text"     type literal text, synthesising Shift for uppercase/symbols
  key <name>      one named key: enter, tab, esc, up, down, left, right, f8
"""
import socket, struct, sys, time

sock_path, action = sys.argv[1], sys.argv[2]
arg = sys.argv[3] if len(sys.argv) > 3 else ""

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(20)
s.connect(sock_path)


def recvn(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


recvn(12)
s.sendall(b"RFB 003.008\n")
n = recvn(1)[0]
recvn(n)
s.sendall(bytes([1]))
assert struct.unpack(">I", recvn(4))[0] == 0, "auth failed"
s.sendall(bytes([1]))
recvn(4)
recvn(16)
nl = struct.unpack(">I", recvn(4))[0]
recvn(nl)

SHIFT = 0xFFE1
NAMED = {
    "enter": 0xFF0D, "tab": 0xFF09, "esc": 0xFF1B, "bs": 0xFF08,
    "up": 0xFF52, "down": 0xFF54, "left": 0xFF51, "right": 0xFF53,
    "f8": 0xFFC5, "f10": 0xFFC7,
}
# Characters that require Shift on a US layout.
SHIFTED = set('~!@#$%^&*()_+{}|:"<>?')


def key(sym, down):
    s.sendall(struct.pack(">BBHI", 4, 1 if down else 0, 0, sym))
    time.sleep(0.03)


def tap(sym):
    key(sym, True)
    key(sym, False)


if action == "shiftf10":
    key(SHIFT, True)
    tap(0xFFC7)
    key(SHIFT, False)
    print("sent Shift+F10")
elif action == "key":
    tap(NAMED[arg])
    print("sent", arg)
elif action == "type":
    for ch in arg:
        need_shift = ch.isupper() or ch in SHIFTED
        if need_shift:
            key(SHIFT, True)
        tap(ord(ch))
        if need_shift:
            key(SHIFT, False)
    print("typed %d chars" % len(arg))
else:
    sys.exit("unknown action")

time.sleep(0.5)
