#!/usr/bin/env python3
"""Offline, sequential resource comparison (0 PASS, 1 FAIL/REGRESSION, 2 usage, 77 SKIP).

Source declarations are NOT build provenance. Private commit (Windows) / RSS (Linux)
are primary; OS high-water marks are retained separately. CPU is single-core.
"""
import argparse
import importlib.util
import json
import math
from pathlib import Path
import queue
import re
import statistics
import subprocess
import sys
import time
import uuid
from types import SimpleNamespace


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


smoke = load_module("comparison_resource_smoke", Path(__file__).with_name("resource-smoke.py"))
MIB = smoke.MIB
Problem, require = smoke.Problem, smoke.require


def median_range(values):
    """Never silently treat missing/nonfinite measurements as zero."""
    if not values or any(type(v) not in (int, float) or not math.isfinite(v) for v in values):
        return None
    return {"median": statistics.median(values), "min": min(values), "max": max(values), "n": len(values)}


def excess(actual, reference, absolute, percent):
    if any(type(v) not in (int, float) or not math.isfinite(v) for v in (actual, reference)):
        return None
    difference = actual - reference
    # A declared 1 MiB floor avoids infinite/meaningless ratios for tiny growth.
    percentage = 100 * difference / max(abs(reference), MIB)
    return {"deltaBytes": difference, "percent": percentage,
            "ratioDenominatorBytes": max(abs(reference), MIB),
            "regression": difference > absolute and percentage > percent}


def round_values(run):
    return {s["round"]: s["sample"]["primary_bytes"] for s in run.get("samples", [])
            if s.get("phase") in ("idle", "post-idle") and type(s.get("round")) is int
            and s["round"] <= run.get("roundsCompleted", 0)
            and isinstance(s.get("sample"), dict) and "primary_bytes" in s["sample"]}


def first_divergence(reference, candidate, absolute, percent):
    """Only compare matching completed rounds present in EVERY paired repeat."""
    if not reference or len(reference) != len(candidate):
        return None
    histories = [round_values(r) for r in reference + candidate]
    common = set(histories[0]).intersection(*(set(h) for h in histories[1:]))
    n = len(reference)
    for number in sorted(common):
        ref = median_range([h[number] for h in histories[:n]])
        cur = median_range([h[number] for h in histories[n:]])
        if ref is None or cur is None:
            continue
        comparison = excess(cur["median"], ref["median"], absolute, percent)
        if comparison["regression"]:
            return dict(round=number, reference=ref, candidate=cur, **comparison)
    return None


def normalize_control(frame, ready):
    """Legacy AgentEnd is completion notification, NOT proof of semantic success.

    Native settlement takes precedence when advertised. Legacy success is verified
    afterwards using get_state and the actual persisted message pair.
    """
    if frame.get("type") == "agent_end":
        semantics = ready.get("semantics", {})
        if semantics.get("settlementEvent") == "agent_settled" or semantics.get("agentEndIsTerminal") is False:
            return None
        return {"type": "agent_settled", "outcome": "legacy-notification", "legacy": True}
    return frame


class CompatibleHost(smoke.Host):
    def pump(self, pipe, name):
        try:
            while True:
                line = pipe.readline(smoke.MAX_LINE + 1)
                if not line:
                    break
                require(len(line) <= smoke.MAX_LINE, "oversize process line")
                line = line.decode("utf-8", errors="strict").rstrip("\r\n")
                self.record(name, line)
                if name != "stdout" or not line:
                    continue
                frame = json.loads(line)
                require(isinstance(frame, dict), "non-object frame")
                if frame.get("type") == "ready":
                    self.ready_frame = frame
                frame = normalize_control(frame, getattr(self, "ready_frame", {}))
                if frame and frame.get("type") in {"ready", "response", "agent_settled", "prepared",
                        "round_complete", "reset_complete", "stopped"}:
                    try:
                        self.queue.put_nowait(frame)
                    except queue.Full:
                        raise Problem("protocol", "control event retention limit exceeded")
        except Exception as error:
            if self.error is None:
                self.error = error

    def completion(self):
        frame = self.__dict__.pop("settled", None)
        if frame is None:
            frame = self.wait(lambda f: f.get("type") == "agent_settled")
        require(frame.get("outcome") in ("completed", "legacy-notification"),
                "run not completed: " + str(frame), "semantic")
        require(not frame.get("aborted") and not frame.get("error") and not frame.get("pendingResume"),
                "aborted/error/pending completion", "semantic")
        return frame


