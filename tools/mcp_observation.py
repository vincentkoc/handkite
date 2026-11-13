"""Decode full or compact MCP observations into privacy-safe summary fields.

The caller supplies one parsed JSON-RPC response and its independently pinned
request ID, tool name and device key. No transport, time or cross-call checks
are performed here. Unknown fields are ignored, never returned or modified.
"""
import base64
import binascii
import json
import re


MAX_RESPONSE = 16 * 1024 * 1024
MAX_COMPACT = 2 * 1024 * 1024


def _tree_chars(payload):
    tree = payload.get("accessibility_tree")
    _require(type(tree) is str and bool(tree.strip()), "tree")
    if not any(key in payload for key in
               ("tree_format", "tree_format_reason", "tree_format_version")):
        return len(tree)
    _require(type(payload.get("tree_format_version")) is int and
             payload["tree_format_version"] == 1, "tree")
    if payload.get("tree_format") == "full":
        _require(payload.get("tree_format_reason") in
                 ("malformed_xml", "unsupported_xml", "limit_exceeded"), "tree")
    else:
        _require(payload.get("tree_format") == "compact" and
                 payload.get("tree_format_reason") == "requested" and
                 len(tree.encode("utf-8")) <= MAX_COMPACT, "tree")
    return len(tree)


def _require(ok, category):
    if not ok:
        raise ValueError("mcp_observation_" + category)


def _pairs(items):
    result = {}
    for key, value in items:
        _require(key not in result, "payload")
        result[key] = value
    return result


def _request_id(value):
    return (type(value) is str or
            (type(value) is int and -(2 ** 63) <= value < 2 ** 64))


def decode_observation(response, expected_request_id, method, device_key):
    """Return wda_ready, lock, tree_chars and image_bytes, or a fixed ValueError.

    Status text is metadata directly; observe text wraps metadata and a tree,
    followed by one PNG/JPEG MIME image with nonempty strict standard base64.
    Compact tree_chars counts its XML presentation. Image codec validity is
    not established by this contract decoder.
    """
    _require(method in ("handkite_status", "handkite_observe") and
             _request_id(expected_request_id) and type(device_key) is str and
             re.fullmatch(r"[A-Za-z0-9_-]{1,64}", device_key), "arguments")
    _require(type(response) is dict and response.get("jsonrpc") == "2.0" and
             type(response.get("id")) is type(expected_request_id) and
             response.get("id") == expected_request_id and "error" not in response,
             "rpc")
    result = response.get("result")
    _require(type(result) is dict and
             ("isError" not in result or result["isError"] is False), "result")
    content = result.get("content")
    observed = method == "handkite_observe"
    _require(type(content) is list and len(content) == (2 if observed else 1),
             "content")
    text = content[0]
    _require(type(text) is dict and text.get("type") == "text" and
             type(text.get("text")) is str and len(text["text"]) <= MAX_RESPONSE,
             "content")
    try:
        _require(len(text["text"].encode("utf-8")) <= MAX_RESPONSE, "payload")
        payload = json.loads(text["text"], object_pairs_hook=_pairs,
                             parse_constant=lambda _: _require(False, "payload"))
    except (ValueError, UnicodeError, RecursionError):
        raise ValueError("mcp_observation_payload") from None
    _require(type(payload) is dict, "payload")
    metadata = payload.get("metadata") if observed else payload
    _require(type(metadata) is dict and metadata.get("device") == device_key,
             "device")
    state = metadata.get("state")
    _require(type(state) is dict and state.get("wda_ready") is True and
             type(state.get("lock")) is str and
             state["lock"] in ("locked", "unlocked", "uncertain"), "state")
    tree_chars = image_bytes = 0
    if observed:
        try:
            tree_chars = _tree_chars(payload)
        except (UnicodeError, RecursionError):
            raise ValueError("mcp_observation_tree") from None
        image = content[1]
        _require(type(image) is dict and image.get("type") == "image" and
                 image.get("mimeType") in ("image/png", "image/jpeg") and
                 type(image.get("data")) is str and
                 0 < len(image["data"]) <= MAX_RESPONSE, "image")
        try:
            decoded = base64.b64decode(image["data"], validate=True)
        except (ValueError, binascii.Error):
            raise ValueError("mcp_observation_image") from None
        _require(bool(decoded) and
                 base64.b64encode(decoded).decode("ascii") == image["data"], "image")
        image_bytes = len(decoded)
    return {"wda_ready": True, "lock": state["lock"],
            "tree_chars": tree_chars, "image_bytes": image_bytes}
