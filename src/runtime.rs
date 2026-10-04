// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{
    config, ipc,
    model::*,
    renderer::Renderer,
    x11::{BarWindow, Platform, XEvent},
};
use anyhow::{Context, Result, bail};
use mio::{Events, Interest, Poll, Token, unix::SourceFd};
use mlua::{Function, IntoLua, Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value};
use notify::{RecursiveMode, Watcher};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 1024 * 1024;

#[derive(Clone)]
struct Handle {
    inner: Rc<RefCell<ModuleData>>,
    dirty: Rc<Cell<bool>>,
}
struct ModuleData {
    data: Value,
    text: Option<String>,
    icon: Option<Option<String>>,
    style: Style,
    state: ModuleState,
}
impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("set_text", |_, h, text: Option<String>| {
            h.inner.borrow_mut().text = text;
            h.dirty.set(true);
            Ok(())
        });
        m.add_method("set_icon", |_, h, icon: Option<String>| {
            h.inner.borrow_mut().icon = Some(icon);
            h.dirty.set(true);
            Ok(())
        });
        m.add_method("set_state", |_, h, state: String| {
            let state = ModuleState::parse(&state)
                .ok_or_else(|| mlua::Error::external("unknown module state"))?;
            h.inner.borrow_mut().state = state;
            h.dirty.set(true);
            Ok(())
        });
        m.add_method("set_style", |_, h, style: Table| {
            let base = h.inner.borrow().style.clone();
            let parsed = config::parse_style(&style, &base).map_err(mlua::Error::external)?;
            h.inner.borrow_mut().style = parsed;
            h.dirty.set(true);
            Ok(())
        });
        m.add_method("get_data", |_, h, ()| Ok(h.inner.borrow().data.clone()));
        m.add_method("data", |_, h, ()| Ok(h.inner.borrow().data.clone()));
        m.add_method("set_data", |_, h, data: Value| {
            h.inner.borrow_mut().data = data;
            h.dirty.set(true);
            Ok(())
        });
        m.add_method("redraw", |_, h, ()| {
            h.dirty.set(true);
            Ok(())
        });
    }
}

struct Timer {
    at: Instant,
    interval: Option<Duration>,
    callback: Function,
    cancelled: Rc<Cell<bool>>,
}
#[derive(Clone)]
struct Cancel(Rc<Cell<bool>>);
impl UserData for Cancel {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("cancel", |_, s, ()| {
            s.0.set(true);
            Ok(())
        });
    }
}
struct Job {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    output: Vec<u8>,
    errors: Vec<u8>,
    callback: Option<Function>,
    target: Option<usize>,
    stream: bool,
    detached: bool,
}
struct Spawn {
    command: String,
    callback: Option<Function>,
    target: Option<usize>,
    stream: bool,
    detached: bool,
}
#[derive(Clone)]
struct SocketHandle {
    socket: Rc<RefCell<SocketData>>,
}
struct SocketData {
    stream: UnixStream,
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
    handlers: Option<Table>,
    closed: bool,
    notified_close: bool,
}
impl UserData for SocketHandle {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("fd", |_, s, ()| {
            let s = s.socket.borrow();
            Ok(if s.closed { -1 } else { s.stream.as_raw_fd() })
        });
        m.add_method("send", |_, s, data: mlua::String| {
            let mut s = s.socket.borrow_mut();
            if s.closed {
                return Err(mlua::Error::external("socket is closed"));
            }
            if s.outgoing.len() + data.as_bytes().len() > MAX_OUTPUT {
                return Err(mlua::Error::external("socket write queue exceeds 1 MiB"));
            }
            s.outgoing.extend_from_slice(&data.as_bytes());
            Ok(())
        });
        m.add_method("write", |_, s, data: mlua::String| {
            let mut s = s.socket.borrow_mut();
            if s.closed {
                return Err(mlua::Error::external("socket is closed"));
            }
            if s.outgoing.len() + data.as_bytes().len() > MAX_OUTPUT {
                return Err(mlua::Error::external("socket write queue exceeds 1 MiB"));
            }
            s.outgoing.extend_from_slice(&data.as_bytes());
            Ok(())
        });
        m.add_method("read", |lua, s, ()| {
            let mut s = s.socket.borrow_mut();
            if s.incoming.is_empty() {
                return Ok(if s.closed {
                    Value::Nil
                } else {
                    Value::String(lua.create_string("")?)
                });
            }
            let data: Vec<_> = s.incoming.drain(..).collect();
            Ok(Value::String(lua.create_string(data)?))
        });
        m.add_method("close", |_, s, ()| {
            let mut s = s.socket.borrow_mut();
            let _ = s.stream.shutdown(std::net::Shutdown::Both);
            s.closed = true;
            Ok(())
        });
    }
}
fn connect_socket(path: &Path) -> Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        bail!("Unix socket path is too long");
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i] = *b as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&addr) as _,
        )
    };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if !matches!(
            e.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(e.into());
        }
    }
    Ok(stream)
}
fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn duration(n: f64) -> mlua::Result<Duration> {
    if !n.is_finite() || n <= 0.0 || n > 86400.0 * 365.0 {
        return Err(mlua::Error::external(
            "time must be finite, positive, and at most one year",
        ));
    }
    Ok(Duration::from_secs_f64(n))
}

#[derive(Default)]
struct Services {
    pending: Vec<Spawn>,
    jobs: Vec<Job>,
    timers: Vec<Timer>,
    sockets: Vec<SocketHandle>,
}
impl Drop for Services {
    fn drop(&mut self) {
        for job in &mut self.jobs {
            if !job.detached && job.child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(-(job.child.id() as i32), libc::SIGKILL);
                }
                let _ = job.child.wait();
            }
        }
    }
}
fn api(lua: &Lua, services: Rc<RefCell<Services>>) -> Result<Table> {
    let t = lua.create_table()?;
    let s = services.clone();
    t.set(
        "spawn",
        lua.create_function(move |_, (command, callback): (String, Option<Function>)| {
            s.borrow_mut().pending.push(Spawn {
                command,
                callback,
                target: None,
                stream: false,
                detached: false,
            });
            Ok(())
        })?,
    )?;
    let s = services.clone();
    t.set(
        "exec",
        lua.create_function(move |_, command: String| {
            s.borrow_mut().pending.push(Spawn {
                command,
                callback: None,
                target: None,
                stream: false,
                detached: true,
            });
            Ok(())
        })?,
    )?;
    for (name, repeating) in [("after", false), ("every", true)] {
        let s = services.clone();
        t.set(
            name,
            lua.create_function(move |_, (seconds, callback): (f64, Function)| {
                let d = duration(seconds)?;
                let cancelled = Rc::new(Cell::new(false));
                s.borrow_mut().timers.push(Timer {
                    at: Instant::now() + d,
                    interval: if repeating { Some(d) } else { None },
                    callback,
                    cancelled: cancelled.clone(),
                });
                Ok(Cancel(cancelled))
            })?,
        )?;
    }
    let sockets = lua.create_table()?;
    let s = services;
    sockets.set(
        "connect",
        lua.create_function(move |_, (path, handlers): (String, Option<Table>)| {
            let stream =
                connect_socket(&config::expand_path(&path)).map_err(mlua::Error::external)?;
            let h = SocketHandle {
                socket: Rc::new(RefCell::new(SocketData {
                    stream,
                    incoming: VecDeque::new(),
                    outgoing: vec![],
                    handlers,
                    closed: false,
                    notified_close: false,
                })),
            };
            s.borrow_mut().sockets.push(h.clone());
            Ok(h)
        })?,
    )?;
    t.set("socket", sockets)?;
    config::install_constructors(lua, &t)?;
    Ok(t)
}

