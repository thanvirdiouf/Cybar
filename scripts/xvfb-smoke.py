#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Exercise Cybar on an already-running, isolated Xvfb display.

Usage: DISPLAY=:117 python3 scripts/xvfb-smoke.py target/debug/cybar
The caller must provide an isolated Xvfb server. DISPLAY=:0 is refused. All
configuration, sockets, logs, and child-process fixtures live in a temporary
directory. No third-party Python packages or X11 command-line tools are needed.
"""

import json
import os
from pathlib import Path
import re
import select
import shlex
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time


class StreamServer:
    """Small Unix socket source; each connection receives the latest line."""

    def __init__(self, path):
        self.path = path
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(32)
        self.listener.setblocking(False)
        self.stopped = threading.Event()
        self.lock = threading.Lock()
        self.value = "41"
        self.version = 0
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def publish(self, value):
        with self.lock:
            self.value = value
            self.version += 1

    def run(self):
        clients = {}
        try:
            while not self.stopped.is_set():
                readable, _, _ = select.select([self.listener, *clients], [], [], 0.05)
                for item in readable:
                    if item is self.listener:
                        try:
                            client, _ = self.listener.accept()
                            client.settimeout(0.2)
                            clients[client] = -1
                        except BlockingIOError:
                            pass
                    else:
                        try:
                            if not item.recv(8192):
                                item.close()
                                clients.pop(item, None)
                        except (OSError, socket.timeout):
                            item.close()
                            clients.pop(item, None)
                with self.lock:
                    value, version = self.value, self.version
                for client, sent in list(clients.items()):
                    if sent == version:
                        continue
                    try:
                        client.sendall((value + "\n").encode())
                        clients[client] = version
                    except OSError:
                        client.close()
                        clients.pop(client, None)
        finally:
            for client in clients:
                client.close()

    def close(self):
        self.stopped.set()
        self.thread.join(timeout=2)
        self.listener.close()


class Smoke:
    def __init__(self, binary, directory, display):
        self.binary = binary
        self.directory = directory
        self.config_dir = directory / "config" / "cybar"
        self.config_dir.mkdir(parents=True)
        (self.config_dir / "nested").mkdir()
        self.config = self.config_dir / "config.lua"
        self.helper = self.config_dir / "helper.lua"
        self.lazy = self.config_dir / "nested" / "lazy.lua"
        self.file_source = directory / "status.txt"
        self.pids = directory / "children.txt"
        self.log_path = directory / "daemon.log"
        self.runtime = directory / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.env = dict(os.environ)
        self.env.update(XDG_RUNTIME_DIR=str(self.runtime), XDG_CONFIG_HOME=str(directory / "config"))
        self.ipc = self.runtime / "cybar" / ("display-" + display.encode().hex() + ".sock")
        self.stream = StreamServer(directory / "source.sock")
        self.daemon = None
        self.log_file = None
        self.passed = 0
        self.last = "preparing fixtures"
        self.helper.write_text("return {value='helper-one'}\n")
        self.lazy.write_text("return {value='lazy-one'}\n")
        self.file_source.write_text("file-a\n")

    def check(self, label, condition, detail=""):
        self.last = label
        if not condition:
            raise AssertionError(f"{label}: {detail}")
        self.passed += 1

    def cli(self, *args, timeout=5):
        return subprocess.run([str(self.binary), *args], env=self.env, capture_output=True,
                              text=True, timeout=timeout)

    def successful(self, label, *args):
        result = self.cli(*args)
        self.check(label, result.returncode == 0, result.stderr)
        return result

    def failed(self, label, *args, contains=None):
        result = self.cli(*args)
        self.check(label, result.returncode != 0 and (contains is None or contains in result.stderr),
                   f"exit={result.returncode}, stderr={result.stderr!r}")
        return result

    def status(self):
        result = self.cli("request", "probe", "status")
        if result.returncode:
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)

    def wait(self, label, predicate, timeout=5, allow_exit=False):
        self.last = label
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            if not allow_exit and self.daemon is not None and self.daemon.poll() is not None:
                raise AssertionError(f"{label}: daemon exited {self.daemon.returncode}")
            try:
                last = predicate()
                if last:
                    self.passed += 1
                    return last
            except (RuntimeError, ValueError, OSError, subprocess.TimeoutExpired) as error:
                last = str(error)
            time.sleep(0.04)
        raise AssertionError(f"{label}: timed out; last={last!r}")

    def logs(self):
        return self.log_path.read_text(errors="replace") if self.log_path.exists() else ""

    @staticmethod
    def lua_string(value):
        return json.dumps(str(value), ensure_ascii=False)

    def configuration(self, version, auto=False, clock_format="%H:%M:%-S"):
        stream_command = (f"printf '%s\\n' \"$$\" >> {shlex.quote(str(self.pids))}; "
                          "while :; do printf 'stream-ready\\n'; sleep 0.15; done")
        return """
