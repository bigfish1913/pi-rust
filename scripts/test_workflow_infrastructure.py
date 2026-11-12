"""Offline regressions for the actual inline CI/recovery workflow scripts.

No apt, sudo, network, PyYAML or GitHub credentials are needed. Recovery checks
out old tags, so release dependency setup must remain inline in its workflow.
"""
from pathlib import Path
import os
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
CI = ROOT / ".github/workflows/ci.yml"
RELEASE = ROOT / ".github/workflows/release-binaries.yml"


def job(text, name):
    return re.search(rf"^  {name}:\n(.*?)(?=^  [\w-]+:|\Z)", text, re.M | re.S)[1]


def script(text, name):
    block = re.split(r"\n(?:      - |  [\w-]+:)",
                     text.split(f"      - name: {name}\n", 1)[1], maxsplit=1)[0]
    lines = block.split("        run: |\n", 1)[1].splitlines()
    return "\n".join(line[10:] if line.startswith("          ") else line for line in lines) + "\n"


MOCKS = r'''
updates=0
installs=0
compiler_ready=$PREINSTALLED
command() {
  if [[ "$1" == -v && "$2" == musl-gcc ]]; then
    if (( compiler_ready )); then echo mock-musl-gcc; return 0; fi
    return 1
  fi
  builtin command "$@"
}
timeout() {
  [[ "$1" == 120s ]] || exit 90
  shift
  "$@"
}
sudo() { "$@"; }
apt-get() {
  case "$1" in
    update)
      updates=$((updates + 1))
      echo UPDATE
      [[ "$*" == *APT::Update::Error-Mode=any* ]] || exit 91
      if (( updates <= UPDATE_FAILURES )); then return "$FAILURE_CODE"; fi
      ;;
    install)
      installs=$((installs + 1))
      echo INSTALL
      if (( installs <= INSTALL_FAILURES )); then return "$FAILURE_CODE"; fi
      compiler_ready=1
      ;;
    *) exit 92 ;;
  esac
  [[ "$*" == *Acquire::Retries=2* && "$*" == *Acquire::http::Timeout=15* &&
     "$*" == *Acquire::https::Timeout=15* ]] || exit 93
}
sleep() { :; }
musl-gcc() {
  echo LINK_PROBE
  (( PROBE_FAILURE == 0 )) || return 1
  while [[ "$1" != -o ]]; do shift; done
  printf '#!/bin/sh\nexit 0\n' > "$2"
  chmod +x "$2"
}
'''


class AptWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        ci, release = CI.read_text(encoding="utf-8"), RELEASE.read_text(encoding="utf-8")
        cls.musl = [script(text, "Install musl linker") for text in (ci, release)]
        cls.native = script(ci, "Install Linux native dependencies")

    def execute(self, code, update=0, install=0, preinstalled=0, probe_failure=0, failure_code=100):
        with tempfile.TemporaryDirectory() as directory:
            env = os.environ | {"RUNNER_TEMP": directory.replace("\\", "/"),
                                "UPDATE_FAILURES": str(update), "INSTALL_FAILURES": str(install),
                                "PREINSTALLED": str(preinstalled), "PROBE_FAILURE": str(probe_failure),
                                "FAILURE_CODE": str(failure_code)}
            result = subprocess.run(["bash", "-euo", "pipefail"], input=MOCKS + code,
                                    text=True, capture_output=True, env=env, timeout=15)
            self.assertFalse((Path(directory) / "musl-linker-smoke").exists())
            return result

    def test_ci_and_old_tag_recovery_use_identical_inline_setup(self):
        self.assertEqual(*self.musl)
        self.assertIn("musl-gcc -static -x c -", self.musl[0])
        for path in (CI, RELEASE):
            text = path.read_text(encoding="utf-8")
            block = text.split("      - name: Install musl linker\n", 1)[1].split("\n      - ", 1)[0]
            self.assertIn("timeout-minutes: 15", block)

    def test_bash_syntax(self):
        for code in [*self.musl, self.native]:
            subprocess.run(["bash", "-n"], input=code, text=True, check=True)

    def test_retry_success_and_failure_propagation(self):
        cases = [(0, 0, 0, 1, 1), (2, 0, 0, 3, 1), (0, 2, 0, 3, 3),
                 (99, 0, 1, 3, 0), (0, 99, 1, 3, 3)]
        for index, code in enumerate([*self.musl, self.native]):
            for uf, inf, expected, updates, installs in cases:
                for failure_code in (100, 124):  # apt failure or command timeout
                    with self.subTest(script=index, update=uf, install=inf, code=failure_code):
                        result = self.execute(code, update=uf, install=inf, failure_code=failure_code)
                        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                        self.assertEqual(result.stdout.splitlines().count("UPDATE"), updates)
                        self.assertEqual(result.stdout.splitlines().count("INSTALL"), installs)

    def test_preinstalled_compiler_skips_network_but_still_links(self):
        for code in self.musl:
            result = self.execute(code, preinstalled=1, update=99, install=99)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("UPDATE", result.stdout)
            self.assertNotIn("INSTALL", result.stdout)
            self.assertIn("LINK_PROBE", result.stdout)

    def test_broken_preinstalled_compiler_is_not_a_success(self):
        for code in self.musl:
            result = self.execute(code, preinstalled=1, probe_failure=1)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("UPDATE", result.stdout)


