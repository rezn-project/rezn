"""Ed25519 envelopes for the supported ASCII/integer pod subset (OpenSSL 3)."""
import base64
import json
import subprocess
from pathlib import Path


def command(*args):
    return subprocess.run(args, check=True, capture_output=True, timeout=15).stdout


def generate_key(directory):
    key = Path(directory) / "signing.pem"
    command("openssl", "genpkey", "-algorithm", "ED25519", "-out", str(key))
    key.chmod(0o600)
    return key


def sign(program, key):
    # This subset has ASCII strings and integers: sorted compact JSON equals JCS.
    # General Unicode/floating-point programs require a full JCS implementation.
    message = json.dumps(program, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    message.encode("ascii")
    message_path = key.parent / "message.json"
    signature_path = key.parent / "signature.bin"
    message_path.write_text(message, encoding="ascii")
    command("openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(key),
            "-in", str(message_path), "-out", str(signature_path))
    public = command("openssl", "pkey", "-in", str(key), "-pubout", "-outform", "DER")
    assert public[:12] == bytes.fromhex("302a300506032b6570032100")
    return {"program": program, "signature": {"algorithm": "ed25519",
            "pub": base64.b64encode(public[12:]).decode(),
            "sig": base64.b64encode(signature_path.read_bytes()).decode()}}


if __name__ == "__main__":
    import argparse
    import tempfile
    parser = argparse.ArgumentParser(description="Sign a supported ASCII pod program with an isolated development key")
    parser.add_argument("program", type=Path, help="JSON array of pod instructions")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="rezn-sign-") as temporary:
        print(json.dumps(sign(json.loads(args.program.read_text()), generate_key(temporary))))
