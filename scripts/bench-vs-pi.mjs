#!/usr/bin/env node
/**
 * Compare `rpi` against the native TypeScript `pi` coding agent.
 *
 * Both tools are measured the same way, on the same machine, in the same shell:
 *
 *   1. `--version`      process spawn -> exit. The floor cost of starting the
 *                       tool at all.
 *   2. RPC ready        process spawn -> the answer to a real
 *                       `{"type":"get_state"}` request on the JSONL RPC channel.
 *                       This is the metric native pi's own
 *                       `scripts/profile-coding-agent-node.mjs` defines for "the
 *                       agent is actually usable", so it is the fair comparison
 *                       point rather than something invented here.
 *   3. RSS at ready     resident set size read once the agent has answered, in a
 *                       separate run from the timing runs.
 *
 * Memory is measured in its own phase on purpose. Reading another process's RSS
 * on Windows means spawning `tasklist`, which costs tens of milliseconds; doing
 * that on a timer inside the timing window loads the CPU and inflates the number
 * being measured. Timing runs therefore do no sampling at all.
 *
 * Fairness rules applied to both sides:
 *   - `PI_OFFLINE=1` and a fresh, empty `PI_CODING_AGENT_DIR` per run, so neither
 *     performs network work and neither reads machine-specific config,
 *     extensions or sessions. rpi honours the same `PI_OFFLINE` variable native
 *     pi sets, so neither is disadvantaged by the offline settings.
 *   - a neutral temporary working directory for both, so neither scans this
 *     repository on startup.
 *   - provider credentials are cleared from the environment.
 *   - warmup runs are discarded; medians are reported.
 *
 * npm installs pi behind a shim (`.bin/pi`), which on Windows is not directly
 * executable and would add a `cmd.exe` hop if used. The script resolves the
 * shim to its JavaScript entrypoint and runs it under `node` directly, so the
 * measurement is Node startup, not shell startup.
 *
 * Usage:
 *   node scripts/bench-vs-pi.mjs --pi <path-to-pi> [--rpi <path>] [--runs 7]
 *
 * To obtain the comparison target:
 *   npm install --prefix .bench @earendil-works/pi-coding-agent
 *   node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";

const IS_WINDOWS = process.platform === "win32";
const MEMORY_RUNS = 3;

function parseArgs(argv) {
  const options = { pi: null, rpi: null, runs: 7, warmup: 2, json: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      const value = argv[i + 1];
      if (value === undefined) throw new Error(`${arg} needs a value`);
      i += 1;
      return value;
    };
    if (arg === "--pi") options.pi = next();
    else if (arg === "--rpi") options.rpi = next();
    else if (arg === "--runs") options.runs = Number.parseInt(next(), 10);
    else if (arg === "--warmup") options.warmup = Number.parseInt(next(), 10);
    else if (arg === "--json") options.json = next();
    else if (arg === "--help" || arg === "-h") {
      console.log("usage: node scripts/bench-vs-pi.mjs --pi <path> [--rpi <path>] [--runs N] [--warmup N] [--json <file>]");
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return options;
}

/**
 * Turn a path into something spawnable.
 *
 * - a `.js`/`.mjs` entrypoint runs under the current Node binary;
 * - an npm shim (`.bin/pi`) is read and redirected to the JS file it wraps,
 *   which is also what keeps `cmd.exe` out of the measurement on Windows;
 * - anything else is executed directly.
 */
function resolveInvocation(path) {
  if (/\.(m?js|cjs)$/i.test(path)) {
    return { command: process.execPath, args: [path], display: `${process.execPath} ${path}` };
  }

  const shim = `${path}.cmd`;
  if (IS_WINDOWS && existsSync(shim)) {
    const text = readFileSync(shim, "utf8");
    const matches = [...text.matchAll(/"([^"]*\.(?:m?js|cjs))"/gi)];
    const captured = matches.at(-1)?.[1];
    if (captured) {
      const dir = dirname(shim);
      const target = resolve(dir, captured.replace(/%dp0%/gi, dir));
      if (existsSync(target)) {
        return { command: process.execPath, args: [target], display: `${process.execPath} ${target}` };
      }
    }
  }

  return { command: path, args: [], display: path };
}

/** Locate the release build of `rpi` through cargo's target directory. */
function resolveRpi(explicit) {
  if (explicit) return resolve(explicit);
  const meta = spawnSync("cargo", ["metadata", "--no-deps", "--format-version", "1", "--locked"], { encoding: "utf8" });
  if (meta.status !== 0) throw new Error("could not run `cargo metadata` to locate the rpi binary");
  const targetDir = JSON.parse(meta.stdout).target_directory;
  const candidate = join(targetDir, "release", IS_WINDOWS ? "rpi.exe" : "rpi");
  if (existsSync(candidate)) return candidate;
  throw new Error(`no release build found at ${candidate}. Run: cargo build -p rpi-cli --release`);
}

function run(invocation, extraArgs, options) {
  return spawnSync(invocation.command, [...invocation.args, ...extraArgs], options);
}

