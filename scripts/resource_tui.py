"""Actual isolated Windows TUI measurements; optional, already-installed pywinpty only.

run_tui owns precisely the PtyProcess it spawns, never the caller's other processes.
No Enter/submit, RPC, model request, external probe or user terminal manipulation.
"""
import importlib.util
from pathlib import Path
import queue
import re
import select
import socket
import sys
import threading
import time

_SPEC = importlib.util.spec_from_file_location("_resource_tui_metrics", Path(__file__).with_name("resource-smoke.py"))
metrics = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(metrics)

MAX_LOG = 4 * 1024 * 1024
TAIL = 256 * 1024
PASTE_BYTES = 64 * 1024
ANSI = re.compile(r"\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]")


class Unconfirmed(Exception):
    pass


def _pty_class():
    if sys.platform != "win32":
        raise Unconfirmed("Windows ConPTY is required")
    try:
        from winpty import PtyProcess
    except ImportError as error:
        raise Unconfirmed("optional installed pywinpty dependency is unavailable") from error
    class PollingPtyProcess(PtyProcess):
        """Use the native nonblocking reader, avoiding pywinpty's immortal
        blocking socket-bridge thread. spawn/write/terminate remain pywinpty's;
        no global PYWINPTY_BLOCK environment mutation is needed.
        """
        def __init__(self, pty):
            self.pty, self.pid = pty, pty.pid
            self.closed = False
            self.delayafterterminate = self.delayafterclose = .1

        def read(self, size=8192):
            value = self.pty.read(size, blocking=False)
            if not value and self.pty.iseof():
                raise EOFError("ConPTY closed")
            return value

        def close(self, force=False):
            if self.pty.isalive() and not self.terminate(force):
                raise OSError("owned ConPTY child did not terminate")
            self.closed = True

    return PollingPtyProcess


class Terminal:
    """Bounded output, one reader/writer; close unblocks reads before joining."""
    def __init__(self, proc, path):
        self.proc = proc
        self.log = path.open("wb")
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.pending = queue.Queue(maxsize=16)
        self.tail = ""
        self.total = self.saved = self.input_bytes = 0
        self.error = None
        self.query_tail = ""
        self.queries = 0
        self.reader = threading.Thread(target=self._read, name="resource-tui-reader", daemon=True)
        self.writer = threading.Thread(target=self._write, name="resource-tui-writer", daemon=True)
        self.reader.start()
        self.writer.start()

    def _read(self):
        try:
            while not self.stop.is_set():
                # Real pywinpty uses a socket bridge. Poll before its blocking read.
                if hasattr(self.proc, "fileobj"):
                    if not select.select([self.proc.fileobj], [], [], .1)[0]:
                        continue
                text = self.proc.read(8192)
                if not text:
                    self.stop.wait(.01)
                    continue
                data = text.encode("utf-8", errors="replace")
                with self.lock:
                    self.total += len(data)
                    self.tail = (self.tail + text)[-TAIL:]
                    remaining = MAX_LOG - self.saved
                    if remaining > 0:
                        self.log.write(data[:remaining])
                        self.log.flush()
                        self.saved += min(len(data), remaining)
                # Only answer queries actually emitted. Fixed virtual cursor is a
                # startup capability response, NOT an editing completion barrier.
                pending = self.query_tail + text
                for match in re.finditer(r"\x1b\[(6n|5n|18t|c|>c)", pending):
                    replies = {"6n": "\x1b[1;1R", "5n": "\x1b[0n", "18t": "\x1b[8;40;120t",
                               "c": "\x1b[?1;2c", ">c": "\x1b[>0;0;0c"}
                    self.pending.put_nowait((replies[match[1]], None, []))
                    self.queries += 1
                # Retain only an incomplete trailing CSI to avoid duplicate replies.
                self.query_tail = re.search(r"\x1b\[[0-9;>?]*$", pending)
                self.query_tail = self.query_tail[0] if self.query_tail else ""
        except (EOFError, OSError):
            pass
        except Exception as error:
            self.error = str(error)

    def _write(self):
        while not self.stop.is_set():
            try:
                text, done, errors = self.pending.get(timeout=.1)
            except queue.Empty:
                continue
            try:
                # One bounded (64 KiB maximum payload) native write preserves
                # framing; this dedicated writer still has a main-thread timeout.
                for offset in range(0, len(text), PASTE_BYTES + 12):
                    if self.stop.is_set():
                        raise EOFError("terminal closing")
                    chunk = text[offset:offset + PASTE_BYTES + 12]
                    written = self.proc.write(chunk)
                    if written is not None and written != len(chunk.encode("utf-8")):
                        raise OSError("partial ConPTY write")
                    self.input_bytes += len(chunk.encode("utf-8"))
            except Exception as error:
                errors.append(str(error))
            finally:
                if done:
                    done.set()

    def view(self):
        with self.lock:
            return ANSI.sub("", self.tail), self.total

    def mark(self):
        with self.lock:
            self.tail = ""
            return self.total

    def send(self, text, check, timeout=10):
        done, errors = threading.Event(), []
        self.pending.put_nowait((text, done, errors))
        deadline = time.monotonic() + timeout
        while not done.wait(.02):
            check()
            if time.monotonic() > deadline:
                raise Unconfirmed("bounded ConPTY write timed out")
        check()
        if errors:
            raise Unconfirmed("ConPTY write failed: " + errors[0])

    def close(self):
        self.stop.set()
        errors = []
        # Shutdown wakes our socket reader even if it is in read's UTF-8 suffix
        # loop. PtyProcess.close(force=True) terminates ONLY this owned child.
        fileobj = getattr(self.proc, "fileobj", None)
        if fileobj is not None:
            try:
                fileobj.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        try:
            self.proc.close(force=True)
        except Exception as error:
            errors.append(str(error))
            try:
                self.proc.terminate(force=True)
            except Exception as second:
                errors.append(str(second))
        for thread in (self.reader, self.writer, getattr(self.proc, "_thread", None)):
            if thread is not None:
                thread.join(timeout=.2)
                if thread.is_alive() and sys.platform == "win32":
                    # Some ConPTY builds block even with blocking=False. Cancel
                    # only this owned thread's synchronous ReadFile/WriteFile.
                    import ctypes
                    from ctypes import wintypes as w
                    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
                    kernel.OpenThread.argtypes = [w.DWORD, w.BOOL, w.DWORD]
                    kernel.OpenThread.restype = w.HANDLE
                    kernel.CancelSynchronousIo.argtypes = [w.HANDLE]
                    kernel.CancelSynchronousIo.restype = w.BOOL
                    kernel.CloseHandle.argtypes = [w.HANDLE]
                    handle = kernel.OpenThread(1, False, thread.native_id)
                    if handle:
                        try:
                            kernel.CancelSynchronousIo(handle)
                        finally:
                            kernel.CloseHandle(handle)
                thread.join(timeout=3)
                if thread.is_alive():
                    errors.append("thread leaked: " + thread.name)
        # Do not close the log under a leaked reader.
        if not self.reader.is_alive():
            self.log.close()
        return errors