def validate_messages(data, prompt, number, final, seen, require_ids=False):
    require(isinstance(data, dict) and isinstance(data.get("messages"), list), "missing messages", "semantic")
    require(data.get("pendingResume") in (None, False), "pending resume", "semantic")
    messages = data["messages"][-2:]
    require(len(messages) == 2 and all(isinstance(m, dict) for m in messages), "missing message pair", "semantic")
    user, assistant = messages
    require(user.get("role", user.get("kind")) == "user" and smoke.text(user.get("content")) == prompt,
            "current user prompt mismatch", "semantic")
    require(assistant.get("role", assistant.get("kind")) == "assistant", "missing assistant", "semantic")
    for key, value in (("provider", "resource-fixture"), ("model", "synthetic"), ("api", "openai-completions")):
        if key in assistant:
            require(assistant[key] == value, "assistant identity mismatch: " + key, "semantic")
    require(assistant.get("stopReason") == "stop" and not assistant.get("errorMessage"),
            "abnormal/truncated assistant stop", "semantic")
    blocks = assistant.get("content")
    require(isinstance(blocks, (str, list)), "invalid assistant content", "semantic")
    if isinstance(blocks, list):
        require(blocks and all(isinstance(b, dict) and b.get("type") == "text" and
                              isinstance(b.get("text"), str) for b in blocks), "non-text assistant", "semantic")
    try:
        answer = json.loads(smoke.text(blocks))
    except (ValueError, TypeError):
        raise Problem("semantic", "invalid final JSON") from None
    require(answer == smoke.expected(number, final) and type(answer.get("round")) is int and
            type(answer.get("sum")) is int and type(answer.get("final")) is bool,
            "round/arithmetic/final marker mismatch", "semantic")
    ids = [m.get("entryId") for m in messages]
    if require_ids or any(i is not None for i in ids):
        require(all(isinstance(i, str) and i for i in ids) and len(set(ids)) == 2 and not set(ids) & seen,
                "stale/missing message identity", "semantic")
        seen.update(ids)
    return {"answer": answer, "entryIds": ids, "identityAvailable": all(i is not None for i in ids),
            "assistantIdentity": {k: assistant.get(k) for k in ("provider", "model", "api")},
            "usage": assistant.get("usage")}


def fixture_args():
    return SimpleNamespace(live=False, model=None, max_output_tokens=256)


def check_safety(peak, args):
    if peak > args.max_memory_mib * MIB:
        raise Problem("safety_ceiling", peak / MIB, args.max_memory_mib)