function readRssBytes(pid) {
  if (IS_WINDOWS) {
    const out = spawnSync("tasklist", ["/FI", `PID eq ${pid}`, "/FO", "CSV", "/NH"], { encoding: "utf8" }).stdout ?? "";
    const match = out.match(/"([\d\s.,]+) K"/);
    if (!match) return null;
    const kb = Number.parseInt(match[1].replace(/[^\d]/g, ""), 10);
    return Number.isFinite(kb) ? kb * 1024 : null;
  }
  const out = spawnSync("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" }).stdout ?? "";
  const kb = Number.parseInt(out.trim(), 10);
  return Number.isFinite(kb) && kb > 0 ? kb * 1024 : null;
}

function killTree(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  if (IS_WINDOWS) spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  else child.kill("SIGKILL");
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0 ? (sorted[mid - 1] + sorted[mid]) / 2 : sorted[mid];
}

const ms = (value) => `${value.toFixed(1)} ms`;
const mib = (bytes) => `${(bytes / 1048576).toFixed(1)} MiB`;

/** Fresh, empty config dir + offline + a neutral cwd, identically for both tools. */
function runEnvironment(agentDir) {
  return {
    ...process.env,
    PI_OFFLINE: "1",
    PI_SKIP_VERSION_CHECK: "1",
    PI_CODING_AGENT_DIR: agentDir,
    ANTHROPIC_API_KEY: "",
    ANTHROPIC_AUTH_TOKEN: "",
    OPENAI_API_KEY: "",
  };
}

function measureVersion(tool, { env, cwd }) {
  const startedAt = performance.now();
  const result = run(tool.invocation, ["--version"], { env, cwd, encoding: "utf8" });
  const elapsedMs = performance.now() - startedAt;
  if (result.status !== 0) {
    throw new Error(`${tool.name} --version exited ${result.status} (${result.error?.code ?? "no error code"}): ${result.stderr ?? ""}`);
  }
  return elapsedMs;
}

/**
 * Spawn the tool in RPC mode, ask for `get_state`, and wait for the answer.
 * No sampling happens in here -- see the header for why.
 */
