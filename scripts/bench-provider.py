#!/usr/bin/env python3
"""Run the identical offline fixture on each revision; emit JSONL measurements."""
import argparse
import json
import os
import platform
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--features", default="provider-performance")
parser.add_argument("--repeats", type=int, default=3)
parser.add_argument("--quick", action="store_true")
args = parser.parse_args()
assert args.repeats > 0
build = subprocess.run([
    "cargo", "test", "-p", "rpi-extensions", "--features", args.features,
    "--release", "--locked", "--no-run", "--message-format=json",
], check=True, text=True, stdout=subprocess.PIPE)
executables = [item["executable"] for line in build.stdout.splitlines()
               if (item := json.loads(line)).get("executable")
               and item.get("target", {}).get("kind") == ["lib"]]
assert len(executables) == 1, executables
print(json.dumps({"environment": platform.platform(), "cpu": platform.processor(),
                  "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
                  "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
                  "features": args.features}), flush=True)
# Representative samples; avoid multiplying every dimension together.
cases = [(4096, 64, 1, 0, 0, 0), (32768, 64, 1, 0, 0, 0),
         (32768, 64, 1, 0, 0, 1)]
if not args.quick:
    cases += [(size, chunk, 1, 0, 0, 0) for size in [4096, 32768, 131072] for chunk in [64, 1024]]
    cases += [(4096, 1, 1, 0, 0, 0), (32768, 1024, 8, 0, 0, 0),
              (32768, 1024, 32, 0, 0, 0), (32768, 1024, 1, 1, 0, 0),
              (32768, 1024, 8, 1, 0, 0), (32768, 64, 1, 0, 1, 0)]
names = ["BYTES", "CHUNK", "STREAMS", "DELAY_MS", "RESULT_ONLY", "SDK_ONLY"]
for case in dict.fromkeys(cases):
    for run in range(args.repeats):
        env = dict(os.environ, **{f"RPI_BENCH_{k}": str(v) for k, v in zip(names, case)})
        command = [executables[0], "offline_provider_pipeline", "--ignored", "--nocapture", "--test-threads=1"]
        if platform.system() == "Linux":
            command = ["/usr/bin/time", "-f", 'RPI_OS {"user_s":%U,"system_s":%S,"max_rss_kib":%M}', *command]
        result = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        metrics = next(json.loads(line.split("RPI_BENCH ", 1)[1]) for line in result.stdout.splitlines() if "RPI_BENCH " in line)
        for line in result.stderr.splitlines():
            if line.startswith("RPI_OS "):
                metrics.update(json.loads(line.removeprefix("RPI_OS ")))
        print(json.dumps(dict(run=run, **metrics)), flush=True)