def run_case(binary, probe, root, args, kind, emit):
    result = {"case": kind, "status": "RUNNING", "roundsCompleted": 0, "samples": [],
              "baseline": None, "final": None, "peakPrimaryBytes": 0, "firstError": None,
              "finalConclusion": {"status": "NOT_COMPLETED" if kind == "rpc" else "NOT_APPLICABLE"}}
    if kind == "editor" and probe is None:
        return dict(result, status="SKIP", reason="no editor probe supplied")
    host = sampler = provider = None
    phase, number = "startup", None
    safety_error = None

    def safety_sample(pid):
        nonlocal safety_error
        value = smoke.sample(pid)
        try:
            check_safety(value["primary_bytes"], args)
        except Problem as error:
            # Abort the owned PID promptly even if the main thread is waiting for
            # a protocol response. Preserve the triggering metric and causal error.
            safety_error = error
            result["peakPrimaryBytes"] = max(result["peakPrimaryBytes"], value["primary_bytes"])
            result["final"] = value
            event = {"phase": "safety-abort", "round": number, "sample": value}
            result["samples"].append(event)
            try:
                emit(event)
            finally:
                if host.proc.poll() is None:
                    host.proc.terminate()
            raise
        return value

    def record(phase, number, sample, before=None):
        smoke.validate_sample(sample)
        result["peakPrimaryBytes"] = max(result["peakPrimaryBytes"], sampler.peak, sample["primary_bytes"])
        value = {"phase": phase, "round": number, "sample": sample,
                 "peakPrimaryBytes": result["peakPrimaryBytes"]}
        if before is not None:
            value["delta"] = smoke.delta(before, sample)
        result["samples"].append(value)
        result["final"] = sample
        emit(value)
        check_safety(result["peakPrimaryBytes"], args)
        return sample

    def fail(error, at):
        error = safety_error or error
        if result["firstError"] is None:
            result["firstError"] = {"category": getattr(error, "category", "runtime"),
                "phase": at, "round": number, "message": str(error), "budget": getattr(error, "budget", None)}
            result["status"] = "SAFETY_ABORT" if getattr(error, "category", None) == "safety_ceiling" else "FAIL"

    try:
        if kind == "rpc":
            provider = smoke.Provider(args.rounds)
        base = f"http://127.0.0.1:{provider.server.server_port}/v1" if provider else "http://127.0.0.1:1/v1"
        env = smoke.environment(root, fixture_args(), base)
        if kind == "tui":
            path = Path(__file__).with_name("resource_tui.py")
            if not path.is_file():
                return dict(result, status="SKIP", reason="optional resource_tui.py unavailable")
            module = load_module("comparison_resource_tui", path)
            tui = module.run_tui(binary, root, env, args, emit)
            if tui.get("category") in ("safety", "safety_ceiling"):
                tui["status"] = "SAFETY_ABORT"
                tui.setdefault("firstError", {"category": "safety_ceiling", "message": tui.get("reason")})
            return tui
        command = [str(probe)] if kind == "editor" else [str(binary), "--mode", "rpc",
            "--provider", "resource-fixture", "--model", "synthetic", "--session-dir", str(root / "sessions"),
            "--session-id", str(uuid.uuid4()), "--rpc-message-updates", "snapshot", "--no-extensions",
            "--no-skills", "--no-context-files", "--no-prompt-templates", "--no-tools", "--thinking", "off",
            "--local-only", "--timeout", str(args.request_timeout)]
        # v0.16.0 args.rs supports snapshot; never use newer compact transport in a comparison.
        host = CompatibleHost(command, root, env, args.request_timeout)
        sampler = smoke.Sampler(host.proc.pid, args.sample_interval, reader=safety_sample)
        record("startup", None, sampler.take())
        ready = host.wait(lambda f: f.get("type") == "ready")
        result["ready"] = ready
        if kind == "editor":
            require(ready.get("ok") is True, "editor not ready")
        else:
            require(ready.get("semantics", {}).get("messageUpdates", "snapshot") == "snapshot",
                    "non-comparable RPC encoding")
        before = record("ready", None, sampler.take())
        phase = "initial-idle"
        time.sleep(args.startup_seconds)
        result["baseline"] = record(phase, 0, sampler.take(), before)
        result["initialIdle"] = smoke.delta(before, result["baseline"])
        if kind == "editor":
            phase = "prepare"
            host.send({"type": "prepare", "pasteBytes": smoke.PASTE_BYTES, "edits": smoke.EDITS})
            frame = host.wait(lambda f: f.get("type") == "prepared")
            require(frame.get("ok") is True and frame.get("pasteBytes") == smoke.PASTE_BYTES and
                    frame.get("edits") == smoke.EDITS, "editor prepare mismatch")
            record(phase, None, sampler.take())
        seen = set()
        for number in range(1, args.rounds + 1):
            phase = "pre"
            before = record(phase, number, sampler.take())
            phase = "round"
            if kind == "editor":
                host.send({"type": "round", "round": number})
                frame = host.wait(lambda f: f.get("type") == "round_complete")
                require(frame.get("ok") is True and frame.get("round") == number and
                        frame.get("pasteBytes") == smoke.PASTE_BYTES and frame.get("edits") == smoke.EDITS,
                        "editor round mismatch")
            else:
                prompt = smoke.prompt_for(number, number == args.rounds)
                accepted = host.call("prompt", message=prompt)
                require(accepted in (None, {"accepted": True}), "prompt rejected")
                completion = host.completion()
                state = host.call("get_state")
                require(isinstance(state, dict), "invalid state")
                for key in ("isStreaming", "isRunning", "pendingResume"):
                    require(state.get(key) in (None, False), "unfinished state: " + key, "semantic")
                data = host.call("get_messages", limit=2)
                answer = validate_messages(data, prompt, number, number == args.rounds, seen,
                                           require_ids=ready.get("semantics", {}).get("messageIds") == "sessionEntryId-v1")
                emit(dict(event="completed-answer", round=number, completion=completion,
                          state=state, **answer))
                if number == args.rounds:
                    result["finalConclusion"] = dict(status="PASS", round=number, **answer)
            after = record("post", number, sampler.take(), before)
            result["roundsCompleted"] = number
            phase = "idle"
            time.sleep(args.idle_seconds)
            record(phase, number, sampler.take(), after)
        phase, number = "stop", None
        result["final"] = record("final", None, sampler.take())
        sampler.close()
        sampler = None
        if kind == "editor":
            host.send({"type": "reset"})
            require(host.wait(lambda f: f.get("type") == "reset_complete").get("ok") is True, "reset failed")
            host.send({"type": "stop"})
            require(host.wait(lambda f: f.get("type") == "stopped").get("ok") is True, "stop failed")
        else:
            host.call("stop")
        host.exit_ok()
        if provider:
            require(provider.calls == args.rounds, "fixture request count mismatch", "provider")
        result["status"] = "PASS"
    except Exception as error:
        fail(error, phase)
    finally:
        for owned in (sampler, host, provider):
            if owned is not None:
                try:
                    owned.close()
                except Exception as error:
                    fail(error, "cleanup")
    if result["status"] != "PASS" and kind == "rpc":
        result["finalConclusion"]["status"] = "NOT_COMPLETED" if result["finalConclusion"].get("answer") is None else "VERIFIED_BEFORE_FAILURE"
    return result


