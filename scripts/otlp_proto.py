"""Small proto metadata parser shared by OTLP code generators."""

from __future__ import annotations

import re


FIELD_RE = re.compile(
    r"^\s*(?:(repeated)\s+)?([.\w]+)\s+([A-Za-z_]\w*)\s*=\s*(\d+)\s*(?:\[[^\]]*\])?\s*;"
)
MESSAGE_RE = re.compile(r"^\s*message\s+([A-Za-z_]\w*)\s*\{")
ONEOF_RE = re.compile(r"^\s*oneof\s+([A-Za-z_]\w*)\s*\{")

PROTO_TYPE_TO_WIRE = {
    "bool": "varint",
    "int32": "varint",
    "int64": "varint",
    "uint32": "varint",
    "uint64": "varint",
    "sint32": "varint",
    "sint64": "varint",
    "fixed32": "fixed32",
    "sfixed32": "fixed32",
    "fixed64": "fixed64",
    "sfixed64": "fixed64",
    "float": "fixed32",
    "double": "fixed64",
    "string": "len",
    "bytes": "len",
    "SeverityNumber": "varint",
}


def short_proto_type(proto_type: str) -> str:
    return proto_type.rsplit(".", 1)[-1]


def proto_wire_for(proto_type: str) -> str:
    short_type = short_proto_type(proto_type)
    if short_type in PROTO_TYPE_TO_WIRE:
        return PROTO_TYPE_TO_WIRE[short_type]
    return "len"


def parse_proto_fields(proto_files: list) -> dict:
    messages = {}
    for path in proto_files:
        if not path.exists():
            raise FileNotFoundError(f"vendored OTLP proto file missing: {path}")

        current_message = None
        in_oneof = None
        for raw_line in path.read_text().splitlines():
            line = raw_line.split("//", 1)[0].strip()
            if not line:
                continue

            if current_message is None:
                message_match = MESSAGE_RE.match(line)
                if message_match:
                    current_message = message_match.group(1)
                    messages.setdefault(current_message, {})
                continue

            oneof_match = ONEOF_RE.match(line)
            if oneof_match:
                in_oneof = oneof_match.group(1)
                continue

            field_match = FIELD_RE.match(line)
            if field_match:
                repeated, proto_type, field_name, number = field_match.groups()
                messages[current_message][field_name] = {
                    "name": field_name,
                    "number": int(number),
                    "wire": proto_wire_for(proto_type),
                    "proto_type": short_proto_type(proto_type),
                    "repeated": repeated is not None,
                    "oneof": in_oneof,
                }
                continue

            if "}" in line:
                if in_oneof is not None:
                    in_oneof = None
                else:
                    current_message = None

    return messages