function measureRpc(tool, { env, cwd, timeoutMs = 180000, onReady }) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(tool.invocation.command, [...tool.invocation.args, "--mode", "rpc"], {
      env,
      cwd,
      stdio: ["pipe", "pipe", "pipe"],
    });
    const requestId = "bench-get-state";
    let stdoutBuffer = "";
    let stderr = "";
    let settled = false;
    const startedAt = performance.now();

    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(`${tool.name} gave no get_state response within ${timeoutMs} ms; stderr: ${stderr.slice(0, 400)}`));
    }, timeoutMs);

    function cleanup() {
      clearTimeout(timer);
      child.stdout.removeAllListeners();
      child.stderr.removeAllListeners();
      child.stdin.destroy();
      killTree(child);
    }

    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdoutBuffer += chunk;
      let index;
      while ((index = stdoutBuffer.indexOf("\n")) >= 0) {
        const line = stdoutBuffer.slice(0, index).trim();
        stdoutBuffer = stdoutBuffer.slice(index + 1);
        if (!line) continue;
        let parsed;
        try {
          parsed = JSON.parse(line);
        } catch {
          continue; // non-JSON banner output
        }
        if (parsed?.type !== "response" || parsed?.id !== requestId || settled) continue;
        settled = true;
        const elapsedMs = performance.now() - startedAt;
        // The caller may need the live process (to read its RSS) before it dies.
        if (onReady) {
          Promise.resolve(onReady(child)).then(
            (value) => {
              cleanup();
              resolvePromise({ elapsedMs, extra: value });
            },
            (error) => {
              cleanup();
              reject(error);
            },
          );
          return;
        }
        cleanup();
        const failed = parsed.success === false || (parsed.status !== undefined && parsed.status !== "ok");
        if (failed) reject(new Error(`${tool.name} answered get_state with a failure: ${line.slice(0, 300)}`));
        else resolvePromise({ elapsedMs });
      }
    });

    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });

    child.on("error", (error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    });

    child.on("exit", (code) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(`${tool.name} exited early with code ${code}; stderr: ${stderr.slice(0, 400)}`));
    });

    child.stdin.write(`${JSON.stringify({ id: requestId, type: "get_state" })}\n`);
  });
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (!options.pi) {
    console.error("error: --pi <path> is required (see the header of this script to install it)");
    process.exit(2);
  }

  const piPath = resolve(options.pi);
  if (!existsSync(piPath)) throw new Error(`pi not found at ${piPath}`);
  const rpiPath = resolveRpi(options.rpi);

  const tools = [
    { name: "rpi", kind: "Rust", path: rpiPath },
    { name: "pi", kind: "TypeScript", path: piPath },
  ].map((tool) => ({ ...tool, invocation: resolveInvocation(tool.path) }));

  for (const tool of tools) {
    const probe = run(tool.invocation, ["--version"], { encoding: "utf8" });
    if (probe.status !== 0) {
      throw new Error(`could not run ${tool.name} (${tool.invocation.display}): ${probe.error?.code ?? probe.stderr}`);
    }
    tool.version = String(probe.stdout).trim().split("\n")[0];
    tool.label = `${tool.name} (${tool.kind}, v${tool.version.replace(/^\D+/, "")})`;
  }

  console.log(`machine: ${process.platform} ${process.arch}, node ${process.versions.node}`);
  console.log(`runs: ${options.runs} measured (+${options.warmup} warmup), fresh isolated agent dir per run\n`);
  for (const tool of tools) console.log(`  ${tool.name.padEnd(3)} -> ${tool.invocation.display}`);
  console.log("");

  const scratch = mkdtempSync(join(tmpdir(), "rpi-vs-pi-"));
  const neutralCwd = join(scratch, "cwd");
  const results = {};

  try {
    for (const tool of tools) {
      const versionTimes = [];
      const readyTimes = [];
      const rssValues = [];
      const total = options.runs + options.warmup;

      for (let index = 0; index < total; index += 1) {
        const agentDir = join(scratch, `${tool.name}-agent-${index}`);
        rmSync(agentDir, { recursive: true, force: true });
        rmSync(neutralCwd, { recursive: true, force: true });
        mkdirSync(agentDir, { recursive: true });
        mkdirSync(neutralCwd, { recursive: true });
        const env = runEnvironment(agentDir);

        const measured = index >= options.warmup;
        const versionMs = measureVersion(tool, { env, cwd: neutralCwd });
        const rpc = await measureRpc(tool, { env, cwd: neutralCwd });

        if (measured) {
          versionTimes.push(versionMs);
          readyTimes.push(rpc.elapsedMs);
        }

        const tag = measured ? `run ${index - options.warmup + 1}` : `warmup ${index + 1}`;
        console.log(`  ${tool.name.padEnd(3)} ${tag.padEnd(9)} version ${ms(versionMs).padStart(9)}   rpc-ready ${ms(rpc.elapsedMs).padStart(9)}`);
      }

      // Separate phase: reading another process's RSS costs a `tasklist` spawn on
      // Windows, so it is kept out of the timing loop entirely.
      for (let index = 0; index < MEMORY_RUNS; index += 1) {
        const agentDir = join(scratch, `${tool.name}-mem-${index}`);
        rmSync(agentDir, { recursive: true, force: true });
        rmSync(neutralCwd, { recursive: true, force: true });
        mkdirSync(agentDir, { recursive: true });
        mkdirSync(neutralCwd, { recursive: true });
        const env = runEnvironment(agentDir);
        const { extra } = await measureRpc(tool, {
          env,
          cwd: neutralCwd,
          onReady: (child) => readRssBytes(child.pid),
        });
        if (extra) rssValues.push(extra);
        console.log(`  ${tool.name.padEnd(3)} memory ${String(index + 1).padEnd(5)} rss-at-ready ${(extra ? mib(extra) : "n/a").padStart(9)}`);
      }

      results[tool.name] = {
        label: tool.label,
        version: tool.version,
        versionMs: median(versionTimes),
        versionAll: versionTimes,
        rpcReadyMs: median(readyTimes),
        rpcReadyAll: readyTimes,
        rssAtReadyBytes: rssValues.length ? median(rssValues) : null,
        rssAtReadyAll: rssValues,
      };
      console.log("");
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }

  const rpi = results.rpi;
  const pi = results.pi;
  const rows = [
    ["`--version` (median)", ms(rpi.versionMs), ms(pi.versionMs), `${(pi.versionMs / rpi.versionMs).toFixed(1)}× faster`],
    ["RPC ready (median)", ms(rpi.rpcReadyMs), ms(pi.rpcReadyMs), `${(pi.rpcReadyMs / rpi.rpcReadyMs).toFixed(1)}× faster`],
  ];
  if (rpi.rssAtReadyBytes && pi.rssAtReadyBytes) {
    rows.push(["RSS at ready", mib(rpi.rssAtReadyBytes), mib(pi.rssAtReadyBytes), `${(pi.rssAtReadyBytes / rpi.rssAtReadyBytes).toFixed(1)}× smaller`]);
  }

  console.log(`| Metric | ${rpi.label} | ${pi.label} | Difference |`);
  console.log("| ------ | ---------: | ----------: | ---------- |");
  for (const [metric, left, right, ratio] of rows) console.log(`| ${metric} | ${left} | ${right} | **${ratio}** |`);

  if (options.json) {
    writeFileSync(
      options.json,
      `${JSON.stringify(
        {
          machine: { platform: process.platform, arch: process.arch, node: process.versions.node },
          options: { runs: options.runs, warmup: options.warmup },
          invocations: Object.fromEntries(tools.map((t) => [t.name, t.invocation.display])),
          results,
        },
        null,
        2,
      )}\n`,
    );
    console.log(`\nwrote ${options.json}`);
  }
}

main().catch((error) => {
  console.error(`\nbenchmark failed: ${error.message}`);
  process.exit(1);
});
