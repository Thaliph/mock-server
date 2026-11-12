# E2E gRPC pair (GRP-28, scripts/e2e.sh). One file, two roles, mounted from the ConfigMap grpc-code into
# the derived image mimic-e2e-grpc (the pinned python:3.13-slim plus grpcio): no protobuf compiler, the
# messages are written and read by hand, which is all a gRPC client and server need to do.
#
#   ROLE=server   B: unary pkg.Users/Get on PORT (default 50051). The answer is the message
#                 {1: "ann", 2: $RESPONSE_TOKEN, 3: {1: 7}} and the trailers x-trace-bin and x-request-id.
#   ROLE=client   A: every second, a unary pkg.Users/Get to TARGET with the message
#                 {1: "ann", 2: $REQUEST_JWT} and the metadata authorization and x-note-bin.
#
# Each call writes one JSON access line to stdout (app, role, method, status), never a message or metadata. A client
# line also says whether the answer carries the placeholder `mimic-token` (`mocked`, GRP-35): the server of the e2e
# sends a real token, which only a mock replaces by its placeholder.
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


def log(status, mocked=None):
    line = {"app": NAME, "role": ROLE, "method": METHOD, "status": status}
    if mocked is not None:
        line["mocked"] = mocked
    print(json.dumps(line), flush=True)


def serve():
    token = os.environ["RESPONSE_TOKEN"]
    answer = string(1, "ann") + string(2, token) + message(3, varint(1 << 3 | 0) + varint(7))

    def get(request, context):
        context.set_trailing_metadata((("x-trace-bin", b"e2e-trace-canary"), ("x-request-id", "r-1")))
        log("OK")
        return answer

    handler = grpc.method_handlers_generic_handler(
        "pkg.Users", {"Get": grpc.unary_unary_rpc_method_handler(get, request_deserializer=None, response_serializer=None)}
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
    while True:
        try:
            reply = stub(request, metadata=metadata, timeout=5)
            log("OK", b"mimic-token" in reply)
        except grpc.RpcError as error:
            log(error.code().name)
        time.sleep(1)


{"server": serve, "client": call}[ROLE]()