def summarize_runs(runs):
    fields = {"baselineBytes": [], "peakPrimaryBytes": [], "finalPrimaryBytes": [], "growthBytes": [],
              "cpuSeconds": [], "idleCpuPercent": [], "osPeakRssBytes": [], "osPeakCommitBytes": []}
    for run in runs:
        baseline, final = run.get("baseline"), run.get("final")
        if not isinstance(baseline, dict) or not isinstance(final, dict):
            continue
        for key, value in (("baselineBytes", baseline.get("primary_bytes")),
                ("peakPrimaryBytes", run.get("peakPrimaryBytes")), ("finalPrimaryBytes", final.get("primary_bytes")),
                ("osPeakRssBytes", final.get("peak_rss_bytes")), ("osPeakCommitBytes", final.get("peak_commit_bytes"))):
            fields[key].append(value)
        try:
            change = smoke.delta(baseline, final)
            fields["growthBytes"].append(change["memory_delta_bytes"])
            fields["cpuSeconds"].append(change["cpu_seconds"])
            idle = [s["delta"]["cpu_percent"] for s in run.get("samples", [])
                    if s.get("phase") in ("idle", "post-idle") and isinstance(s.get("delta"), dict)]
            fields["idleCpuPercent"].append(statistics.median(idle) if idle else None)
        except (Problem, KeyError, TypeError):
            pass
    return {key: median_range(values) if len(values) == len(runs) else None for key, values in fields.items()}