def run_tui(binary: Path, root: Path, env: dict, args, emit) -> dict:
    """Return PASS/FAIL/SKIP; caller supplies new root and isolated offline env.

    Required args: rounds, idle_seconds, max_memory_mib, sample_interval.
    Optional startup_seconds defaults to 2; no dependency installation/builds.
    emit receives phase/round/sample/delta dictionaries (same as samples).
    """
    result = dict(case="tui", status="SKIP", startupStatus="SKIP", pressureStatus="SKIP",
                  baseline=None, final=None, samples=[], peakPrimaryBytes=0,
                  roundsCompleted=0, workload={"largePasteBytesPerRound": PASTE_BYTES,
                  "insertCharactersPerRound": 64, "backspacesPerRound": 64,
                  "pasteFramingBytesPerRound": 12, "submittedMessages": 0,
                  "completedInputBytes": 0, "confirmations": []},
                  coverageLimits=["Only owned TUI PID; excludes ConPTY host and descendant processes.",
                  "Rendered insertions confirm editing, not internal editor state or undo correctness.",
                  "Windows paste heuristics may render leading characters outside the folded paste; input bytes are not asserted equal to placeholder size.",
                  "Backspace redraw is observed, not proof of exact deletion semantics.",
                  "No submission, model calls, 1 MiB paste, or long-running conversation coverage.",
                  "Peak private/CPU are sampled; working-set peak also uses OS high-water metric."],
                  terminalLog=str(root / "terminal.log"))
    proc = terminal = sampler = None
    ceiling = min(float(args.max_memory_mib), 512) * 1024 * 1024
    result["safetyCeilingBytes"] = int(ceiling)
    startup = float(getattr(args, "startup_seconds", 2))

    def check():
        if sampler.error:
            raise sampler.error
        if sampler.peak > ceiling:
            raise metrics.Problem("safety", sampler.peak, ceiling)
        if not proc.isalive():
            raise metrics.Problem("process", "TUI exited before intentional cleanup")
        if terminal.error:
            raise metrics.Problem("terminal", terminal.error)

    def wait_for(predicate, seconds, reason):
        deadline = time.monotonic() + seconds
        while True:
            check()
            if predicate():
                return
            if time.monotonic() >= deadline:
                raise Unconfirmed(reason)
            time.sleep(min(.02, max(.001, deadline - time.monotonic())))

    def idle():
        deadline = time.monotonic() + float(args.idle_seconds)
        while time.monotonic() < deadline:
            check()
            time.sleep(min(.02, max(.001, deadline - time.monotonic())))
        check()

    def record(phase, round_number=None):
        value = sampler.take()
        check()
        previous = result["samples"][-1]["sample"] if result["samples"] else None
        entry = dict(phase=phase, round=round_number, sample=value,
                     delta=metrics.delta(previous, value) if previous else None)
        result["samples"].append(entry)
        result["final"] = value
        emit(entry)
        return value

    try:
        pty_class = _pty_class()
        if args.rounds < 1 or args.sample_interval <= 0 or startup <= 0 or args.idle_seconds < 0 or ceiling <= 0:
            raise ValueError("invalid TUI measurement budgets")
        # Verified in v0.16.0 args.rs: default TTY mode is interactive; --mode
        # accepts text/json/rpc, NOT tui. All flags below exist in that release.
        command = [str(Path(binary).resolve()), "--no-extensions", "--no-skills",
                   "--no-context-files", "--no-prompt-templates", "--no-tools",
                   "--thinking", "off", "--local-only", "--offline",
                   "--provider", "resource-fixture", "--model", "synthetic",
                   "--session-dir", str((root / "sessions").resolve())]
        result["command"] = command
        proc = pty_class.spawn(command, cwd=str(root / "workspace"), env=dict(env),
                               dimensions=(40, 120), backend=0)  # Backend.ConPTY (WinPTY is 1)
        result["pid"] = proc.pid
        terminal = Terminal(proc, root / "terminal.log")
        sampler = metrics.Sampler(proc.pid, min(float(args.sample_interval), .1))
        record("startup")
        wait_for(lambda: bool(re.search(r"Enter\s*:?\s*[Ss]end", terminal.view()[0])),
                 startup, "rendered TUI send-prompt not observed within startup budget")
        result["readiness"] = "rendered Enter send prompt (ANSI stripped)"
        record("startup-ready")
        idle()
        result["baseline"] = record("idle-baseline")
        result["startupStatus"] = "PASS"
        for number in range(1, args.rounds + 1):
            terminal.mark()
            terminal.send("\x1b[200~" + "p" * PASTE_BYTES + "\x1b[201~", check)
            wait_for(lambda: bool(re.search(r"\[paste #\d+ \d+ chars\]", terminal.view()[0])),
                     max(2, startup), "64 KiB paste rendering not confirmed")
            paste_evidence = re.search(r"\[paste #\d+ \d+ chars\]", terminal.view()[0])[0]
            terminal.mark()
            small = ("resource_edit_%04d_" % number).ljust(64, "q")
            terminal.send(small, check)
            wait_for(lambda: small[:19] in terminal.view()[0], max(2, startup),
                     "small insertion rendering not confirmed")
            marker = terminal.mark()
            terminal.send("\x7f" * 64, check)
            wait_for(lambda: terminal.view()[1] > marker, max(2, startup),
                     "no rendered output after backspace workload")
            result["workload"]["confirmations"].append(dict(round=number,
                pasteRendered=paste_evidence, insertionRendered=small[:19],
                backspaceOutputBytes=terminal.view()[1] - marker))
            record("post-edit", number)
            idle()
            record("post-idle", number)
            result["roundsCompleted"] = number
        record("final", args.rounds)
        result.update(status="PASS", pressureStatus="PASS")
    except Unconfirmed as error:
        result["reason"] = str(error)
        if terminal:
            result["unconfirmedRenderedTail"] = terminal.view()[0][-1024:]
    except Exception as error:
        result.update(status="FAIL", reason=str(error),
                      category=getattr(error, "category", "measurement"))
    finally:
        cleanup = []
        if sampler:
            try:
                if proc.isalive():
                    value = sampler.take()
                    previous = result["samples"][-1]["sample"] if result["samples"] else None
                    entry = dict(phase="cleanup-snapshot", round=result["roundsCompleted"],
                                 sample=value, delta=metrics.delta(previous, value) if previous else None)
                    result["samples"].append(entry)
                    result["final"] = value
                    emit(entry)
                sampler.close()
            except Exception as error:
                cleanup.append(str(error))
            result["peakPrimaryBytes"] = sampler.peak
            result["metricSampleCount"] = sampler.count
            if sampler.peak > ceiling:
                result.update(status="FAIL", category="safety", reason="memory safety ceiling exceeded")
            values = [e["sample"] for e in result["samples"]]
            if result["final"]:
                values.append(result["final"])
            result["peakWorkingSetBytes"] = max((v["peak_rss_bytes"] for v in values), default=0)
            result["peakCpuPercent"] = max((e["delta"]["cpu_percent"] for e in result["samples"] if e["delta"]), default=0)
        if terminal:
            cleanup.extend(terminal.close())
            result["terminalBytes"] = terminal.total
            result["terminalLogBytes"] = terminal.saved
            result["terminalLogTruncated"] = terminal.total > terminal.saved
            result["terminalQueriesAnswered"] = terminal.queries
            result["workload"]["completedInputBytes"] = terminal.input_bytes
        elif proc:
            try:
                proc.close(force=True)
            except Exception as error:
                cleanup.append(str(error))
        if proc:
            result["exitStatus"] = getattr(proc, "exitstatus", None)
            result["cleanupIntentional"] = True
            try:
                if proc.isalive():
                    cleanup.append("owned TUI still alive after cleanup")
            except Exception as error:
                cleanup.append(str(error))
        if cleanup:
            result.update(status="FAIL", cleanupErrors=cleanup)
    return result