class ValidationGateTests(unittest.TestCase):
    def setUp(self):
        self.ci = CI.read_text(encoding="utf-8")

    def test_required_aggregate_checks_every_dependency_on_ordinary_prs_too(self):
        block = job(self.ci, "release-validation")
        self.assertIn("if: always()", block)
        condition = " ".join(block.split("        if: >-\n", 1)[1].split("        run: |", 1)[0].split())
        # Only an upstream draft Release PR defers validation, matching the
        # dependency jobs. A normal or fork PR must never get a no-op success.
        self.assertEqual(condition, "${{ !(github.event_name == 'pull_request' && "
                         "github.repository == 'bigfish1913/pi-rust' && "
                         "github.event.pull_request.base.ref == 'main' && "
                         "github.event.pull_request.head.repo.full_name == github.repository && "
                         "startsWith(github.event.pull_request.head.ref, 'release-plz-') && "
                         "github.event.pull_request.draft == true) }}")
        code = script(self.ci, "Require every validation job to pass")
        fields = re.findall(r"\$\{\{ (needs[^}]+) \}\}", code)
        self.assertEqual(len(fields), 7)
        for failing in [None, *fields]:
            for status in ("failure", "cancelled", "skipped"):
                with self.subTest(failing=failing, status=status):
                    rendered = re.sub(r"\$\{\{ (needs[^}]+) \}\}",
                                      lambda match: status if match[1] == failing else "success", code)
                    result = subprocess.run(["bash"], input=rendered, text=True, capture_output=True)
                    self.assertEqual(result.returncode == 0, failing is None)

    def test_performance_and_release_platform_baselines_do_not_drift(self):
        resources = job(self.ci, "resources")
        self.assertIn("runs-on: ubuntu-24.04", resources)
        self.assertIn("--max-ready-ms 4000 --max-single-ready-ms 8000", resources)
        self.assertIn("--max-ready-ms 6000 --max-single-ready-ms 10000", resources)
        self.assertIn("--frames 701000 --entries 21733 --repeats 3", resources)
        self.assertIn("ImageVersion", resources)
        self.assertIn("${{ env.COLD_START_DIAGNOSTICS_NAME }}-environment.txt", resources)
        self.assertNotIn("continue-on-error", resources)
        platform = job(self.ci, "platform-startup")
        for target in ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"):
            self.assertIn(f"- os: ubuntu-24.04\n            target: {target}", platform)
        # Retain rolling compatibility coverage AND branch-protection names.
        self.assertIn("os: [ubuntu-latest, windows-latest]", self.ci)
        self.assertIn("name: Release PR validation", self.ci)


if __name__ == "__main__":
    unittest.main()