fn call<A: mlua::IntoLuaMulti, R: mlua::FromLuaMulti>(
    function: &Function,
    args: A,
    watchdog: u64,
) -> Result<R> {
    let start = Instant::now();
    let result = function.call(args).map_err(anyhow::Error::from);
    let elapsed = start.elapsed();
    if elapsed > Duration::from_millis(watchdog) {
        eprintln!(
            "cybar: Lua callback ran for {:.1} ms (threshold {watchdog} ms)",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    result
}
fn handler(spec: &ModuleSpec, name: &str) -> Result<Option<Function>> {
    let v = spec.options.get::<Value>(name)?;
    let v = if matches!(v, Value::Nil) {
        match spec.options.get::<Option<Table>>("handlers")? {
            Some(t) => t.get::<Value>(name)?,
            None => Value::Nil,
        }
    } else {
        v
    };
    match v {
        Value::Function(f) => Ok(Some(f)),
        Value::Boolean(false) | Value::Nil => Ok(None),
        _ => bail!("handler {name} must be a function or false"),
    }
}
fn disabled(spec: &ModuleSpec, name: &str) -> bool {
    matches!(spec.options.get::<Value>(name), Ok(Value::Boolean(false)))
        || spec
            .options
            .get::<Option<Table>>("handlers")
            .ok()
            .flatten()
            .is_some_and(|t| matches!(t.get::<Value>(name), Ok(Value::Boolean(false))))
}

#[derive(Default)]
struct Cpu {
    previous: Option<(u64, u64)>,
}
impl Cpu {
    fn sample(&mut self) -> Result<f64> {
        let s = fs::read_to_string("/proc/stat")?;
        let values: Vec<u64> = s
            .lines()
            .next()
            .context("empty /proc/stat")?
            .split_whitespace()
            .skip(1)
            .take(8)
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        if values.len() < 4 {
            bail!("invalid /proc/stat");
        }
        let total: u64 = values.iter().sum();
        let idle = values[3] + values.get(4).copied().unwrap_or(0);
        let usage = if let Some((pt, pi)) = self.previous {
            let d = total.saturating_sub(pt);
            if d > 0 {
                100.0 * (d.saturating_sub(idle.saturating_sub(pi))) as f64 / d as f64
            } else {
                0.0
            }
        } else {
            0.0
        };
        self.previous = Some((total, idle));
        Ok(usage)
    }
}
fn memory() -> Result<(u64, u64, f64)> {
    let s = fs::read_to_string("/proc/meminfo")?;
    let map: HashMap<&str, u64> = s
        .lines()
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            Some((p.next()?.trim_end_matches(':'), p.next()?.parse().ok()?))
        })
        .collect();
    let total = *map.get("MemTotal").context("MemTotal missing")? * 1024;
    let available = map.get("MemAvailable").copied().unwrap_or_else(|| {
        map.get("MemFree").copied().unwrap_or(0)
            + map.get("Buffers").copied().unwrap_or(0)
            + map.get("Cached").copied().unwrap_or(0)
    }) * 1024;
    let used = total.saturating_sub(available);
    Ok((
        used,
        total,
        if total > 0 {
            used as f64 * 100.0 / total as f64
        } else {
            0.0
        },
    ))
}

enum Source {
    Builtin,
    Poll(Function),
    Command(String),
    StreamCommand(String),
    StreamFile {
        path: PathBuf,
        stamp: Option<(std::time::SystemTime, u64)>,
    },
    StreamSocket {
        handle: SocketHandle,
        buffer: Vec<u8>,
    },
    None,
}
struct Module {
    spec: ModuleSpec,
    handle: Handle,
    source: Source,
    next: Instant,
    interval: Duration,
    busy: bool,
    cpu: Cpu,
    scroll: f64,
    scroll_due: Option<Instant>,
    hovered: bool,
}
struct LiveBar {
    spec: BarSpec,
    monitor: Monitor,
    window: BarWindow,
    sections: Vec<Vec<usize>>,
    frame: Option<Frame>,
    visible: bool,
}
struct Backend {
    conn: Table,
    fd: i32,
}
struct Engine {
    lua: Lua,
    config: Config,
    services: Rc<RefCell<Services>>,
    modules: Vec<Module>,
    bars: Vec<LiveBar>,
    dirty: Rc<Cell<bool>>,
    backend: Option<Backend>,
    hover: Option<usize>,
    path: PathBuf,
    watched: Rc<RefCell<HashSet<PathBuf>>>,
    tray_window: Option<u32>,
    tray_module: Option<usize>,
    source_watch: Option<Watches>,
}

