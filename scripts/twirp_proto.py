#!/usr/bin/env python3
"""Minimal protobuf wire codec for the v2 CacheService smoke test.

Only the fields the smoke test needs are supported. Requests are encoded with
the proto field numbers of github.actions.results.api.v1.CacheService; omitted
fields stay out of the message, matching how @actions/cache omits unset fields.
Responses are decoded to their proto snake_case JSON names, which is what the
proxy contract mandates. Uses only the Python standard library.
"""

import json
import sys

# Method -> field name -> (field number, wire type).
REQUEST_FIELDS = {
    "CreateCacheEntry": {"key": (2, 2), "version": (3, 2)},
    "FinalizeCacheEntryUpload": {"key": (2, 2), "size_bytes": (3, 0), "version": (4, 2)},
    "GetCacheEntryDownloadURL": {"key": (2, 2), "restore_keys": (3, 2), "version": (4, 2)},
}

# Method -> field number -> snake_case response field name.
RESPONSE_FIELDS = {
    "CreateCacheEntry": {1: "ok", 2: "signed_upload_url", 3: "message"},
    "FinalizeCacheEntryUpload": {1: "ok", 2: "entry_id", 3: "message"},
    "GetCacheEntryDownloadURL": {1: "ok", 2: "signed_download_url", 3: "matched_key"},
}


def encode_varint(value):
    out = bytearray()
    while value > 0x7F:
        out.append((value & 0x7F) | 0x80)
        value >>= 7
    out.append(value)
    return bytes(out)


def encode_string(field, value):
    data = value.encode("utf-8")
    return encode_varint((field << 3) | 2) + encode_varint(len(data)) + data


def encode_request(method, args):
    out = bytearray()
    for name, (field, wire) in REQUEST_FIELDS[method].items():
        if name == "restore_keys":
            for value in args.get("restore_keys", []):
                out += encode_string(field, value)
        elif name == "size_bytes":
            if args.get("size_bytes") is not None:
                out += encode_varint((field << 3) | wire) + encode_varint(int(args["size_bytes"]))
        elif args.get(name) is not None:
            out += encode_string(field, args[name])
    return bytes(out)


def decode_varint(buf, pos):
    value = 0
    shift = 0
    while True:
        byte = buf[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, pos
        shift += 7


def decode_response(method, buf):
    fields = {}
    pos = 0
    while pos < len(buf):
        tag, pos = decode_varint(buf, pos)
        field, wire = tag >> 3, tag & 7
        if wire == 0:
            value, pos = decode_varint(buf, pos)
        elif wire == 2:
            length, pos = decode_varint(buf, pos)
            value = buf[pos:pos + length]
            pos += length
        else:
            raise ValueError("unsupported wire type: %d" % wire)
        fields.setdefault(field, []).append(value)
    out = {}
    for number, name in RESPONSE_FIELDS[method].items():
        if number not in fields:
            continue
        raw = fields[number][0]
        if name == "ok":
            out[name] = raw != 0
        elif name == "entry_id":
            out[name] = str(raw)
        else:
            out[name] = raw.decode("utf-8", "replace")
    return out


def main():
    argv = sys.argv[1:]
    if len(argv) < 2 or argv[0] not in ("encode", "decode"):
        sys.stderr.write(
            "usage: twirp_proto.py encode METHOD [--key K] [--version V] "
            "[--size-bytes N] [--restore-keys a,b]\n"
            "       twirp_proto.py decode METHOD < response.bin\n"
        )
        return 2
    action, method = argv[0], argv[1]
    if method not in REQUEST_FIELDS:
        sys.stderr.write("unknown method: %s\n" % method)
        return 2
    if action == "decode":
        json.dump(decode_response(method, sys.stdin.buffer.read()), sys.stdout)
        sys.stdout.write("\n")
        return 0
    args = {}
    pos = 2
    while pos < len(argv):
        flag = argv[pos]
        if flag in ("--key", "--version", "--size-bytes"):
            args[flag[2:].replace("-", "_")] = argv[pos + 1]
            pos += 2
        elif flag == "--restore-keys":
            args["restore_keys"] = argv[pos + 1].split(",")
            pos += 2
        else:
            sys.stderr.write("unknown flag: %s\n" % flag)
            return 2
    sys.stdout.buffer.write(encode_request(method, args))
    return 0


if __name__ == "__main__":
    sys.exit(main())
