#!/usr/bin/env python3
import subprocess, time, sys

subprocess.run(["sudo", "docker", "rm", "-f", "s4drive-minio"], capture_output=True)
subprocess.run(["sudo", "rm", "-rf", "/tmp/s4drive-minio-data"])

PASS = "minioadmin"

cid = subprocess.run([
    "sudo", "docker", "run", "-d", "--rm",
    "--name", "s4drive-minio",
    "-p", "9000:9000",
    "-e", "MINIO_ROOT_USER=minioadmin",
    "-e", f"MINIO_ROOT_PASSWORD={PASS}",
    "-v", "/tmp/s4drive-minio-data:/data",
    "minio/minio", "server", "/data"
], capture_output=True, text=True)
print("container id:", cid.stdout.strip()[:20])
if cid.returncode != 0:
    print("FAIL:", cid.stderr); sys.exit(1)
time.sleep(3)
rc = subprocess.run(["sudo", "docker", "ps", "-q", "--filter", "name=s4drive-minio"],
    capture_output=True, text=True)
if not rc.stdout.strip():
    logs = subprocess.run(["sudo", "docker", "logs", "s4drive-minio"], capture_output=True, text=True)
    print("DIED:", logs.stdout[-300:]); sys.exit(1)
env = subprocess.run(["sudo", "docker", "exec", "s4drive-minio", "env"],
    capture_output=True, text=True)
for L in env.stdout.split("\n"):
    if "MINIO_ROOT" in L:
        print("  ", L)
for tag, cmd in [
    ("mc alias", ["sudo", "docker", "exec", "s4drive-minio", "mc", "alias", "set", "local",
     "http://127.0.0.1:9000", "minioadmin", PASS]),
    ("mc mb",    ["sudo", "docker", "exec", "s4drive-minio", "mc", "mb", "local/s4drive-test"]),
]:
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    print(f"  {tag}:", (r.stdout.strip() or r.stderr.strip())[:80])
