# E2E gRPC pair (GRP-28, scripts/e2e.sh). One file, two roles, mounted from the ConfigMap grpc-code into
# the derived image mimic-e2e-grpc (the pinned python:3.13-slim plus grpcio): no protobuf compiler, the
# messages are written and read by hand, which is all a gRPC client and server need to do.
#
#   ROLE=server   B: unary pkg.Users/Get on PORT (default 50051). The answer is the message
#                 {1: "ann", 2: $RESPONSE_TOKEN, 3: {1: 7}} and the trailers x-trace-bin and x-request-id.
#                 And the server-streaming pkg.Users/Watch (GRP-46): 5 messages {1: n} 200 ms apart, the third also
#                 holding {2: $RESPONSE_TOKEN}, then the same trailers.
#   ROLE=client   A: every second, a unary pkg.Users/Get to TARGET with the message
#                 {1: "ann", 2: $REQUEST_JWT} and the metadata authorization and x-note-bin, then a pkg.Users/Watch
#                 with the message {1: "ann"}.
#
# Each call writes one JSON access line to stdout (app, role, method, status), never a message or metadata. A client
# line also says whether the answer carries the placeholder `mimic-token` (`mocked`, GRP-35): the server of the e2e
# sends a real token, which only a mock replaces by its placeholder. A Watch line of the client also holds the
# numbers of the messages in the order they came (`order`) and the time from the first to the last (`span_ms`).
import json
import os
import sys
import time
from concurrent import futures

import grpc

ROLE = os.environ["ROLE"]
NAME = os.environ.get("APP_NAME", ROLE)
PORT = int(os.environ.get("PORT", "50051"))
METHOD = "/pkg.Users/Get"
WATCH = "/pkg.Users/Watch"
STEP_SECONDS = 0.2


def varint(n):
    out = bytearray()
    while True:
        byte = n & 0x7F
        n >>= 7
        out.append(byte | (0x80 if n else 0))
        if not n:
            return bytes(out)


def string(number, text):
    data = text.encode()
    return varint(number << 3 | 2) + varint(len(data)) + data


def message(number, inner):
    return varint(number << 3 | 2) + varint(len(inner)) + inner


def log(status, mocked=None, method=METHOD, **extra):
    line = {"app": NAME, "role": ROLE, "method": method, "status": status}
    if mocked is not None:
        line["mocked"] = mocked
    line.update(extra)
    print(json.dumps(line), flush=True)


def serve():
    token = os.environ["RESPONSE_TOKEN"]
    answer = string(1, "ann") + string(2, token) + message(3, varint(1 << 3 | 0) + varint(7))

    def get(request, context):
        context.set_trailing_metadata((("x-trace-bin", b"e2e-trace-canary"), ("x-request-id", "r-1")))
        log("OK")
        return answer

    def watch(request, context):
        context.set_trailing_metadata((("x-trace-bin", b"e2e-trace-canary"), ("x-request-id", "r-1")))
        for n in range(1, 6):
            if n > 1:
                time.sleep(STEP_SECONDS)
            yield varint(1 << 3 | 0) + varint(n) + (string(2, token) if n == 3 else b"")
        log("OK", method=WATCH)

    handler = grpc.method_handlers_generic_handler(
        "pkg.Users",
        {
            "Get": grpc.unary_unary_rpc_method_handler(get, request_deserializer=None, response_serializer=None),
            "Watch": grpc.unary_stream_rpc_method_handler(watch, request_deserializer=None, response_serializer=None),
        },
    )
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    server.add_generic_rpc_handlers((handler,))
    server.add_insecure_port(f"0.0.0.0:{PORT}")
    server.start()
    server.wait_for_termination()


def call():
    request = string(1, "ann") + string(2, os.environ["REQUEST_JWT"])
    metadata = (("authorization", "Bearer " + os.environ["REQUEST_BEARER"]), ("x-note-bin", b"e2e-note-canary"))
    channel = grpc.insecure_channel(os.environ["TARGET"])
    stub = channel.unary_unary(METHOD, request_serializer=None, response_deserializer=None)
    watch = channel.unary_stream(WATCH, request_serializer=None, response_deserializer=None)
    while True:
        try:
            reply = stub(request, metadata=metadata, timeout=5)
            log("OK", b"mimic-token" in reply)
        except grpc.RpcError as error:
            log(error.code().name)
        try:
            first = time.monotonic()
            replies = list(watch(string(1, "ann"), timeout=10))
            span = round((time.monotonic() - first) * 1000)
            # Each message starts with field 1 (the tag 0x08), a number below 128.
            order = [r[1] for r in replies]
            log("OK", any(b"mimic-token" in r for r in replies), method=WATCH, order=order, span_ms=span)
        except grpc.RpcError as error:
            log(error.code().name, method=WATCH)
        time.sleep(1)


{"server": serve, "client": call}[ROLE]()
