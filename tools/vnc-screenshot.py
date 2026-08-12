#!/usr/bin/env python3
"""Grab one framebuffer update over RFB and write it as a PNG.

Lets us actually look at what the guest is displaying, which is the only
practical way to read a graphical Windows Setup screen from a headless host.
Pure stdlib: no PIL on the build host.
"""
import socket, struct, sys, zlib

sock_path = sys.argv[1]
out_path = sys.argv[2] if len(sys.argv) > 2 else "/tmp/screen.png"

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
s.connect(sock_path)


def recvn(n):
    buf = b""
    while len(buf) < n:
        c = s.recv(n - len(buf))
        if not c:
            raise EOFError("short read %d/%d" % (len(buf), n))
        buf += c
    return buf


recvn(12)
s.sendall(b"RFB 003.008\n")
ntypes = recvn(1)[0]
recvn(ntypes)
s.sendall(bytes([1]))
if struct.unpack(">I", recvn(4))[0] != 0:
    sys.exit("auth failed")
s.sendall(bytes([1]))
w, h = struct.unpack(">HH", recvn(4))
pf = recvn(16)
bpp = pf[0]
namelen = struct.unpack(">I", recvn(4))[0]
recvn(namelen)

s.sendall(struct.pack(">BBHi", 2, 0, 1, 0))          # SetEncodings: Raw
s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))  # full update

if recvn(1)[0] != 0:
    sys.exit("unexpected server message")
recvn(1)
nrects = struct.unpack(">H", recvn(2))[0]

step = bpp // 8
fb = bytearray(w * h * 3)
for _ in range(nrects):
    rx, ry, rw, rh, enc = struct.unpack(">HHHHi", recvn(12))
    if enc != 0:
        continue
    data = recvn(rw * rh * step)
    for row in range(rh):
        src = row * rw * step
        dst = ((ry + row) * w + rx) * 3
        for col in range(rw):
            p = src + col * step
            # server pixel format here is little-endian BGRX
            fb[dst + col * 3 + 0] = data[p + 2]
            fb[dst + col * 3 + 1] = data[p + 1]
            fb[dst + col * 3 + 2] = data[p + 0]


def png(width, height, rgb):
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # filter: none
        raw += rgb[y * width * 3:(y + 1) * width * 3]

    def chunk(tag, payload):
        c = struct.pack(">I", len(payload)) + tag + payload
        return c + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
            + chunk(b"IEND", b""))


with open(out_path, "wb") as f:
    f.write(png(w, h, fb))
nonblack = sum(1 for i in range(0, len(fb), 3) if fb[i] or fb[i + 1] or fb[i + 2])
print("wrote %s  %dx%d  %d non-black pixels" % (out_path, w, h, nonblack))
