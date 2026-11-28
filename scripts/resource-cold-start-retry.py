#!/usr/bin/env python3
"""Exit successfully when cold-start results qualify for one latency retry."""

import argparse
import json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=argparse.FileType("r", encoding="utf-8"))
    with parser.parse_args().results as stream:
        data = json.load(stream)

    runs = data.get("runs", [])
    summaries = [item for item in data.get("summaries", [])
                 if item.get("gate", {}).get("gatesExit")]
    only_latency = (data.get("status") == "FAIL" and "fatalError" not in data
                    and runs and summaries
                    and all(run.get("status") == "PASS" and not run.get("errors")
                            for run in runs)
                    and all(item["gate"].get("complete") is True for item in summaries)
                    and all(item.get("metrics", {}).get(key) is not None
                            for item in summaries
                            for key in ("readyMs", "getMessages40Ms", "getStateMs"))
                    and any(item["gate"].get("status") == "FAIL" for item in summaries))
    return 0 if only_latency else 1


if __name__ == "__main__":
    raise SystemExit(main())