def compare(data, args):
    comparisons = []
    latest = args.variant[-1][0]
    for kind in data["cases"]:
        reference = [r for r in data["runs"] if r["label"] == args.reference and r["case"] == kind]
        for label, _, _ in args.variant:
            if label == args.reference:
                continue
            candidate = [r for r in data["runs"] if r["label"] == label and r["case"] == kind]
            complete = (len(reference) == len(candidate) == args.repeats and
                        all(r["status"] == "PASS" and r.get("roundsCompleted") == args.rounds for r in reference + candidate))
            ref, cur = summarize_runs(reference), summarize_runs(candidate)
            metrics = {}
            for key in ("baselineBytes", "peakPrimaryBytes", "finalPrimaryBytes", "growthBytes"):
                metrics[key] = excess(cur[key]["median"], ref[key]["median"], args.max_regression_mib * MIB,
                                      args.max_regression_percent) if cur[key] and ref[key] else None
            divergent = first_divergence(reference, candidate, args.max_regression_mib * MIB,
                                         args.max_regression_percent)
            regression = any(v and v["regression"] for k, v in metrics.items() if k != "baselineBytes")
            comparisons.append({"case": kind, "reference": args.reference, "candidate": label,
                "gatesExit": label == latest, "status": "INCOMPLETE" if not complete else "REGRESSION" if regression else "PASS",
                "referenceStats": ref, "candidateStats": cur, "metrics": metrics,
                "firstDivergentCompletedRound": divergent})
    return comparisons


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", nargs=3, action="append", required=True, metavar=("LABEL", "BINARY", "PROBE"))
    parser.add_argument("--source", nargs=2, action="append", default=[], metavar=("LABEL", "COMMIT"))
    parser.add_argument("--reference")
    parser.add_argument("--diagnostics-dir", type=Path, required=True)
    parser.add_argument("--case", choices=("all", "rpc", "editor", "tui"), default="all")
    for name, default, typ in (("rounds", 20, int), ("repeats", 3, int), ("idle-seconds", .2, float),
            ("startup-seconds", 1, float), ("sample-interval", .05, float), ("request-timeout", 25, int),
            ("max-memory-mib", 512, float), ("max-regression-mib", 32, float), ("max-regression-percent", 25, float)):
        parser.add_argument("--" + name, type=typ, default=default)
    args = parser.parse_args(argv)
    for name in ("rounds", "repeats", "idle_seconds", "startup_seconds", "sample_interval", "request_timeout",
                 "max_memory_mib", "max_regression_mib", "max_regression_percent"):
        if not math.isfinite(getattr(args, name)) or getattr(args, name) <= 0:
            parser.error("--" + name.replace("_", "-") + " must be finite and positive")
    if args.rounds > 1000 or args.repeats > 10 or args.startup_seconds < 1:
        parser.error("rounds <= 1000, repeats <= 10, startup-seconds >= 1 required")
    if not 2 <= len(args.variant) <= 3:
        parser.error("supply two or three variants")
    labels = [v[0] for v in args.variant]
    if len(set(labels)) != len(labels) or any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", l) for l in labels):
        parser.error("variant labels must be unique safe path components")
    args.reference = args.reference or labels[0]
    if args.reference not in labels:
        parser.error("unknown reference label")
    if len({s[0] for s in args.source}) != len(args.source) or any(s[0] not in labels for s in args.source):
        parser.error("source labels must be unique and name variants")
    variants = []
    for label, binary, probe in args.variant:
        paths = [Path(binary).resolve(), None if probe == "-" else Path(probe).resolve()]
        if any(p is not None and not p.is_file() for p in paths):
            parser.error("binary/probe must be existing files (use '-' for absent probe)")
        variants.append((label, *paths))
    args.variant = variants
    args.diagnostics_dir = args.diagnostics_dir.absolute()
    if args.diagnostics_dir.exists():
        parser.error("diagnostics directory exists; refusing overwrite")
    return args


