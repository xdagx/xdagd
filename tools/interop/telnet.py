#!/usr/bin/env python3
"""Run one command on xdagj's telnet admin shell and print the reply."""
import socket, sys, time, re
IAC, DONT, DO, WONT, WILL, SB, SE = 255, 254, 253, 252, 251, 250, 240
def strip(sock, data):
    out, i = bytearray(), 0
    while i < len(data):
        b = data[i]
        if b == IAC and i + 1 < len(data):
            c = data[i + 1]
            if c in (DO, DONT, WILL, WONT) and i + 2 < len(data):
                opt = data[i + 2]
                sock.sendall(bytes([IAC, WONT if c == DO else DONT, opt])) if c in (DO, WILL) else None
                i += 3; continue
            if c == SB:
                j = data.find(bytes([IAC, SE]), i)
                i = (j + 2) if j >= 0 else len(data); continue
            i += 2; continue
        out.append(b); i += 1
    return bytes(out)
def read(sock, wait):
    sock.settimeout(0.3); buf = b""; end = time.time() + wait
    while time.time() < end:
        try:
            d = sock.recv(65536)
            if not d: break
            buf += strip(sock, d); end = max(end, time.time() + 0.6)
        except socket.timeout:
            pass
    return buf
s = socket.create_connection(("127.0.0.1", 6101), timeout=5)
read(s, 1.5)
s.sendall(b"root\r\n"); read(s, 1.5)
cmd = " ".join(sys.argv[1:])
s.sendall(cmd.encode() + b"\r\n")
out = read(s, float(8))
s.close()
text = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", out).decode(errors="replace").replace("\r", "")
print(text.strip())