fn track_require(lua: &Lua, watched: Rc<RefCell<HashSet<PathBuf>>>) -> Result<()> {
    let original: Function = lua.globals().get("require")?;
    lua.globals().set(
        "require",
        lua.create_function(move |lua, name: String| {
            let package: Table = lua.globals().get("package")?;
            let path: String = package.get("path")?;
            let name_path = name.replace('.', "/");
            for template in path.split(';') {
                let p = PathBuf::from(template.replace('?', &name_path));
                if p.is_file() {
                    watched
                        .borrow_mut()
                        .insert(fs::canonicalize(&p).unwrap_or(p));
                    break;
                }
            }
            original.call::<Value>(name)
        })?,
    )?;
    Ok(())
}
impl Engine {
    fn load(path: &Path) -> Result<Self> {
        let lua = Lua::new();
        let services = Rc::new(RefCell::new(Services::default()));
        let _ = api(&lua, services.clone())?;
        let watched = Rc::new(RefCell::new(HashSet::new()));
        watched
            .borrow_mut()
            .insert(fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()));
        track_require(&lua, watched.clone())?;
        let config = config::load(&lua, path)?;
        Ok(Self {
            lua,
            config,
            services,
            modules: vec![],
            bars: vec![],
            dirty: Rc::new(Cell::new(true)),
            backend: None,
            hover: None,
            path: path.to_owned(),
            watched,
            tray_window: None,
            tray_module: None,
            source_watch: None,
        })
    }
    fn prepare(&mut self, platform: &mut Platform) -> Result<()> {
        let monitors = platform.monitors()?;
        if monitors.is_empty() {
            bail!("no active X11 monitors");
        }
        let result = (|| {
            for base in self.config.bars.clone() {
                for monitor in &monitors {
                    if base.monitor != "all"
                        && base.monitor != monitor.name
                        && !(base.monitor == "primary" && monitor.primary)
                    {
                        continue;
                    }
                    let spec = config::for_monitor(&self.lua, &base, monitor)?;
                    let mut sections = vec![];
                    for section in &spec.sections {
                        let mut indices = vec![];
                        for module in &section.modules {
                            let index = self.modules.len();
                            self.modules.push(self.module(module.clone())?);
                            indices.push(index);
                        }
                        sections.push(indices);
                    }
                    let window = platform.create_bar(&spec, monitor, self.config.scale)?;
                    self.bars.push(LiveBar {
                        spec,
                        monitor: monitor.clone(),
                        window,
                        sections,
                        frame: None,
                        visible: true,
                    });
                }
            }
            let paths = self
                .modules
                .iter()
                .filter_map(|m| {
                    if let Source::StreamFile { path, .. } = &m.source {
                        Some(path.clone())
                    } else {
                        None
                    }
                })
                .collect::<HashSet<_>>();
            if !paths.is_empty() {
                self.source_watch = match Watches::for_paths(paths) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        eprintln!(
                            "cybar: file source watcher unavailable; falling back to polling: {e:#}"
                        );
                        None
                    }
                };
            }
            Ok(())
        })();
        if result.is_err() {
            self.destroy(platform);
        }
        result
    }
    fn module(&self, spec: ModuleSpec) -> Result<Module> {
        let source = if let Some(source) = spec.options.get::<Option<Table>>("source")? {
            let kind = source
                .get::<Option<String>>("type")?
                .unwrap_or_else(|| "poll".into());
            match kind.as_str() {
                "poll" => Source::Poll(
                    source
                        .get::<Option<Function>>("poll")?
                        .or(source.get::<Option<Function>>("fn")?)
                        .or(source.get::<Option<Function>>("callback")?)
                        .context("poll source requires poll function")?,
                ),
                "command" => Source::Command(source.get::<String>("command")?),
                "stream" => {
                    if let Some(command) = source.get::<Option<String>>("command")? {
                        Source::StreamCommand(command)
                    } else if let Some(path) = source.get::<Option<String>>("file")? {
                        Source::StreamFile {
                            path: config::expand_path(&path),
                            stamp: None,
                        }
                    } else {
                        let path = source.get::<String>("socket")?;
                        let socket = SocketHandle {
                            socket: Rc::new(RefCell::new(SocketData {
                                stream: connect_socket(&config::expand_path(&path))?,
                                incoming: VecDeque::new(),
                                outgoing: vec![],
                                handlers: None,
                                closed: false,
                                notified_close: false,
                            })),
                        };
                        self.services.borrow_mut().sockets.push(socket.clone());
                        Source::StreamSocket {
                            handle: socket,
                            buffer: vec![],
                        }
                    }
                }
                _ => bail!("unknown source type {kind}"),
            }
        } else if spec.kind == ModuleKind::Custom {
            Source::None
        } else {
            Source::Builtin
        };
        let format = spec
            .options
            .get::<Option<String>>("format")?
            .unwrap_or_default();
        let default = if spec.kind == ModuleKind::Clock && !clock_needs_seconds(&format) {
            60.0
        } else {
            1.0
        };
        let interval = spec
            .options
            .get::<Option<Table>>("source")?
            .map(|s| s.get::<Option<f64>>("interval"))
            .transpose()?
            .flatten()
            .or(spec.options.get::<Option<f64>>("interval")?)
            .unwrap_or(default);
        let handle = Handle {
            inner: Rc::new(RefCell::new(ModuleData {
                data: Value::Nil,
                text: None,
                icon: None,
                style: spec.style.clone(),
                state: ModuleState::Normal,
            })),
            dirty: self.dirty.clone(),
        };
        Ok(Module {
            spec,
            handle,
            source,
            next: Instant::now(),
            interval: duration(interval).map_err(anyhow::Error::from)?,
            busy: false,
            cpu: Cpu::default(),
            scroll: 0.0,
            scroll_due: None,
            hovered: false,
        })
    }
    fn start_backend(&mut self) -> Result<()> {
        if let Some(wm) = &self.config.wm {
            if let Some(backend) = wm.get::<Option<Table>>("backend")? {
                let connect: Function = backend.get("connect")?;
                let opts = wm
                    .get::<Option<Table>>("opts")?
                    .unwrap_or(self.lua.create_table()?);
                let conn: Table = call(&connect, opts, self.config.watchdog_ms)?;
                let fd: i32 = call(
                    &conn.get::<Function>("fd")?,
                    conn.clone(),
                    self.config.watchdog_ms,
                )?;
                if fd < 0 {
                    bail!("backend fd() returned an invalid descriptor");
                }
                for method in ["get_workspaces", "get_active_window", "command", "main"] {
                    let _: Function = conn
                        .get(method)
                        .with_context(|| format!("backend requires {method}()"))?;
                }
                self.backend = Some(Backend { conn, fd });
            }
        }
        if let Some(b) = &self.backend {
            let subscribe = b
                .conn
                .get::<Option<Function>>("subscribe")?
                .or(b.conn.get::<Option<Function>>("on")?)
                .context("backend requires subscribe() or on()")?;
            for event in ["workspace_changed", "window_title_changed"] {
                let dirty = self.dirty.clone();
                let callback = self.lua.create_function(move |_, _: mlua::MultiValue| {
                    dirty.set(true);
                    Ok(())
                })?;
                let _: Value = call(
                    &subscribe,
                    (b.conn.clone(), event, callback),
                    self.config.watchdog_ms,
                )?;
            }
        }
        Ok(())
    }
    fn activate_tray(&mut self, platform: &mut Platform) {
        if let Some(index) = self.bars.iter().position(|b| {
            b.spec
                .sections
                .iter()
                .flat_map(|s| &s.modules)
                .any(|m| m.kind == ModuleKind::Systray)
                && (self.config.tray_monitor == "all"
                    || self.config.tray_monitor == b.monitor.name
                    || (self.config.tray_monitor == "primary" && b.monitor.primary)
                    || self.config.tray_monitor == b.spec.name)
        }) {
            let b = &self.bars[index];
            let size = b
                .spec
                .sections
                .iter()
                .flat_map(|s| &s.modules)
                .find(|m| m.kind == ModuleKind::Systray)
                .map(|m| (m.style.icon_size as f64 * b.window.scale).round() as u32)
                .unwrap_or(16);
            match platform.claim_tray(b.window.window, b.spec.position.vertical(), size) {
                Ok(()) => {
                    self.tray_window = Some(b.window.window);
                    self.tray_module = b
                        .sections
                        .iter()
                        .flatten()
                        .copied()
                        .find(|i| self.modules[*i].spec.kind == ModuleKind::Systray);
                }
                Err(e) => eprintln!("cybar: system tray unavailable: {e:#}"),
            }
        }
    }
    fn destroy(&mut self, platform: &mut Platform) {
        for b in self.bars.drain(..) {
            let _ = platform.destroy_bar(b.window.window);
        }
        self.modules.clear();
    }
    fn fds(&self) -> Vec<(i32, bool)> {
        let s = self.services.borrow();
        let mut fds = vec![];
        for j in &s.jobs {
            if let Some(p) = &j.stdout {
                fds.push((p.as_raw_fd(), false));
            }
            if let Some(p) = &j.stderr {
                fds.push((p.as_raw_fd(), false));
            }
        }
        for h in &s.sockets {
            let socket = h.socket.borrow();
            if !socket.closed {
                fds.push((socket.stream.as_raw_fd(), !socket.outgoing.is_empty()));
            }
        }
        if let Some(b) = &self.backend {
            fds.push((b.fd, false));
        }
        fds
    }
    fn delay(&self) -> Duration {
        let now = Instant::now();
        let mut next = now + Duration::from_secs(1);
        for m in &self.modules {
            if !matches!(m.source, Source::None) {
                next = next.min(m.next);
            }
            if let Some(d) = m.scroll_due {
                next = next.min(d);
            }
        }
        let s = self.services.borrow();
        for t in &s.timers {
            next = next.min(t.at);
        }
        if !s.jobs.is_empty() {
            next = next.min(now + Duration::from_millis(100));
        }
        next.saturating_duration_since(now)
    }
    fn set_data(&self, index: usize, value: Value) {
        if let Some(m) = self.modules.get(index) {
            let value = if let Some(parse) = m
                .spec
                .options
                .get::<Option<Table>>("source")
                .ok()
                .flatten()
                .and_then(|source| source.get::<Option<Function>>("parse").ok().flatten())
            {
                match call(&parse, (value, m.handle.clone()), self.config.watchdog_ms) {
                    Ok(value) => value,
                    Err(e) => {
                        eprintln!("cybar: module {} source.parse: {e:#}", m.spec.name);
                        return;
                    }
                }
            } else {
                value
            };
            let previous = m.handle.inner.borrow().data.clone();
            if previous == value {
                return;
            }
            if matches!(previous, Value::Table(_)) && matches!(value, Value::Table(_)) {
                if let (Ok(a), Ok(b)) = (
                    self.lua.from_value::<serde_json::Value>(previous),
                    self.lua.from_value::<serde_json::Value>(value.clone()),
                ) {
                    if a == b {
                        return;
                    }
                }
            }
            m.handle.inner.borrow_mut().data = value;
            m.handle.dirty.set(true);
        }
    }
    fn launch(&mut self) {
        let pending = std::mem::take(&mut self.services.borrow_mut().pending);
        for p in pending {
            let mut command = Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(&p.command)
                .stdin(Stdio::null())
                .stdout(if p.detached {
                    Stdio::null()
                } else {
                    Stdio::piped()
                })
                .stderr(if p.detached {
                    Stdio::null()
                } else {
                    Stdio::piped()
                });
            command.process_group(0);
            match command.spawn() {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    if let Some(s) = &stdout {
                        let _ = nonblocking(s.as_raw_fd());
                    }
                    if let Some(s) = &stderr {
                        let _ = nonblocking(s.as_raw_fd());
                    }
                    self.services.borrow_mut().jobs.push(Job {
                        child,
                        stdout,
                        stderr,
                        output: vec![],
                        errors: vec![],
                        callback: p.callback,
                        target: p.target,
                        stream: p.stream,
                        detached: p.detached,
                    });
                }
                Err(e) => {
                    eprintln!("cybar: cannot spawn command: {e}");
                    if let Some(i) = p.target {
                        self.modules[i].busy = false;
                    }
                    if let Some(f) = p.callback {
                        let _ =
                            call::<_, Value>(&f, ("", -1, e.to_string()), self.config.watchdog_ms);
                    }
                }
            }
        }
    }
    fn services(&mut self) -> Result<()> {
        self.launch();
        let mut completed = vec![];
        let mut lines = vec![];
        let mut callbacks: Vec<(Function, String, i32, String)> = vec![];
        {
            let mut s = self.services.borrow_mut();
            for (i, j) in s.jobs.iter_mut().enumerate() {
                let mut buf = [0u8; 8192];
                if let Some(out) = &mut j.stdout {
                    loop {
                        match out.read(&mut buf) {
                            Ok(0) => {
                                j.stdout = None;
                                break;
                            }
                            Ok(n) => {
                                j.output.extend_from_slice(&buf[..n]);
                                if j.output.len() > MAX_OUTPUT {
                                    eprintln!(
                                        "cybar: command output exceeded 1 MiB; terminating command"
                                    );
                                    unsafe {
                                        libc::kill(-(j.child.id() as i32), libc::SIGKILL);
                                    }
                                    j.output.truncate(MAX_OUTPUT);
                                    break;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                j.stdout = None;
                                break;
                            }
                        }
                    }
                }
                if let Some(err) = &mut j.stderr {
                    let mut consumed = 0;
                    loop {
                        if consumed >= 65536 {
                            break;
                        }
                        match err.read(&mut buf) {
                            Ok(0) => {
                                j.stderr = None;
                                break;
                            }
                            Ok(n) => {
                                consumed += n;
                                let left = MAX_OUTPUT.saturating_sub(j.errors.len());
                                j.errors.extend_from_slice(&buf[..n.min(left)]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                j.stderr = None;
                                break;
                            }
                        }
                    }
                }
                if j.stream {
                    while let Some(end) = j.output.iter().position(|b| *b == b'\n') {
                        let line: Vec<_> = j.output.drain(..=end).collect();
                        if let Some(target) = j.target {
                            lines.push((
                                target,
                                String::from_utf8_lossy(&line)
                                    .trim_end_matches(['\r', '\n'])
                                    .to_string(),
                            ));
                        }
                    }
                }
                if let Some(status) = j.child.try_wait()? {
                    if j.stdout.is_some() || j.stderr.is_some() {
                        continue;
                    }
                    if let Some(f) = j.callback.clone() {
                        callbacks.push((
                            f,
                            String::from_utf8_lossy(&j.output).into_owned(),
                            status.code().unwrap_or(-1),
                            String::from_utf8_lossy(&j.errors).into_owned(),
                        ));
                    }
                    if let Some(target) = j.target {
                        if !j.stream || !j.output.is_empty() {
                            lines.push((
                                target,
                                String::from_utf8_lossy(&j.output)
                                    .trim_end_matches(['\r', '\n'])
                                    .to_string(),
                            ));
                        }
                        self.modules[target].busy = false;
                        if !status.success() {
                            eprintln!(
                                "cybar: module {} command exited {}: {}",
                                self.modules[target].spec.name,
                                status,
                                String::from_utf8_lossy(&j.errors)
                            );
                        }
                    }
                    completed.push(i);
                }
            }
            for i in completed.into_iter().rev() {
                s.jobs.remove(i);
            }
        }
        for (target, line) in lines {
            self.set_data(target, Value::String(self.lua.create_string(line)?));
        }
        for (f, out, code, err) in callbacks {
            if let Err(e) = call::<_, Value>(&f, (out, code, err), self.config.watchdog_ms) {
                eprintln!("cybar: spawn callback: {e:#}");
            }
        }
        let now = Instant::now();
        let mut timers = vec![];
        {
            let mut s = self.services.borrow_mut();
            s.timers.retain_mut(|t| {
                if t.cancelled.get() {
                    return false;
                }
                if now >= t.at {
                    timers.push(t.callback.clone());
                    if let Some(d) = t.interval {
                        t.at = now + d;
                        return true;
                    }
                    return false;
                }
                true
            });
        }
        for f in timers {
            if let Err(e) = call::<_, Value>(&f, (), self.config.watchdog_ms) {
                eprintln!("cybar: timer callback: {e:#}");
            }
        }
        let sockets = self.services.borrow().sockets.clone();
        let mut socket_callbacks = vec![];
        for h in sockets {
            let mut s = h.socket.borrow_mut();
            if !s.closed {
                while !s.outgoing.is_empty() {
                    let bytes = s.outgoing.clone();
                    match s.stream.write(&bytes) {
                        Ok(0) => {
                            s.closed = true;
                            break;
                        }
                        Ok(n) => {
                            s.outgoing.drain(..n);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            s.closed = true;
                            break;
                        }
                    }
                }
                let mut bytes = vec![];
                let mut buf = [0u8; 8192];
                loop {
                    match s.stream.read(&mut buf) {
                        Ok(0) => {
                            s.closed = true;
                            break;
                        }
                        Ok(n) => {
                            bytes.extend_from_slice(&buf[..n]);
                            if bytes.len() > MAX_OUTPUT {
                                s.closed = true;
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            s.closed = true;
                            break;
                        }
                    }
                }
                if !bytes.is_empty() {
                    if let Some(f) = s
                        .handlers
                        .as_ref()
                        .and_then(|t| t.get::<Option<Function>>("on_data").ok().flatten())
                    {
                        socket_callbacks.push((f, h.clone(), Some(bytes)));
                    } else {
                        if s.incoming.len() + bytes.len() > MAX_OUTPUT {
                            s.closed = true;
                        } else {
                            s.incoming.extend(bytes);
                        }
                    }
                }
            }
            if s.closed && !s.notified_close {
                s.notified_close = true;
                if let Some(f) = s
                    .handlers
                    .as_ref()
                    .and_then(|t| t.get::<Option<Function>>("on_close").ok().flatten())
                {
                    socket_callbacks.push((f, h.clone(), None));
                }
            }
        }
        for (f, h, bytes) in socket_callbacks {
            let r = if let Some(bytes) = bytes {
                call::<_, Value>(
                    &f,
                    (h, self.lua.create_string(bytes)?),
                    self.config.watchdog_ms,
                )
            } else {
                call::<_, Value>(&f, h, self.config.watchdog_ms)
            };
            if let Err(e) = r {
                eprintln!("cybar: socket callback: {e:#}");
            }
        }
        self.services.borrow_mut().sockets.retain(|h| {
            let s = h.socket.borrow();
            !s.closed || !s.notified_close
        });
        Ok(())
    }
}

impl Engine {
    fn workspaces(&self, platform: &Platform) -> Result<Vec<Workspace>> {
        if let Some(b) = &self.backend {
            let value: Value = call(
                &b.conn.get::<Function>("get_workspaces")?,
                b.conn.clone(),
                self.config.watchdog_ms,
            )?;
            Ok(self.lua.from_value(value)?)
        } else {
            platform.workspaces()
        }
    }
    fn active_window(&self, platform: &Platform) -> Result<Option<ActiveWindow>> {
        if let Some(b) = &self.backend {
            let value: Value = call(
                &b.conn.get::<Function>("get_active_window")?,
                b.conn.clone(),
                self.config.watchdog_ms,
            )?;
            Ok(self.lua.from_value(value)?)
        } else {
            platform.active_window()
        }
    }
    fn switch_workspace(&self, platform: &Platform, id: u32) -> Result<()> {
        if let Some(b) = &self.backend {
            let (ok, err): (bool, Option<String>) = call(
                &b.conn.get::<Function>("command")?,
                (b.conn.clone(), "switch_workspace", id),
                self.config.watchdog_ms,
            )?;
            if !ok {
                bail!("{}", err.unwrap_or_else(|| "backend command failed".into()));
            }
            Ok(())
        } else {
            platform.switch_workspace(id)
        }
    }
    fn tick(&mut self, platform: &Platform) -> Result<()> {
        let mut backend_hup = false;
        let backend_ready = self.backend.as_mut().is_some_and(|b| {
            if let Ok(function) = b.conn.get::<Function>("fd") {
                match call::<_, i32>(&function, b.conn.clone(), self.config.watchdog_ms) {
                    Ok(fd) => b.fd = fd,
                    Err(e) => {
                        eprintln!("cybar: backend fd(): {e:#}");
                        b.fd = -1;
                    }
                }
            }
            if b.fd < 0 {
                return false;
            }
            if self.services.borrow().sockets.iter().any(|s| {
                let s = s.socket.borrow();
                s.stream.as_raw_fd() == b.fd && s.closed
            }) {
                b.fd = -1;
                return false;
            }
            let mut fd = libc::pollfd {
                fd: b.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, 0) > 0 };
            backend_hup = fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            ready
        });
        self.services()?;
        if backend_ready {
            if let Some(b) = &self.backend {
                if let Err(e) = b
                    .conn
                    .get::<Function>("main")
                    .map_err(anyhow::Error::from)
                    .and_then(|f| call::<_, Value>(&f, b.conn.clone(), self.config.watchdog_ms))
                {
                    eprintln!("cybar: backend main(): {e:#}");
                }
                self.dirty.set(true);
            }
        }
        if backend_hup {
            if let Some(b) = &mut self.backend {
                b.fd = -1;
            }
        }
        let now = Instant::now();
        if self.source_watch.as_ref().is_some_and(Watches::changed) {
            for m in &mut self.modules {
                if matches!(m.source, Source::StreamFile { .. }) {
                    m.next = now;
                }
            }
        }
        let need_wm = self.dirty.get()
            || self.modules.iter().any(|m| {
                now >= m.next
                    && matches!(
                        m.spec.kind,
                        ModuleKind::Workspaces | ModuleKind::WindowTitle
                    )
            });
        let workspaces = if need_wm {
            match self.workspaces(platform) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("cybar: workspace backend: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let window = if need_wm {
            match self.active_window(platform) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("cybar: window backend: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let mut updates = vec![];
        for (index, m) in self.modules.iter_mut().enumerate() {
            if let Some(due) = m.scroll_due {
                if now >= due {
                    let dy = m.scroll;
                    m.scroll = 0.0;
                    m.scroll_due = None;
                    if let Some(f) = handler(&m.spec, "on_scroll")? {
                        if let Err(e) = call::<_, Value>(
                            &f,
                            (m.handle.clone(), 0.0, dy),
                            self.config.watchdog_ms,
                        ) {
                            eprintln!("cybar: scroll callback: {e:#}");
                        }
                    }
                }
            }
            if let Source::StreamSocket { handle, buffer } = &mut m.source {
                let mut socket = handle.socket.borrow_mut();
                buffer.extend(socket.incoming.drain(..));
                while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                    let bytes: Vec<_> = buffer.drain(..=end).collect();
                    updates.push((
                        index,
                        Value::String(self.lua.create_string(
                            String::from_utf8_lossy(&bytes).trim_end_matches(['\r', '\n']),
                        )?),
                    ));
                }
                if socket.closed && !buffer.is_empty() {
                    updates.push((
                        index,
                        Value::String(self.lua.create_string(std::mem::take(buffer))?),
                    ));
                }
            }
            if now < m.next
                && !((m.spec.kind == ModuleKind::Workspaces
                    || m.spec.kind == ModuleKind::WindowTitle)
                    && need_wm)
            {
                continue;
            }
            m.next = now
                + if matches!(m.source, Source::StreamFile { .. }) && self.source_watch.is_some() {
                    Duration::from_secs(3600)
                } else {
                    m.interval
                };
            let result: Result<Option<Value>> = (|| {
                Ok(match &mut m.source {
                    Source::Poll(f) => Some(call(f, m.handle.clone(), self.config.watchdog_ms)?),
                    Source::Command(command) | Source::StreamCommand(command) => {
                        if !m.busy {
                            self.services.borrow_mut().pending.push(Spawn {
                                command: command.clone(),
                                callback: None,
                                target: Some(index),
                                stream: matches!(m.source, Source::StreamCommand(_)),
                                detached: false,
                            });
                            m.busy = true;
                        }
                        None
                    }
                    Source::StreamFile { path, stamp } => {
                        let meta = fs::metadata(&path)?;
                        let current = (meta.modified()?, meta.len());
                        if stamp.as_ref() != Some(&current) {
                            let text = read_regular_source(path)?;
                            *stamp = Some(current);
                            Some(Value::String(
                                self.lua
                                    .create_string(text.trim_end_matches(['\r', '\n']))?,
                            ))
                        } else {
                            None
                        }
                    }
                    Source::StreamSocket { .. } | Source::None => None,
                    Source::Builtin => match m.spec.kind {
                        ModuleKind::Clock => {
                            let format = m
                                .spec
                                .options
                                .get::<Option<String>>("format")?
                                .unwrap_or_else(|| "%a %d %b  %H:%M".into());
                            let timestamp = chrono::Local::now();
                            let mut text = String::new();
                            std::fmt::write(
                                &mut text,
                                format_args!("{}", timestamp.format(&format)),
                            )
                            .context("invalid clock strftime format")?;
                            if m.spec.options.get::<Option<f64>>("interval")?.is_none() {
                                let seconds = m.interval.as_secs();
                                if seconds == 60 || seconds == 1 {
                                    let offset = timestamp
                                        .timestamp_millis()
                                        .rem_euclid((seconds * 1000) as i64)
                                        as u64;
                                    m.next = now + Duration::from_millis(seconds * 1000 - offset);
                                }
                            }
                            let t = self.lua.create_table()?;
                            t.set("text", text)?;
                            t.set("timestamp", timestamp.timestamp())?;
                            Some(Value::Table(t))
                        }
                        ModuleKind::Cpu | ModuleKind::Ram | ModuleKind::CpuRam => {
                            let t = self.lua.create_table()?;
                            let cpu = if m.spec.kind != ModuleKind::Ram {
                                m.cpu.sample()?
                            } else {
                                0.0
                            };
                            let (used, total, ram) = if m.spec.kind != ModuleKind::Cpu {
                                memory()?
                            } else {
                                (0, 0, 0.0)
                            };
                            let percent = if m.spec.kind == ModuleKind::Ram {
                                ram
                            } else {
                                cpu
                            };
                            t.set("percent", percent)?;
                            t.set("usage", percent)?;
                            t.set("cpu", cpu)?;
                            t.set("ram", ram)?;
                            t.set("used", used)?;
                            t.set("total", total)?;
                            let format = m
                                .spec
                                .options
                                .get::<Option<String>>("format")?
                                .unwrap_or_else(|| {
                                    match m.spec.kind {
                                        ModuleKind::Cpu => "CPU {percent}%",
                                        ModuleKind::Ram => "RAM {percent}%",
                                        _ => "CPU {cpu}%  RAM {ram}%",
                                    }
                                    .into()
                                });
                            let text = format
                                .replace("{percent}", &format!("{percent:.0}"))
                                .replace("{usage}", &format!("{percent:.0}"))
                                .replace("{cpu}", &format!("{cpu:.0}"))
                                .replace("{ram}", &format!("{ram:.0}"))
                                .replace("{used}", &human_bytes(used))
                                .replace("{total}", &human_bytes(total));
                            t.set("text", text)?;
                            Some(Value::Table(t))
                        }
                        ModuleKind::Workspaces => workspaces
                            .as_ref()
                            .map(|v| self.lua.to_value(v))
                            .transpose()?,
                        ModuleKind::WindowTitle => {
                            if let Some(window) = &window {
                                let t = self.lua.create_table()?;
                                let title = window.as_ref().map(|w| w.title.clone()).unwrap_or(
                                    m.spec
                                        .options
                                        .get::<Option<String>>("empty_text")?
                                        .unwrap_or_default(),
                                );
                                let max = m
                                    .spec
                                    .options
                                    .get::<Option<usize>>("max_length")?
                                    .unwrap_or(80);
                                t.set("text", truncate(&title, max))?;
                                if let Some(w) = window {
                                    t.set("id", w.id)?;
                                    t.set("title", w.title.as_str())?;
                                    t.set("class", w.class.as_str())?;
                                }
                                Some(Value::Table(t))
                            } else {
                                None
                            }
                        }
                        ModuleKind::Systray => {
                            let t = self.lua.create_table()?;
                            t.set("count", platform.tray_count())?;
                            Some(Value::Table(t))
                        }
                        ModuleKind::Custom => None,
                    },
                })
            })();
            match result {
                Ok(Some(value)) => updates.push((index, value)),
                Ok(None) => {}
                Err(e) => eprintln!("cybar: module {}: {e:#}", m.spec.name),
            }
        }
        for (index, value) in updates {
            self.set_data(index, value);
        }
        self.launch();
        Ok(())
    }
    fn item(
        &self,
        index: usize,
        text: String,
        workspace: Option<u32>,
        state: ModuleState,
    ) -> DisplayItem {
        let m = &self.modules[index];
        let d = m.handle.inner.borrow();
        DisplayItem {
            module: index,
            text: d.text.clone().unwrap_or(text),
            icon: d.icon.clone().unwrap_or_else(|| m.spec.icon.clone()),
            style: d.style.clone(),
            state: if m.hovered {
                ModuleState::Hover
            } else if d.state != ModuleState::Normal {
                d.state
            } else {
                state
            },
            workspace,
            tray: m.spec.kind == ModuleKind::Systray,
            tray_count: 0,
        }
    }
    fn render_description(&self, index: usize, value: Value) -> Result<Vec<DisplayItem>> {
        if let Value::Table(t) = &value {
            if t.raw_len() > 0 && matches!(t.get::<Value>(1), Ok(Value::Table(_))) {
                let mut items = vec![];
                for v in t.clone().sequence_values::<Value>() {
                    items.extend(self.render_description(index, v?)?);
                }
                return Ok(items);
            }
        }
        let mut item = self.item(index, String::new(), None, ModuleState::Normal);
        match value {
            Value::String(s) => item.text = s.to_string_lossy(),
            Value::Integer(i) => item.text = i.to_string(),
            Value::Number(n) => item.text = n.to_string(),
            Value::Nil => {}
            Value::Table(t) => {
                item.text = t.get::<Option<String>>("text")?.unwrap_or_default();
                match t.get::<Value>("icon")? {
                    Value::String(icon) => item.icon = Some(icon.to_string_lossy()),
                    Value::Boolean(false) => item.icon = None,
                    Value::Nil => {}
                    _ => bail!("render icon must be a string or false"),
                };
                if let Some(state) = t.get::<Option<String>>("state")? {
                    item.state =
                        ModuleState::parse(&state).context("render returned unknown state")?;
                }
                item.workspace = t.get::<Option<u32>>("workspace")?;
                let inline = self.lua.create_table()?;
                for key in [
                    "foreground",
                    "background",
                    "font",
                    "color",
                    "colors",
                    "states",
                    "normal",
                    "hover",
                    "active",
                    "urgent",
                    "warning",
                    "padding",
                    "gap",
                    "corner_radius",
                    "icon_size",
                    "icon_position",
                    "alignment",
                    "align",
                    "min_width",
                    "max_width",
                ] {
                    inline.set(key, t.get::<Value>(key)?)?;
                }
                item.style = config::parse_style(&inline, &item.style)?;
                if let Some(style) = t.get::<Option<Table>>("style")? {
                    item.style = config::parse_style(&style, &item.style)?;
                }
            }
            _ => bail!("render must return text, a display table, or an array of display tables"),
        }
        let m = &self.modules[index];
        let d = m.handle.inner.borrow();
        if let Some(text) = &d.text {
            item.text = text.clone();
        }
        if let Some(icon) = &d.icon {
            item.icon = icon.clone();
        }
        if m.hovered {
            item.state = ModuleState::Hover;
        } else if d.state != ModuleState::Normal {
            item.state = d.state;
        }
        Ok(vec![item])
    }
    fn display(
        &self,
        index: usize,
        platform: &Platform,
        monitor: &Monitor,
    ) -> Result<Vec<DisplayItem>> {
        let m = &self.modules[index];
        let data = m.handle.inner.borrow().data.clone();
        if let Some(render) = m.spec.options.get::<Option<Function>>("render")? {
            let value: Value = call(&render, (data, m.handle.clone()), self.config.watchdog_ms)?;
            return self.render_description(index, value);
        }
        if m.spec.kind == ModuleKind::Workspaces {
            let workspaces: Vec<Workspace> = if matches!(data, Value::Nil) {
                vec![]
            } else {
                self.lua.from_value(data)?
            };
            let focused_only = m
                .spec
                .options
                .get::<Option<bool>>("monitor_only")?
                .unwrap_or(false);
            return Ok(workspaces
                .into_iter()
                .filter(|w| !focused_only || w.monitor.is_empty() || w.monitor == monitor.name)
                .map(|w| {
                    let state = if w.urgent {
                        ModuleState::Urgent
                    } else if w.focused {
                        ModuleState::Active
                    } else {
                        ModuleState::Normal
                    };
                    let label = if m
                        .spec
                        .options
                        .get::<Option<bool>>("numbers")
                        .ok()
                        .flatten()
                        .unwrap_or(false)
                    {
                        (w.index + 1).to_string()
                    } else {
                        w.name
                    };
                    self.item(index, label, Some(w.id), state)
                })
                .collect());
        }
        let text = if let Value::Table(t) = data {
            t.get::<Option<String>>("text")?.unwrap_or_default()
        } else if let Value::String(s) = data {
            s.to_string_lossy()
        } else {
            String::new()
        };
        let mut item = self.item(index, text, None, ModuleState::Normal);
        if m.spec.kind == ModuleKind::Systray {
            item.tray_count = platform.tray_count();
        }
        Ok(vec![item])
    }
    fn draw(&mut self, renderer: &mut Renderer, platform: &mut Platform) -> Result<()> {
        if !self.dirty.replace(false) {
            return Ok(());
        }
        for i in 0..self.bars.len() {
            let b = &self.bars[i];
            if !b.visible {
                continue;
            }
            let sections: Vec<Vec<DisplayItem>> = b
                .sections
                .iter()
                .map(|indices| {
                    indices
                        .iter()
                        .flat_map(|index| match self.display(*index, platform, &b.monitor) {
                            Ok(mut items) => {
                                for item in &mut items {
                                    if item.tray {
                                        item.tray_count = if self.tray_window
                                            == Some(b.window.window)
                                            && self.tray_module == Some(item.module)
                                        {
                                            platform.tray_count()
                                        } else {
                                            0
                                        };
                                    }
                                }
                                items
                            }
                            Err(e) => {
                                eprintln!(
                                    "cybar: render module {}: {e:#}",
                                    self.modules[*index].spec.name
                                );
                                vec![]
                            }
                        })
                        .collect()
                })
                .collect();
            let frame = renderer.render(
                &b.spec,
                &self.config.theme,
                b.window.geometry,
                b.window.scale,
                &sections,
            )?;
            platform.paint(b.window.window, &frame)?;
            platform.layout_tray(b.window.window, frame.tray)?;
            self.bars[i].frame = Some(frame);
        }
        Ok(())
    }
    fn hit(&self, window: u32, x: f32, y: f32) -> Option<Hit> {
        self.bars
            .iter()
            .find(|b| b.window.window == window && b.visible)
            .and_then(|b| b.frame.as_ref())
            .and_then(|f| f.hits.iter().rev().find(|h| h.rect.contains(x, y)))
            .cloned()
    }
    fn hover(&mut self, index: Option<usize>) -> Result<()> {
        if self.hover == index {
            return Ok(());
        }
        if let Some(old) = self.hover.take() {
            self.modules[old].hovered = false;
            if let Some(f) = handler(&self.modules[old].spec, "on_hover_leave")? {
                if let Err(e) = call::<_, Value>(
                    &f,
                    self.modules[old].handle.clone(),
                    self.config.watchdog_ms,
                ) {
                    eprintln!("cybar: hover callback: {e:#}");
                }
            }
        }
        self.hover = index;
        if let Some(i) = index {
            self.modules[i].hovered = true;
            if let Some(f) = handler(&self.modules[i].spec, "on_hover_enter")? {
                if let Err(e) =
                    call::<_, Value>(&f, self.modules[i].handle.clone(), self.config.watchdog_ms)
                {
                    eprintln!("cybar: hover callback: {e:#}");
                }
            }
        }
        self.dirty.set(true);
        Ok(())
    }
    fn button(&mut self, platform: &Platform, hit: Hit, button: u8) -> Result<()> {
        let name = match button {
            1 => "on_click_left",
            2 => "on_click_middle",
            3 => "on_click_right",
            _ => "on_click_button",
        };
        let m = &self.modules[hit.module];
        if let Some(f) = handler(&m.spec, name)? {
            let context = self.lua.create_table()?;
            if let Some(id) = hit.workspace {
                context.set("workspace", id)?;
            }
            context.set("button", button)?;
            let args = if name == "on_click_button" {
                mlua::MultiValue::from_vec(vec![
                    m.handle.clone().into_lua(&self.lua)?,
                    Value::Integer(button as i64),
                    Value::Table(context),
                ])
            } else {
                mlua::MultiValue::from_vec(vec![
                    m.handle.clone().into_lua(&self.lua)?,
                    Value::Table(context),
                ])
            };
            if let Err(e) = call::<_, Value>(&f, args, self.config.watchdog_ms) {
                eprintln!("cybar: click callback: {e:#}");
            }
        } else if !disabled(&m.spec, name) && m.spec.kind == ModuleKind::Workspaces && button == 1 {
            if let Some(id) = hit.workspace {
                self.switch_workspace(platform, id)?;
            }
        }
        Ok(())
    }
    fn scroll(&mut self, platform: &Platform, hit: Hit, dx: f64, dy: f64) -> Result<()> {
        let m = &mut self.modules[hit.module];
        if let Some(f) = handler(&m.spec, "on_scroll")? {
            let ms = m
                .spec
                .scroll_coalesce_ms
                .unwrap_or(self.config.scroll_coalesce_ms);
            if dx != 0.0 {
                if let Err(e) =
                    call::<_, Value>(&f, (m.handle.clone(), dx, 0.0), self.config.watchdog_ms)
                {
                    eprintln!("cybar: scroll callback: {e:#}");
                }
            }
            if dy != 0.0 {
                if ms > 0 {
                    m.scroll += dy;
                    if m.scroll_due.is_none() {
                        m.scroll_due = Some(Instant::now() + Duration::from_millis(ms));
                    }
                } else if let Err(e) =
                    call::<_, Value>(&f, (m.handle.clone(), 0.0, dy), self.config.watchdog_ms)
                {
                    eprintln!("cybar: scroll callback: {e:#}");
                }
            }
        } else if !disabled(&m.spec, "on_scroll") && m.spec.kind == ModuleKind::Workspaces {
            m.scroll += dx + dy;
            let steps = m.scroll.trunc() as i32;
            m.scroll -= steps as f64;
            if steps != 0 {
                let workspaces = self.workspaces(platform)?;
                if !workspaces.is_empty() {
                    let current = workspaces.iter().position(|w| w.focused).unwrap_or(0) as i32;
                    let next = (current + steps).rem_euclid(workspaces.len() as i32) as usize;
                    self.switch_workspace(platform, workspaces[next].id)?;
                }
            }
        }
        Ok(())
    }
    fn resolve_module(&self, name: &str) -> Result<usize> {
        let exact = self.modules.iter().any(|m| m.spec.name == name);
        let (name, monitor) = if exact {
            (name, None)
        } else if let Some((name, monitor)) = name.rsplit_once('@') {
            (name, Some(monitor))
        } else {
            (name, None)
        };
        let index = self
            .bars
            .iter()
            .filter(|b| monitor.is_none_or(|monitor| monitor == b.monitor.name))
            .flat_map(|b| {
                b.sections
                    .iter()
                    .flatten()
                    .copied()
                    .map(move |index| (b.monitor.primary, index))
            })
            .filter(|(_, i)| self.modules[*i].spec.name == name)
            .max_by_key(|(primary, i)| (*primary, std::cmp::Reverse(*i)))
            .map(|(_, i)| i)
            .or_else(|| {
                if self.bars.is_empty() {
                    self.modules.iter().position(|m| m.spec.name == name)
                } else {
                    None
                }
            })
            .with_context(|| format!("module {name:?} does not exist"))?;
        Ok(index)
    }
    fn message(&self, name: &str, text: &str) -> Result<serde_json::Value> {
        let m = &self.modules[self.resolve_module(name)?];
        let f = handler(&m.spec, "on_message")?
            .with_context(|| format!("module {name:?} has no on_message handler"))?;
        let value: Value = call(&f, (m.handle.clone(), text), self.config.watchdog_ms)?;
        Ok(self.lua.from_value(value)?)
    }
    fn toggle(&mut self, platform: &mut Platform, target: Option<&str>) -> Result<()> {
        let mut found = false;
        for b in &mut self.bars {
            if target.is_none()
                || target == Some(b.spec.name.as_str())
                || target == Some(b.monitor.name.as_str())
            {
                b.visible = !b.visible;
                platform.set_visible(b.window.window, b.visible)?;
                found = true;
            }
        }
        if !found {
            bail!("bar or monitor target does not exist");
        }
        self.dirty.set(true);
        Ok(())
    }
}

fn read_regular_source(path: &Path) -> Result<String> {
    let mut text = String::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        bail!("file stream requires a regular file: {}", path.display());
    }
    file.take((MAX_OUTPUT + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_OUTPUT {
        bail!("file stream exceeds 1 MiB");
    }
    Ok(text)
}
fn clock_needs_seconds(format: &str) -> bool {
    use chrono::format::{Fixed, Item, Numeric};
    chrono::format::StrftimeItems::new(format).any(|item| {
        matches!(
            item,
            Item::Numeric(
                Numeric::Second | Numeric::Nanosecond | Numeric::Timestamp,
                _
            ) | Item::Fixed(
                Fixed::Nanosecond
                    | Fixed::Nanosecond3
                    | Fixed::Nanosecond6
                    | Fixed::Nanosecond9
                    | Fixed::RFC2822
                    | Fixed::RFC3339
                    | Fixed::Internal(_)
            )
        )
    })
}
fn human_bytes(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MiB", n as f64 / (1u64 << 20) as f64)
    }
}
fn truncate(s: &str, max: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let clusters: Vec<&str> = s.graphemes(true).collect();
    if clusters.len() <= max {
        s.into()
    } else if max == 0 {
        String::new()
    } else {
        format!("{}…", clusters[..max - 1].concat())
    }
}

struct Watches {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<notify::Event>>,
    paths: HashSet<PathBuf>,
}
impl Watches {
    fn new(engine: &Engine) -> Result<Self> {
        let mut paths = engine.watched.borrow().clone();
        paths.insert(engine.path.clone());
        Self::for_paths(paths)
    }
    fn for_paths(paths: HashSet<PathBuf>) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })?;
        let directories: HashSet<PathBuf> = paths
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        for dir in directories {
            watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .with_context(|| format!("watch {}", dir.display()))?;
        }
        Ok(Self {
            _watcher: watcher,
            receiver,
            paths,
        })
    }
    fn changed(&self) -> bool {
        self.receiver.try_iter().any(|event| match event {
            Ok(event) => {
                !matches!(event.kind, notify::EventKind::Access(_))
                    && event.paths.iter().any(|p| {
                        self.paths.contains(p)
                            || fs::canonicalize(p).is_ok_and(|p| self.paths.contains(&p))
                    })
            }
            Err(e) => {
                eprintln!("cybar: config watcher: {e}");
                false
            }
        })
    }
}