def version_metadata(binary, probe, root, timeout):
    env = smoke.environment(root, fixture_args(), "http://127.0.0.1:1/v1")
    # Host bounds stdout/stderr and owns only its child, unlike unbounded capture_output.
    host = None
    failure = None
    result = {"binary": str(binary), "binarySha256": smoke.sha256_file(binary),
              "probe": str(probe) if probe else None, "probeSha256": smoke.sha256_file(probe) if probe else None}
    try:
        host = smoke.Host([str(binary), "--version"], root, env, timeout, raw=True)
        frame = host.wait(lambda f: f.get("type") == "version")
        result["version"] = frame["text"]
        host.exit_ok()
    except Exception as error:
        failure = error
    finally:
        if host:
            try:
                host.close()
            except Exception as error:
                failure = failure or error
    if failure:
        result["versionError"] = str(failure)
    return result


def markdown(data):
    lines = ["# Offline resource comparison", "", "**" + data["status"] + "**", "",
        "Primary: Windows PrivateUsage / Linux RSS. CPU: single-core, not divided by CPU count.",
        "OS peaks are separate from sampled primary peaks. Median [min, max] across repeats.",
        "Regression requires BOTH absolute and percentage thresholds; percentage denominator floor is 1 MiB.",
        "Only the LAST declared variant versus the reference gates regression exit status.",
        "No provider/model overrides, no external provider; loopback synthetic streaming arithmetic only.",
        "Initial idle baseline precedes all work; no request warmup hides startup growth.", "",
        "| Variant | Case | Status | Baseline MiB | Peak MiB | Growth MiB | CPU s | Idle CPU % |",
        "|---|---|---|---:|---:|---:|---:|---:|"]
    def fmt(value, divisor=1):
        return "missing" if value is None else f'{value["median"]/divisor:.2f} [{value["min"]/divisor:.2f}, {value["max"]/divisor:.2f}]'
    for label in data["variants"]:
        for kind in data["cases"]:
            runs = [r for r in data["runs"] if r["label"] == label and r["case"] == kind]
            stats = summarize_runs(runs)
            status = ", ".join(sorted({r["status"] for r in runs})) or "missing"
            cells = [label, kind, status] + [fmt(stats[k], MIB if k.endswith("Bytes") else 1) for k in
                        ("baselineBytes", "peakPrimaryBytes", "growthBytes", "cpuSeconds", "idleCpuPercent")]
            lines.append("| " + " | ".join(cells) + " |")
    lines += ["", "## Comparisons"]
    for comparison in data["comparisons"]:
        first = comparison["firstDivergentCompletedRound"]
        lines.append(f'- {comparison["case"]}: {comparison["candidate"]} vs {comparison["reference"]}: '
                     f'{comparison["status"]}; first divergent common completed round: {first["round"] if first else "none observed"}.')
    lines += ["", "## Identity and limitations"]
    for label, identity in data["variants"].items():
        lines.append(f'- {label}: version `{identity.get("version", "unavailable")}`; SHA256 `{identity["binarySha256"]}`; '
                     f'declared source `{identity.get("declaredSource", "unspecified")}` (NOT build proof).')
    lines += ["", "Empty histories and a small synthetic workload do not disprove user-observed growth, long histories, or UI workloads.",
              "SKIP is unsupported/unavailable, never PASS. SAFETY_ABORT is a stopped measurement, never PASS.",
              "Partial runs retain their first error and original measurements; incomplete comparisons cannot establish absence of regression.",
              "See results.json and each run's metrics.jsonl/process.jsonl for raw metrics and semantic validation."]
    return "\n".join(lines) + "\n"


def exit_status(data):
    if any(r["status"] in ("FAIL", "SAFETY_ABORT") for r in data["runs"]) or any(
            c["gatesExit"] and c["status"] == "REGRESSION" for c in data["comparisons"]):
        return 1
    return 0 if any(r["status"] == "PASS" for r in data["runs"]) else 77


