#!/usr/bin/env python3
"""Build/watch the patched web vault inside a 4 GiB, no-swap cgroup."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path, help="Installed vw_web_builds checkout")
    parser.add_argument("--node", type=Path, help="Node >=24.17 executable")
    parser.add_argument("--mode", choices=["watch", "once", "serve"], default="watch")
    args = parser.parse_args()
    source = args.source.resolve()
    if not (source / "apps/web/webpack.config.js").is_file():
        parser.error("--source must contain apps/web/webpack.config.js")
    node = args.node or shutil.which("node")
    if not node:
        parser.error("Node is missing; supply --node")
    node = Path(node).resolve()
    version = subprocess.check_output([node, "--version"], text=True).strip()
    if tuple(map(int, version.lstrip("v").split("."))) < (24, 17, 0):
        parser.error(f"Node {version} is too old; supply --node pointing to Node >=24.17")
    cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "tutor-web-vault"
    cache.mkdir(parents=True, exist_ok=True)
    lock = (cache / (hashlib.sha256(str(source).encode()).hexdigest()[:16] + ".lock")).open("w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        parser.error("Another Tutor dev build already uses this source checkout")
    unit = f"tutor-web-vault-{os.getpid()}.service"
    logs = cache / unit.removesuffix(".service")
    logs.mkdir()
    output = logs / "build.log"
    env = dict(os.environ)
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    env.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path={env['XDG_RUNTIME_DIR']}/bus")
    env.update(WEB_VAULT_SOURCE=str(source), NODE_ENV="development", ENV="selfhosted",
               TUTOR_METRICS=str(logs / "v8.jsonl"))
    # Transient services do not inherit the invoking shell's environment.
    command = ["systemd-run", "--user", f"--unit={unit}",
               "-p", "MemoryMax=4G", "-p", "MemorySwapMax=0", "-p", "OOMPolicy=kill",
               "-p", "KillMode=control-group", "-p", "TimeoutStopSec=15", "-p", "RemainAfterExit=yes",
               "-p", f"StandardOutput=append:{output}", "-p", f"StandardError=append:{output}",
               "/usr/bin/env", *[f"{key}={env[key]}" for key in
                 ["WEB_VAULT_SOURCE", "NODE_ENV", "ENV", "TUTOR_METRICS"]],
               str(node), "--max-old-space-size=3072", str(Path(__file__).resolve().with_name("run.cjs")), args.mode]
    subprocess.run(command, env=env, check=True)
    print(f"Logs: {logs}\nUnit: {unit} (4 GiB; no swap)", flush=True)
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    peak_rss = 0
    peak_cgroup = 0
    read_offset = 0
    stop_sent = None
    details = ""
    try:
        with (logs / "cgroup.jsonl").open("w") as measurements:
            while True:
                state = subprocess.check_output(
                    ["systemctl", "--user", "show", unit, "-p", "ControlGroup", "-p", "ActiveState", "-p", "SubState"],
                    env=env, text=True)
                props = dict(line.split("=", 1) for line in state.splitlines() if "=" in line)
                group_name = props.get("ControlGroup")
                group = Path("/sys/fs/cgroup") / group_name.lstrip("/") if group_name else None
                row = {"time": time.time(), "rss": 0}
                if group and group.exists():
                    for key in ["memory.current", "memory.peak", "memory.events", "cpu.stat"]:
                        try:
                            row[key] = (group / key).read_text().strip()
                        except FileNotFoundError:
                            pass
                    # Sum every process in the cgroup and its descendants.
                    groups = [group, *[p for p in group.rglob("*") if p.is_dir()]]
                    for child in groups:
                        try:
                            pids = (child / "cgroup.procs").read_text().split()
                        except FileNotFoundError:
                            continue
                        for pid in pids:
                            try:
                                status = Path("/proc", pid, "status").read_text()
                                row["rss"] += int(status.split("VmRSS:")[1].split()[0]) * 1024
                            except (FileNotFoundError, IndexError, ProcessLookupError):
                                pass
                    peak_rss = max(peak_rss, row["rss"])
                    peak_cgroup = max(peak_cgroup, int(row.get("memory.peak", 0)))
                    measurements.write(json.dumps(row) + "\n")
                    measurements.flush()
                if output.exists():
                    with output.open() as stream:
                        stream.seek(read_offset)
                        print(stream.read(), end="", flush=True)
                        read_offset = stream.tell()
                if props.get("ActiveState") in ("inactive", "failed") or props.get("SubState") == "exited":
                    break
                if stopping and stop_sent is None:
                    # Keep the exited unit around until its result and resource
                    # counters have been collected; stopping it first unloads it.
                    subprocess.run(["systemctl", "--user", "kill", "--signal=SIGTERM", unit], env=env, check=False)
                    stop_sent = time.monotonic()
                elif stop_sent is not None and time.monotonic() - stop_sent > 15:
                    subprocess.run(["systemctl", "--user", "kill", "--signal=SIGKILL", unit], env=env, check=False)
                time.sleep(0.25)
        details = subprocess.check_output(
            ["systemctl", "--user", "show", unit, "-p", "Result", "-p", "ExecMainStatus",
             "-p", "MemoryPeak", "-p", "CPUUsageNSec"], env=env, text=True)
    finally:
        subprocess.run(["systemctl", "--user", "stop", unit], env=env, check=False)
    (logs / "summary.txt").write_text(
        details + f"SampledProcessTreePeakRSS={peak_rss}\nSampledCgroupPeak={peak_cgroup}\n")
    print(details + f"Sampled process-tree peak RSS: {peak_rss / 2**30:.2f} GiB")
    return 0 if "Result=success\n" in details else 1


if __name__ == "__main__":
    raise SystemExit(main())