fn sync_fds(
    poll: &Poll,
    registered: &mut HashMap<i32, bool>,
    desired: Vec<(i32, bool)>,
) -> Result<()> {
    let mut map = HashMap::new();
    for (fd, write) in desired {
        if fd >= 0 {
            map.entry(fd).and_modify(|v| *v |= write).or_insert(write);
        }
    }
    let removed: Vec<i32> = registered
        .keys()
        .filter(|fd| !map.contains_key(fd))
        .copied()
        .collect();
    for fd in removed {
        let _ = poll.registry().deregister(&mut SourceFd(&fd));
        registered.remove(&fd);
    }
    for (fd, write) in map {
        let interest = if write {
            Interest::READABLE | Interest::WRITABLE
        } else {
            Interest::READABLE
        };
        let result = if registered.contains_key(&fd) {
            poll.registry()
                .reregister(&mut SourceFd(&fd), Token(fd as usize), interest)
        } else {
            poll.registry()
                .register(&mut SourceFd(&fd), Token(fd as usize), interest)
        };
        if let Err(e) = result {
            match e.kind() {
                std::io::ErrorKind::NotFound => {
                    poll.registry()
                        .register(&mut SourceFd(&fd), Token(fd as usize), interest)?
                }
                std::io::ErrorKind::AlreadyExists => {
                    poll.registry()
                        .reregister(&mut SourceFd(&fd), Token(fd as usize), interest)?
                }
                _ => return Err(e.into()),
            }
        }
        registered.insert(fd, write);
    }
    Ok(())
}
fn reload(engine: &mut Engine, platform: &mut Platform, renderer: &mut Renderer) -> Result<()> {
    let mut candidate = Engine::load(&engine.path)?;
    candidate.prepare(platform)?;
    // Backend connection is validated before retiring the active generation.
    if let Err(e) = candidate.start_backend() {
        candidate.destroy(platform);
        return Err(e);
    }
    platform.release_tray()?;
    engine.destroy(platform);
    let mut old_services = engine.services.borrow_mut();
    let mut i = 0;
    while i < old_services.jobs.len() {
        if old_services.jobs[i].detached {
            candidate
                .services
                .borrow_mut()
                .jobs
                .push(old_services.jobs.remove(i));
        } else {
            i += 1;
        }
    }
    drop(old_services);
    std::mem::swap(engine, &mut candidate);
    renderer.clear_caches();
    engine.activate_tray(platform);
    engine.tick(platform)?;
    engine.draw(renderer, platform)?;
    Ok(())
}
pub fn run(path: &Path) -> Result<()> {
    let mut engine = Engine::load(path)?;
    let mut platform = Platform::connect()?;
    let mut renderer = Renderer::new()?;
    let mut server = ipc::Server::bind(&ipc::socket_path()?)?;
    engine.prepare(&mut platform)?;
    if let Err(e) = engine.start_backend() {
        engine.destroy(&mut platform);
        return Err(e);
    }
    engine.activate_tray(&mut platform);
    engine.tick(&platform)?;
    engine.draw(&mut renderer, &mut platform)?;
    let quit = Arc::new(AtomicBool::new(false));
    let reload_signal = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, quit.clone())?;
    }
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, reload_signal.clone())?;
    let mut watch = if engine.config.auto_reload {
        match Watches::new(&engine) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("cybar: auto-reload unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let mut poll = Poll::new()?;
    let mut registered = HashMap::new();
    let mut events = Events::with_capacity(128);
    let mut reload_due = None;
    let result = (|| -> Result<()> {
        while !quit.load(Ordering::Relaxed) {
            if watch
                .as_ref()
                .is_some_and(|w| !engine.watched.borrow().is_subset(&w.paths))
            {
                match Watches::new(&engine) {
                    Ok(w) => watch = Some(w),
                    Err(e) => eprintln!("cybar: watch required Lua module: {e:#}"),
                }
            }
            if watch.as_ref().is_some_and(Watches::changed) {
                reload_due =
                    Some(Instant::now() + Duration::from_millis(engine.config.reload_debounce_ms));
            }
            if reload_signal.swap(false, Ordering::Relaxed)
                || reload_due.is_some_and(|d| Instant::now() >= d)
            {
                reload_due = None;
                match reload(&mut engine, &mut platform, &mut renderer) {
                    Ok(()) => {
                        watch = if engine.config.auto_reload {
                            Watches::new(&engine).ok()
                        } else {
                            None
                        };
                        eprintln!("cybar: configuration reloaded");
                    }
                    Err(e) => {
                        eprintln!("cybar: reload failed; keeping current configuration: {e:#}")
                    }
                }
            }
            let mut monitors_changed = false;
            for event in platform.poll_events()? {
                let result = match event {
                    XEvent::Expose { .. } | XEvent::WmChanged | XEvent::TrayChanged => {
                        engine.dirty.set(true);
                        Ok(())
                    }
                    XEvent::MonitorsChanged => {
                        monitors_changed = true;
                        Ok(())
                    }
                    XEvent::Close { .. } => {
                        quit.store(true, Ordering::Relaxed);
                        Ok(())
                    }
                    XEvent::Pointer { window, x, y } => {
                        let index = engine.hit(window, x, y).map(|h| h.module);
                        engine.hover(index)
                    }
                    XEvent::Leave { .. } => engine.hover(None),
                    XEvent::Button {
                        window,
                        x,
                        y,
                        button,
                    } => {
                        if let Some(hit) = engine.hit(window, x, y) {
                            engine.button(&platform, hit, button)
                        } else {
                            Ok(())
                        }
                    }
                    XEvent::Scroll {
                        window,
                        x,
                        y,
                        dx,
                        dy,
                    } => {
                        if let Some(hit) = engine.hit(window, x, y) {
                            engine.scroll(&platform, hit, dx, dy)
                        } else {
                            Ok(())
                        }
                    }
                };
                if let Err(e) = result {
                    eprintln!("cybar: event callback: {e:#}");
                }
            }
            if monitors_changed {
                match reload(&mut engine, &mut platform, &mut renderer) {
                    Ok(()) => {
                        watch = if engine.config.auto_reload {
                            Watches::new(&engine).ok()
                        } else {
                            None
                        }
                    }
                    Err(e) => eprintln!("cybar: monitor update: {e:#}"),
                }
            }
            engine.tick(&platform)?;
            for request in server.pump() {
                let e = &request.envelope;
                let result = match e.method.as_deref() {
                    Some("reload") => reload(&mut engine, &mut platform, &mut renderer).map(|()| {
                        watch = if engine.config.auto_reload {
                            Watches::new(&engine).ok()
                        } else {
                            None
                        };
                        serde_json::Value::Bool(true)
                    }),
                    Some("quit") => {
                        quit.store(true, Ordering::Relaxed);
                        Ok(serde_json::Value::Bool(true))
                    }
                    Some("toggle") => engine
                        .toggle(
                            &mut platform,
                            e.params
                                .as_ref()
                                .and_then(|p| p.get("target"))
                                .and_then(|t| t.as_str()),
                        )
                        .map(|()| serde_json::Value::Bool(true)),
                    Some("module.exists") => (|| {
                        let p = e.params.as_ref().context("missing params")?;
                        let name = p
                            .get("module")
                            .and_then(|v| v.as_str())
                            .context("missing module name")?;
                        engine.resolve_module(name)?;
                        Ok(serde_json::Value::Bool(true))
                    })(),
                    Some("module.message") => (|| {
                        let p = e.params.as_ref().context("missing params")?;
                        let module = p
                            .get("module")
                            .and_then(|v| v.as_str())
                            .context("missing module name")?;
                        let text = p
                            .get("text")
                            .and_then(|v| v.as_str())
                            .context("missing text")?;
                        engine.message(module, text)
                    })(),
                    Some(method) => Err(anyhow::anyhow!("unknown method {method:?}")),
                    None => Err(anyhow::anyhow!("missing method")),
                };
                server.reply(request.client, e, result);
            }
            server.flush();
            engine.draw(&mut renderer, &mut platform)?;
            let mut fds = vec![(platform.fd(), false), (server.fd(), false)];
            fds.extend(server.fds());
            fds.extend(engine.fds());
            sync_fds(&poll, &mut registered, fds)?;
            let mut delay = engine.delay();
            if watch.is_some() || engine.source_watch.is_some() {
                delay = delay.min(Duration::from_millis(100));
            }
            if let Some(due) = reload_due {
                delay = delay.min(due.saturating_duration_since(Instant::now()));
            }
            if quit.load(Ordering::Relaxed) {
                break;
            }
            match poll.poll(&mut events, Some(delay)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })();
    server.flush();
    let _ = platform.release_tray();
    engine.destroy(&mut platform);
    result
}

pub fn check(path: &Path) -> Result<()> {
    let _ = Engine::load(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::ObjectLike;
    use std::sync::atomic::AtomicUsize;
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn fixture(extra: &str) -> Engine {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cybar-runtime-{}-{id}.lua", std::process::id()));
        fs::write(&path,"local b=require('cybar');return {bars={{sections={left={b.custom{name='test',render=function(d)return d or '' end,__EXTRA__}}}}}}".replace("__EXTRA__",extra)).unwrap();
        let mut engine = Engine::load(&path).unwrap();
        fs::remove_file(path).unwrap();
        let spec = engine.config.bars[0].sections[0].modules[0].clone();
        engine.modules.push(engine.module(spec).unwrap());
        engine
    }
    fn pump_until(engine: &mut Engine, done: impl Fn(&Engine) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            engine.services().unwrap();
            if done(engine) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "asynchronous work did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn display_text_is_not_a_color_and_false_clears_icon() {
        let engine = fixture("");
        let table: Value = engine
            .lua
            .load(
                "return {text='Connected',icon=false,state='warning',style={foreground='#abcdef'}}",
            )
            .eval()
            .unwrap();
        let items = engine.render_description(0, table).unwrap();
        assert_eq!(items[0].text, "Connected");
        assert_eq!(items[0].icon, None);
        assert_eq!(items[0].state, ModuleState::Warning);
        assert_eq!(items[0].style.foreground, Some(Color(171, 205, 239, 255)));
    }
    #[test]
    fn parser_updates_data_and_keeps_previous_value_on_error() {
        let engine = fixture(
            "source={type='command',command='printf 7',parse=function(v) if v=='bad' then error('bad data') end return {number=tonumber(v)} end}",
        );
        engine.set_data(0, Value::String(engine.lua.create_string("7").unwrap()));
        let value = engine.modules[0].handle.inner.borrow().data.clone();
        let table = match value {
            Value::Table(t) => t,
            _ => panic!(),
        };
        assert_eq!(table.get::<i64>("number").unwrap(), 7);
        engine.dirty.set(false);
        engine.set_data(0, Value::String(engine.lua.create_string("7").unwrap()));
        assert!(!engine.dirty.get());
        engine.set_data(0, Value::String(engine.lua.create_string("bad").unwrap()));
        assert_eq!(table.get::<i64>("number").unwrap(), 7);
    }
    #[test]
    fn timers_are_cancellable_and_callbacks_can_schedule_more_work() {
        let mut engine = fixture("");
        engine.lua.load("local b=require('cybar'); count=0; local cancel=b.after(0.001,function()count=99 end);cancel:cancel();b.after(0.001,function()count=count+1;b.after(0.001,function()count=count+1 end)end)").exec().unwrap();
        pump_until(&mut engine, |e| {
            e.lua.globals().get::<i64>("count").unwrap() == 2
        });
        assert!(engine.services.borrow().timers.is_empty());
    }
    #[test]
    fn spawn_captures_both_pipes_and_reaps_child() {
        let mut engine = fixture("");
        engine.lua.load("local b=require('cybar');b.spawn(\"printf hello;printf problem >&2;exit 3\",function(out,code,err)result={out=out,code=code,err=err};b.after(0.001,function()finished=true end)end)").exec().unwrap();
        pump_until(&mut engine, |e| {
            e.lua.globals().get::<Option<bool>>("finished").unwrap() == Some(true)
        });
        let result: Table = engine.lua.globals().get("result").unwrap();
        assert_eq!(result.get::<String>("out").unwrap(), "hello");
        assert_eq!(result.get::<String>("err").unwrap(), "problem");
        assert_eq!(result.get::<i32>("code").unwrap(), 3);
        assert!(engine.services.borrow().jobs.is_empty());
    }
    #[test]
    fn stream_child_updates_on_each_line() {
        let mut engine = fixture("");
        engine.services.borrow_mut().pending.push(Spawn {
            command: "printf 'one\\ntwo\\n'".into(),
            callback: None,
            target: Some(0),
            stream: true,
            detached: false,
        });
        pump_until(&mut engine, |e| {
            e.services.borrow().jobs.is_empty()
                && matches!(&e.modules[0].handle.inner.borrow().data,Value::String(v) if v.to_string_lossy()=="two")
        });
    }
    #[test]
    fn noisy_stderr_does_not_block_event_loop() {
        let mut engine = fixture("");
        engine.services.borrow_mut().pending.push(Spawn {
            command: "while :; do printf 'noise noise noise\\n' >&2; done".into(),
            callback: None,
            target: None,
            stream: true,
            detached: false,
        });
        engine.launch();
        std::thread::sleep(Duration::from_millis(10));
        let start = Instant::now();
        engine.services().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn closed_socket_reports_disabled_fd_and_releases_service_reference() {
        let mut engine = fixture("");
        let path =
            std::env::temp_dir().join(format!("cybar-runtime-socket-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let t: Table = engine.lua.load("return require('cybar')").eval().unwrap();
        let sockets: Table = t.get("socket").unwrap();
        let function: Function = sockets.get("connect").unwrap();
        let handle: mlua::AnyUserData = function.call(path.to_string_lossy().to_string()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"hello").unwrap();
        engine.services().unwrap();
        let h = handle.borrow::<SocketHandle>().unwrap();
        assert_eq!(
            h.socket
                .borrow()
                .incoming
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            b"hello"
        );
        drop(h);
        let close: Function = handle.get("close").unwrap();
        close.call::<()>(handle.clone()).unwrap();
        engine.services().unwrap();
        let fd: Function = handle.get("fd").unwrap();
        assert_eq!(fd.call::<i32>(handle).unwrap(), -1);
        assert!(engine.services.borrow().sockets.is_empty());
        drop(listener);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn fifo_source_cannot_block_the_event_loop() {
        let path = std::env::temp_dir().join(format!("cybar-fifo-{}", std::process::id()));
        let name = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(read_regular_source(&path).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn clock_refresh_detects_strftime_modifiers() {
        assert!(!clock_needs_seconds("%H:%M"));
        for format in ["%H:%M:%-S", "%_S", "%.3f", "%T", "%c", "%+"] {
            assert!(clock_needs_seconds(format), "{format}");
        }
    }
    #[test]
    fn title_truncation_preserves_graphemes() {
        assert_eq!(truncate("A👩‍💻BC", 3), "A👩‍💻…");
        assert_eq!(truncate("e\u{301}xy", 2), "e\u{301}…");
    }
}