local b = require('cybar')
local helper = require('helper')
local version = VERSION
local ticks, cancelled = 0, 0
local after_done, nested_done, aux_closed = false, false, false
local spawn_out, spawn_code, spawn_err
local file_seen, command_seen, stream_seen, socket_seen, aux_seen, clock_seen
local cpu_seen, ram_used, ram_total
local note, lazy_value = 'initial', nil
b.every(0.05, function() ticks = ticks + 1 end)
b.after(0.03, function()
  after_done = true
  b.after(0.03, function() nested_done = true end)
end)
local cancellation = b.after(0.04, function() cancelled = 99 end)
cancellation:cancel()
b.after(0.12, function() error('deliberate timer failure') end)
b.spawn("printf spawn-ready; printf spawn-note >&2; exit 3", function(out, code, err)
  spawn_out, spawn_code, spawn_err = out, code, err
  b.after(0.01, function() note = 'spawn-finished' end)
end)
local aux = b.socket.connect(SOCKET_PATH, {
  on_data = function(sock, bytes) aux_seen = bytes; sock:send('ack') end,
  on_close = function() aux_closed = true end,
})
local probe = b.custom {
  name = 'probe', source = {type='poll', interval=0.05, poll=function() return version end},
  render = function(data) return {text=data, icon=false} end,
  on_message = function(handle, text)
    if text == 'status' then
      return {version=version, helper=helper.value, ticks=ticks, cancelled=cancelled,
        after=after_done, nested=nested_done, spawn_out=spawn_out, spawn_code=spawn_code,
        spawn_err=spawn_err, file=file_seen, command=command_seen, stream=stream_seen,
        socket=socket_seen, aux=aux_seen, aux_closed=aux_closed, clock=clock_seen,
        cpu=cpu_seen, ram_used=ram_used, ram_total=ram_total, note=note, lazy=lazy_value}
    elseif text == 'explode' then error('deliberate message failure')
    elseif text == 'slow' then
      local start = os.clock(); while os.clock()-start < 0.15 do end
      return 'finished slowly'
    elseif text == 'lazy' then lazy_value=require('nested.lazy').value; return lazy_value
    elseif text == 'close-aux' then aux:close(); return true
    elseif text == 'aux-fd' then return aux:fd()
    elseif string.sub(text,1,4) == 'set:' then note=string.sub(text,5); return note
    end
    return 'unknown'
  end,
}
return {
  auto_reload=AUTO, reload_debounce_ms=100, watchdog_ms=30, scale=1,
  bars={
    {name='smoke-main', monitor='primary', position='top', size={thickness=32,length='100%'},
      sections={left={probe,
        b.custom{name='file', source={type='stream', file=FILE_PATH, parse=function(v)
          if v=='bad' then error('deliberate parse failure') end; return v end},
          render=function(v) file_seen=v; return {text='file:'..tostring(v),icon=false,
            style={foreground='#cdd6f4'}} end},
        b.custom{name='command', source={type='command',command='printf 7',interval=0.2,
          parse=function(v) return tonumber(v) end},
          render=function(v) command_seen=v; return tostring(v) end},
        b.custom{name='stream', source={type='stream',command=STREAM_COMMAND,interval=0.2,
          parse=function(v) return string.upper(v) end},
          render=function(v) stream_seen=v; return v end},
        b.custom{name='socket', source={type='stream',socket=SOCKET_PATH,
          parse=function(v) return tonumber(v) end},
          render=function(v) socket_seen=v; return tostring(v) end},
        b.custom{name='broken',source={type='poll',interval=0.3,
          poll=function() error('deliberate poll failure') end},
          render=function(v) return tostring(v) end},
      }, middle={b.clock{name='clock',format=CLOCK_FORMAT,render=function(v)
          clock_seen=v.text; return v.text end}},
      right={b.cpu{name='cpu',interval=0.2,render=function(v) cpu_seen=v.percent; return v.text end},
        b.ram{name='ram',interval=0.2,render=function(v) ram_used=v.used;ram_total=v.total;return v.text end},
        b.systray{name='tray'}},}},
    {name='smoke-secondary',monitor='primary',position='bottom',size={thickness=24,length='40%'},
      sections={middle={b.clock{name='secondary-clock',format='%H:%M'}}}},
  },
}
""".replace("VERSION", self.lua_string(version)).replace("AUTO", "true" if auto else "false") \
            .replace("SOCKET_PATH", self.lua_string(self.stream.path)) \
            .replace("FILE_PATH", self.lua_string(self.file_source)) \
            .replace("STREAM_COMMAND", self.lua_string(stream_command)) \
            .replace("CLOCK_FORMAT", self.lua_string(clock_format))

    def write_config(self, version, auto=False, clock_format="%H:%M:%-S"):
        temporary = self.config.with_suffix(".tmp")
        temporary.write_text(self.configuration(version, auto, clock_format))
        temporary.replace(self.config)

    def wire(self, envelopes, half_close=False, partial=False):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(3)
            client.connect(str(self.ipc))
            payload = b"".join(json.dumps(e).encode() + b"\n" for e in envelopes)
            if partial:
                split = len(payload) // 2
                client.sendall(payload[:split])
                time.sleep(0.025)
                client.sendall(payload[split:])
            else:
                client.sendall(payload)
            if half_close:
                client.shutdown(socket.SHUT_WR)
            data = b""
            while data.count(b"\n") < len(envelopes):
                chunk = client.recv(8192)
                if not chunk:
                    raise AssertionError("IPC connection closed before all replies")
                data += chunk
            return [json.loads(line) for line in data.splitlines()]

    def execute(self):
        invalid = self.config_dir / "invalid.lua"
        invalid.write_text("return {bars=\n")
        self.failed("invalid Lua startup exits nonzero", "--config", str(invalid))
        invalid.write_text(self.configuration("invalid-clock", clock_format="%Q"))
        self.failed("invalid clock is rejected by check", "--config", str(invalid), "check", contains="clock format")
        self.failed("invalid clock startup exits nonzero", "--config", str(invalid), contains="clock format")
        self.write_config("v1")
        self.successful("valid configuration check", "check")
        self.log_file = self.log_path.open("w")
        self.daemon = subprocess.Popen([str(self.binary)], env=self.env, stdout=subprocess.DEVNULL,
                                       stderr=self.log_file)
        self.wait("daemon and private IPC become ready", lambda: self.ipc.exists() and self.status().get("version") == "v1")
        self.check("runtime directory and socket have private modes",
                   stat.S_IMODE(self.ipc.parent.stat().st_mode) == 0o700 and
                   stat.S_IMODE(self.ipc.stat().st_mode) == 0o600)
        self.failed("duplicate daemon is refused", contains="already running")
        self.wait("timer cancellation and nested timer callback", lambda: (lambda s:
                  s.get("ticks", 0) >= 3 and s.get("after") and s.get("nested") and s.get("cancelled") == 0)(self.status()))
        self.wait("spawn stdout, stderr and exit code", lambda: (lambda s:
                  s.get("spawn_out") == "spawn-ready" and s.get("spawn_code") == 3 and
                  s.get("spawn_err") == "spawn-note" and s.get("note") == "spawn-finished")(self.status()))
        self.wait("file, parsed command and parsed stream sources", lambda: (lambda s:
                  s.get("file") == "file-a" and s.get("command") == 7 and
                  s.get("stream") == "STREAM-READY")(self.status()))
        self.wait("socket source and reentrant socket data callback", lambda: (lambda s:
                  s.get("socket") == 41 and "41" in s.get("aux", ""))(self.status()))
        self.wait("CPU, RAM and clock builtins produce data", lambda: (lambda s:
                  isinstance(s.get("cpu"), (float, int)) and 0 <= s["cpu"] <= 100 and
                  0 < s.get("ram_used", 0) <= s.get("ram_total", 0) and
                  bool(re.fullmatch(r"\d\d:\d\d:\d{1,2}", s.get("clock", ""))))(self.status()))
        self.check("required helper is loaded", self.status().get("helper") == "helper-one")
        self.wait("timer and poll callback errors are logged without stopping daemon", lambda:
                  "deliberate timer failure" in self.logs() and "deliberate poll failure" in self.logs())
        self.failed("missing request target is rejected", "request", "absent", "status", contains="does not exist")
        self.failed("missing message target is rejected", "msg", "absent", "status", contains="does not exist")
        self.failed("message callback error reaches request client", "request", "probe", "explode", contains="deliberate message failure")
        self.check("daemon remains responsive after callback error", self.status().get("version") == "v1")
        request = {"id": 17, "type": "request", "method": "module.message", "params": {"module": "probe", "text": "status"}}
        reply = self.wire([request], partial=True)[0]
        self.check("partial JSON Lines framing", reply.get("id") == 17 and reply.get("result", {}).get("version") == "v1", reply)
        reply = self.wire([request], half_close=True)[0]
        self.check("half-closed request receives reply", reply.get("id") == 17 and reply.get("type") == "reply", reply)
        second = dict(request, id="second")
        replies = self.wire([request, second])
        self.check("pipelined requests retain symmetric IDs", {r.get("id") for r in replies} == {17, "second"}, replies)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(3)
            client.connect(str(self.ipc))
            client.sendall(b"not-json\n" + json.dumps(request).encode() + b"\n")
            reply = json.loads(client.makefile("rb").readline())
            self.check("malformed IPC line does not poison next request", reply.get("result", {}).get("version") == "v1", reply)
        self.failed("client request timeout is honored", "--timeout", "30", "request", "probe", "slow", contains="timed out")
        self.wait("slow callback watchdog logs and daemon recovers", lambda:
                  "Lua callback ran for" in self.logs() and self.status().get("version") == "v1")
        self.successful("CLI fire-and-forget msg succeeds", "msg", "probe", "set:cli-note")
        self.wait("fire-and-forget message reaches handler", lambda: self.status().get("note") == "cli-note")
        self.file_source.write_text("file-b\n")
        self.wait("inotify updates regular file source", lambda: self.status().get("file") == "file-b")
        self.file_source.write_text("bad\n")
        self.wait("parser error keeps previous data", lambda:
                  "deliberate parse failure" in self.logs() and self.status().get("file") == "file-b")
        self.file_source.write_text("file-b\n")
        self.stream.publish("42")
        self.wait("socket publishes another event", lambda: self.status().get("socket") == 42)
        self.successful("socket can close from Lua", "request", "probe", "close-aux")
        self.wait("socket close callback is dispatched", lambda: self.status().get("aux_closed"))
        result = self.successful("closed socket fd remains queryable", "request", "probe", "aux-fd")
        self.check("closed socket fd is invalidated", json.loads(result.stdout) == -1, result.stdout)
        self.successful("targeted bar can hide", "toggle", "smoke-main")
        self.successful("targeted bar can show", "toggle", "smoke-main")
        self.successful("all bars can toggle", "toggle")
        self.successful("all bars can restore", "toggle")
        self.failed("missing toggle target is rejected", "toggle", "absent", contains="does not exist")
        self.config.write_text("return {bars=\n")
        self.failed("invalid Lua reload reports failure", "reload")
        self.check("invalid Lua reload retains old configuration", self.status().get("version") == "v1")
        self.write_config("bad-clock", clock_format="%Q")
        self.failed("invalid clock reload reports failure", "reload", contains="clock format")
        self.check("invalid clock reload retains old configuration", self.status().get("version") == "v1")
        self.config.write_text(self.configuration("bad-offset").replace(
            "position='top',", "position='top',offset={x='2147483647%',y=0},", 1))
        self.failed("out-of-range position reload reports failure", "reload", contains="coordinate range")
        self.check("out-of-range position reload retains old configuration", self.status().get("version") == "v1")
        self.config.write_text(self.configuration("bad-frame").replace(
            "size={thickness=32,length='100%'}", "size={thickness=5000,length=16000}", 1))
        self.failed("oversized framebuffer reload reports failure", "reload", contains="256 MiB")
        self.check("oversized framebuffer reload retains old configuration", self.status().get("version") == "v1")
        self.write_config("v2")
        self.successful("successful CLI reload", "reload")
        self.wait("CLI reload applies new configuration", lambda: self.status().get("version") == "v2")
        self.write_config("v3")
        self.daemon.send_signal(signal.SIGUSR1)
        self.wait("SIGUSR1 reload applies new configuration", lambda: self.status().get("version") == "v3")
        self.write_config("v4", auto=True)
        self.successful("enable automatic reload", "reload")
        self.wait("automatic reload configuration becomes active", lambda: self.status().get("version") == "v4")
        before = self.logs().count("configuration reloaded")
        self.write_config("intermediate", auto=True)
        time.sleep(0.025)
        self.write_config("v5", auto=True)
        self.wait("root config inotify debounce applies final contents", lambda: self.status().get("version") == "v5")
        time.sleep(0.25)
        self.check("rapid writes coalesce into one automatic reload",
                   self.logs().count("configuration reloaded") == before + 1, self.logs()[-2000:])
        self.helper.write_text("return {value='helper-two'}\n")
        self.wait("required Lua helper change reloads config", lambda: self.status().get("helper") == "helper-two")
        result = self.successful("lazy helper is loaded from handler", "request", "probe", "lazy")
        self.check("lazy helper initial value", result.stdout.strip() == "lazy-one", result.stdout)
        time.sleep(0.2)
        self.lazy.write_text("return {value='lazy-two'}\n")
        self.wait("lazily required Lua file is watched", lambda:
                  self.cli("request", "probe", "lazy").stdout.strip() == "lazy-two")
        self.successful("quit acknowledges clean shutdown", "quit")
        self.daemon.wait(timeout=5)
        self.check("daemon exits successfully", self.daemon.returncode == 0)
        self.check("IPC socket is removed on shutdown", not self.ipc.exists())
        pids = {int(line) for line in self.pids.read_text().splitlines()} if self.pids.exists() else set()
        self.check("long-running stream fixture was exercised", len(pids) >= 2)
        self.wait("stream children are terminated and reaped", lambda: all(not process_exists(pid) for pid in pids), allow_exit=True)
        self.failed("CLI reports absent daemon after quit", "request", "probe", "status", contains="no Cybar instance")

    def close(self):
        if self.daemon is not None and self.daemon.poll() is None:
            self.daemon.terminate()
            try:
                self.daemon.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait(timeout=3)
        if self.log_file is not None:
            self.log_file.close()
        self.stream.close()
        if self.pids.exists():
            for line in self.pids.read_text().splitlines():
                try:
                    pid = int(line)
                    if process_exists(pid):
                        os.killpg(pid, signal.SIGKILL)
                except (ValueError, ProcessLookupError, PermissionError):
                    pass


def process_exists(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def main():
    if len(sys.argv) != 2:
        raise SystemExit("Usage: DISPLAY=:117 python3 scripts/xvfb-smoke.py target/debug/cybar")
    display = os.environ.get("DISPLAY", "")
    match = re.fullmatch(r"(?:localhost)?:([0-9]+)(?:\.[0-9]+)?", display)
    if match is None or int(match.group(1)) == 0:
        raise SystemExit("Provide an isolated local Xvfb DISPLAY such as :117; :0 is refused.")
    binary = Path(sys.argv[1]).resolve()
    if not binary.is_file():
        raise SystemExit(f"Build Cybar first: executable does not exist: {binary}")
    with tempfile.TemporaryDirectory(prefix="cybar-smoke-") as temporary:
        smoke = Smoke(binary, Path(temporary), display)
        try:
            smoke.execute()
            print(f"PASS: {smoke.passed} Cybar/Xvfb regression assertions")
        except Exception as error:
            print(f"FAIL after {smoke.passed} assertions ({smoke.last}): {error}", file=sys.stderr)
            print("Daemon log tail:\n" + smoke.logs()[-12000:], file=sys.stderr)
            return 1
        finally:
            smoke.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