def main(argv=None):
    args = parse_args(argv)
    root = args.diagnostics_dir
    root.mkdir(parents=True, exist_ok=False)  # Race-safe no-overwrite guarantee.
    data = {"status": "RUNNING", "platform": sys.platform, "offline": True,
            "reference": args.reference, "latest": args.variant[-1][0], "variants": {}, "runs": [],
            "cases": ["rpc", "editor", "tui"] if args.case == "all" else [args.case], "comparisons": [],
            "configuration": {k: getattr(args, k) for k in ("rounds", "repeats", "idle_seconds", "startup_seconds",
                "sample_interval", "max_memory_mib", "max_regression_mib", "max_regression_percent")},
            "notes": "Declared source is not binary build proof; original metric units retained. Single-core CPU."}
    def save():
        smoke.dump(root / "results.json", data)
        (root / "summary.md").write_text(markdown(data), encoding="utf-8")
    save()
    if sys.platform != "win32" and not sys.platform.startswith("linux"):
        data.update(status="SKIP", reason="resource metrics supported only on Windows/Linux")
        save()
        return 77
    for label, binary, probe in args.variant:
        identity_root = root / (label + "-identity")
        identity_root.mkdir()
        identity = version_metadata(binary, probe, identity_root, args.request_timeout)
        identity["declaredSource"] = dict(args.source).get(label)
        identity["sourceScope"] = "user declaration, not build proof"
        data["variants"][label] = identity
        save()
    # Repeat -> case -> variant: identical work interleaved, never overlapping hosts.
    for repeat in range(1, args.repeats + 1):
        for kind in data["cases"]:
            for label, binary, probe in args.variant:
                run_root = root / f"repeat-{repeat}-{kind}-{label}"
                run_root.mkdir()
                count = 0
                limit = 12 * args.rounds + 512
                def emit(value):
                    nonlocal count
                    require(count < limit, "event retention safety limit", "diagnostics")
                    encoded = json.dumps(value, ensure_ascii=False)
                    require(len(encoded.encode("utf-8")) <= smoke.MAX_LINE, "oversize diagnostic event", "diagnostics")
                    with (run_root / "metrics.jsonl").open("a", encoding="utf-8") as file:
                        file.write(encoded + "\n")
                    count += 1
                result = None
                try:
                    result = run_case(binary, probe, run_root, args, kind, emit)
                    require(result.get("status") in ("PASS", "FAIL", "SAFETY_ABORT", "SKIP"), "invalid case status")
                    if result["status"] == "PASS":
                        require(result.get("roundsCompleted") == args.rounds and result.get("baseline") and result.get("final"),
                                "incomplete case reported PASS", "semantic")
                        smoke.validate_sample(result["baseline"])
                        smoke.validate_sample(result["final"], result["baseline"])
                        check_safety(result.get("peakPrimaryBytes", math.inf), args)
                except Exception as error:
                    if not isinstance(result, dict):
                        result = {"roundsCompleted": 0}
                    result["status"] = "SAFETY_ABORT" if getattr(error, "category", None) == "safety_ceiling" else "FAIL"
                    if not result.get("firstError"):
                        result["firstError"] = {"category": getattr(error, "category", "runtime"), "message": str(error)}
                result.update(label=label, case=kind, repeat=repeat, diagnostics=str(run_root))
                data["runs"].append(result)
                save()
    data["comparisons"] = compare(data, args)
    code = exit_status(data)
    data["status"] = ("FAIL" if any(r["status"] in ("FAIL", "SAFETY_ABORT") for r in data["runs"]) else
                      "REGRESSION") if code == 1 else "SKIP" if code == 77 else "PASS"
    save()
    print(data["status"] + ": " + str(root / "summary.md"))
    return code


if __name__ == "__main__":
    sys.exit(main())
