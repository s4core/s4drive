#!/usr/bin/env python3
"""Minimal S3 client for the end-to-end tests (standard library only).

The tests need a fresh bucket, a local copy of its .s4drive/ prefix and a
cleanup afterwards. The s4drive CLI does none of that, and curl's SigV4
support differs between versions, so requests are signed here.

Usage:
  e2e_s3.py mb BUCKET                    create a bucket
  e2e_s3.py rb BUCKET                    delete every object, then the bucket
  e2e_s3.py ls BUCKET PREFIX             print the keys under PREFIX
  e2e_s3.py get-prefix BUCKET PREFIX DIR download the keys under PREFIX into DIR

Environment: S4_E2E_ENDPOINT, S4_E2E_ACCESS_KEY, S4_E2E_SECRET_KEY,
S4_E2E_REGION (default us-east-1). Buckets are addressed path-style.
"""

import datetime
import hashlib
import hmac
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

S3_NS = "{http://s3.amazonaws.com/doc/2006-03-01/}"


def env(name, default=None):
    value = os.environ.get(name, default)
    if value is None:
        sys.exit(f"e2e_s3.py: {name} is not set")
    return value


ENDPOINT = env("S4_E2E_ENDPOINT").rstrip("/")
ACCESS_KEY = env("S4_E2E_ACCESS_KEY")
SECRET_KEY = env("S4_E2E_SECRET_KEY")
REGION = env("S4_E2E_REGION", "us-east-1")


def quote(value):
    return urllib.parse.quote(value, safe="-_.~")


def sign(key, message):
    return hmac.new(key, message.encode(), hashlib.sha256).digest()


def request(method, bucket, key="", query=None, body=b""):
    """Send one SigV4-signed request and return the response body."""
    # Each segment is encoded on its own; a trailing "/" (folder marker) stays.
    path = "/" + quote(bucket) + ("/" + "/".join(map(quote, key.split("/"))) if key else "")
    query_string = "&".join(
        f"{quote(k)}={quote(v)}" for k, v in sorted((query or {}).items())
    )
    host = urllib.parse.urlparse(ENDPOINT).netloc
    now = datetime.datetime.now(datetime.timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date = now.strftime("%Y%m%d")
    payload_hash = hashlib.sha256(body).hexdigest()

    headers = {"host": host, "x-amz-content-sha256": payload_hash, "x-amz-date": amz_date}
    signed_headers = ";".join(sorted(headers))
    canonical_request = "\n".join([
        method,
        path,
        query_string,
        "".join(f"{name}:{headers[name]}\n" for name in sorted(headers)),
        signed_headers,
        payload_hash,
    ])
    scope = f"{date}/{REGION}/s3/aws4_request"
    string_to_sign = "\n".join([
        "AWS4-HMAC-SHA256",
        amz_date,
        scope,
        hashlib.sha256(canonical_request.encode()).hexdigest(),
    ])
    signing_key = sign(sign(sign(sign(f"AWS4{SECRET_KEY}".encode(), date), REGION), "s3"), "aws4_request")
    signature = hmac.new(signing_key, string_to_sign.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{scope}, "
        f"SignedHeaders={signed_headers}, Signature={signature}"
    )
    del headers["host"]  # urllib sets it from the URL

    url = ENDPOINT + path + (f"?{query_string}" if query_string else "")
    req = urllib.request.Request(url, data=body or None, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode(errors="replace")
        sys.exit(f"e2e_s3.py: {method} {path} failed with {error.code}: {detail}")


def list_keys(bucket, prefix):
    keys, token = [], None
    while True:
        query = {"list-type": "2", "prefix": prefix}
        if token:
            query["continuation-token"] = token
        root = ET.fromstring(request("GET", bucket, query=query))
        keys += [item.findtext(f"{S3_NS}Key") for item in root.iter(f"{S3_NS}Contents")]
        if root.findtext(f"{S3_NS}IsTruncated") != "true":
            return keys
        token = root.findtext(f"{S3_NS}NextContinuationToken")


def get_prefix(bucket, prefix, dest):
    for key in list_keys(bucket, prefix):
        if key.endswith("/"):
            continue  # folder marker such as .s4drive/system/locks/
        target = os.path.join(dest, key[len(prefix):])
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as file:
            file.write(request("GET", bucket, key))


def remove_bucket(bucket):
    for key in list_keys(bucket, ""):
        request("DELETE", bucket, key)
    request("DELETE", bucket)


def main(args):
    if len(args) == 2 and args[0] == "mb":
        request("PUT", args[1])
    elif len(args) == 2 and args[0] == "rb":
        remove_bucket(args[1])
    elif len(args) == 3 and args[0] == "ls":
        print("\n".join(list_keys(args[1], args[2])))
    elif len(args) == 4 and args[0] == "get-prefix":
        get_prefix(args[1], args[2], args[3])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
