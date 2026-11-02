#!/usr/bin/env python3
"""Synthetic JSONL history probe runner; never opens user session directories.

Build crates/rpi-harness/examples/jsonl_history_probe.rs with --release first.
The output root must not already exist. Timing and memory use separate children.
Native peak counters retain the load peak, so no sampler competes with loading.
--exercise additionally uses a fresh fixture and child for paging/append phases.
Its native peaks are cumulative process high-water marks, not per-phase peaks;
RSS samples and raw timings are observations, not a constant-memory guarantee.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import subprocess
import time


def memory(process):
    if os.name == 'nt':
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD)] + [
                (name, ctypes.c_size_t) for name in (
                    'PeakWorkingSetSize', 'WorkingSetSize', 'QuotaPeakPagedPoolUsage',
                    'QuotaPagedPoolUsage', 'QuotaPeakNonPagedPoolUsage',
                    'QuotaNonPagedPoolUsage', 'PagefileUsage', 'PeakPagefileUsage', 'PrivateUsage')]
        value = Counters()
        value.cb = ctypes.sizeof(value)
        call = ctypes.WinDLL('psapi', use_last_error=True).GetProcessMemoryInfo
        call.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
        call.restype = wintypes.BOOL
        if not call(wintypes.HANDLE(process._handle), ctypes.byref(value), value.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        return {'readyRssBytes': value.WorkingSetSize,
                'peakRssBytes': value.PeakWorkingSetSize,
                'readyPrivateBytes': value.PrivateUsage,
                'method': 'Windows GetProcessMemoryInfo after ready; native peak counter'}
    status = {}
    for line in Path(f'/proc/{process.pid}/status').read_text(encoding='utf8').splitlines():
        if ':' in line:
            key, val = line.split(':', 1)
            status[key] = val.strip()
    return {'readyRssBytes': int(status['VmRSS'].split()[0]) * 1024,
            'peakRssBytes': int(status['VmHWM'].split()[0]) * 1024,
            'method': 'Linux /proc status VmRSS/VmHWM after ready'}


def run(binary, fixture, phase):
    process = subprocess.Popen([str(binary), 'load', str(fixture)],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, encoding='utf8')
    try:
        ready = process.stdout.readline()
        if not ready:
            raise RuntimeError(process.stderr.read())
        result = json.loads(ready)
        if result.get('phase') != 'ready':
            raise RuntimeError(f'unexpected probe response: {ready}')
        if phase == 'memory':
            result.update(memory(process))
            # Give the process a quiescent interval; do not ask Rust to trim heaps.
            time.sleep(0.25)
            result['quiescent'] = memory(process)
        stdout, stderr = process.communicate(timeout=30)
        if process.returncode:
            raise RuntimeError(f'probe exit {process.returncode}: {stderr} {stdout}')
        result['measurementPhase'] = phase
        return result
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


def run_exercise(binary, fixture, append_count, body_bytes):
    process = subprocess.Popen([str(binary), 'exercise', str(fixture),
                                str(append_count), str(body_bytes)],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, encoding='utf8')
    phases = []
    try:
        for line in process.stdout:
            result = json.loads(line)
            expected = 'loaded' if not phases else ('paged' if len(phases) == 1 else 'appended')
            if result.get('phase') != expected:
                raise RuntimeError(f'unexpected exercise response: {line}')
            # Phase stdout is flushed before a 500 ms pause (last phase: 2 s).
            # The native counter includes all earlier phases, including load.
            sample = memory(process)
            sample['method'] = sample['method'].replace('after ready', 'after phase')
            result['memory'] = sample
            result['sampleMonotonicNs'] = time.monotonic_ns()
            time.sleep(0.25)
            sample = memory(process)
            sample['method'] = sample['method'].replace('after ready', 'after phase')
            result['quiescent'] = sample
            phases.append(result)
        _, stderr = process.communicate(timeout=30)
        if process.returncode:
            raise RuntimeError(f'exercise exit {process.returncode}: {stderr}')
        if (len(phases) < 3 or not phases[-1].get('finished')
                or phases[-1].get('appendCount') != append_count):
            raise RuntimeError(f'incomplete exercise phases: {phases}')
        return phases
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output-root', type=Path, required=True)
    parser.add_argument('--label', required=True)
    parser.add_argument('--exercise', action='store_true',
                        help='also measure paging and batched append on a separate 512 x 128 KiB fixture')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.output_root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    results = []
    for count, body_bytes in [(512, 8192), (512, 131072), (4096, 8192)]:
        fixture = root / f'entries-{count}-body-{body_bytes}'
        generated = subprocess.run([str(binary), 'generate', str(fixture),
                                    str(count), str(body_bytes)], check=True,
                                   capture_output=True, text=True, encoding='utf8')
        case = {'label': args.label, 'fixture': json.loads(generated.stdout),
                'timing': run(binary, fixture, 'timing'),
                'memory': run(binary, fixture, 'memory')}
        results.append(case)
        print(json.dumps(case, ensure_ascii=False), flush=True)
        (root / 'results.json').write_text(json.dumps(results, ensure_ascii=False, indent=2),
                                          encoding='utf8')
    if args.exercise:
        count, body_bytes, append_count = 512, 131072, 512
        fixture = root / f'exercise-entries-{count}-body-{body_bytes}-append-{append_count}'
        generated = subprocess.run([str(binary), 'generate', str(fixture),
                                    str(count), str(body_bytes)], check=True,
                                   capture_output=True, text=True, encoding='utf8')
        result = {
            'label': args.label,
            'fixture': json.loads(generated.stdout),
            'appendCount': append_count,
            'bodyBytes': body_bytes,
            'appendPayloadBytes': append_count * body_bytes,
            'measurementPhase': 'exercise',
            'notes': ('Independent child; phase RSS and cumulative native peak, no heap trimming. '
                      'Raw operation timings exclude phase pauses but include sampling overhead '
                      'if the sampler overlaps work; not a constant-memory guarantee.'),
            'phases': run_exercise(binary, fixture, append_count, body_bytes),
        }
        with (root / 'exercise-results.json').open('x', encoding='utf8') as output:
            json.dump(result, output, ensure_ascii=False, indent=2)
        print(json.dumps(result, ensure_ascii=False), flush=True)



if __name__ == '__main__':
    main()
