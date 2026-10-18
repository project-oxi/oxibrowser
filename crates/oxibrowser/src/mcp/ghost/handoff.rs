//! Separate inherited anonymous Unix socket: possession belongs to the trusted
//! supervisor, never the page/agent MCP channel. No listener or saved capability.
use super::*;
use std::collections::HashSet;
use std::os::fd::FromRawFd;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub(super) struct HostIo {
    pub rx: tokio::sync::mpsc::Receiver<String>,
    writer: tokio::net::unix::OwnedWriteHalf,
}
impl HostIo {
    pub fn open(fd: i32) -> Result<Self, String> {
        if fd < 3 {
            return Err("host control fd must be >=3".into());
        }
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err("invalid host control descriptor".into());
        }
        let mut kind: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
        let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut address_size = std::mem::size_of_val(&address) as libc::socklen_t;
        let valid = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut size,
            ) == 0
                && kind == libc::SOCK_STREAM
                && libc::getsockname(
                    fd,
                    (&mut address as *mut libc::sockaddr_storage).cast(),
                    &mut address_size,
                ) == 0
                && address.ss_family as libc::c_int == libc::AF_UNIX
        };
        if !valid {
            return Err("host control must be an AF_UNIX stream socketpair".into());
        }
        // Explicitly supplied descriptor is owned by this child; validate before reading.
        let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        let local = stream
            .local_addr()
            .map_err(|_| "host control must be a connected Unix socket")?;
        let peer = stream
            .peer_addr()
            .map_err(|_| "host control must be a connected Unix socket")?;
        if !local.is_unnamed() || !peer.is_unnamed() {
            return Err("host control must be an anonymous socketpair".into());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err("host control descriptor setup failed".into());
        }
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| e.to_string())?;
        let (reader, writer) = stream.into_split();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = Vec::new();
                let read = (&mut reader)
                    .take((MAX_LINE + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .await;
                if !matches!(read, Ok(n) if n>0) {
                    break;
                }
                if line.len() > MAX_LINE {
                    let _ = tx.send("null".into()).await;
                    break;
                }
                let Ok(line) = String::from_utf8(line) else {
                    let _ = tx.send("null".into()).await;
                    break;
                };
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self { rx, writer })
    }
    pub async fn send(&mut self, value: Value) -> bool {
        let bytes = format!("{value}\n").into_bytes();
        matches!(
            tokio::time::timeout(Duration::from_secs(1), async {
                self.writer.write_all(&bytes).await?;
                self.writer.flush().await
            })
            .await,
            Ok(Ok(()))
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Agent,
    Pausing,
    Human,
    Refresh,
    Closed,
}
impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent_active",
            Self::Pausing => "pause_requested",
            Self::Human => "human_owned",
            Self::Refresh => "resume_pending",
            Self::Closed => "closed",
        }
    }
}
pub(super) struct State {
    task: Option<String>,
    pub phase: Phase,
    pub epoch: u64,
    handoff: Option<String>,
    used_handoffs: HashSet<String>,
    pub pending: Option<Request>,
    fresh: HashSet<String>,
    inventory: bool,
}
impl State {
    pub fn new(task: Option<&str>) -> Self {
        Self {
            task: task.map(String::from),
            phase: Phase::Agent,
            epoch: 0,
            handoff: None,
            used_handoffs: HashSet::new(),
            pending: None,
            fresh: HashSet::new(),
            inventory: false,
        }
    }
    pub fn status(&self, scope: &str) -> Value {
        let mut tabs: Vec<_> = self.fresh.iter().collect();
        tabs.sort();
        json!({"browserSessionId":scope,"ghostTaskId":self.task,"controlEpoch":self.epoch,
            "state":self.phase.label(),"handoffId":self.handoff,"controlAvailable":self.task.is_some(),
            "freshObservationRequired":self.phase==Phase::Refresh,"tabsToObserve":tabs,
            "scriptsFrozen":false,"networkFrozen":false,"trust":"trusted_host_state"})
    }
    pub fn agent_gate(&self, req: &Request, scope: &str) -> Result<(), &'static str> {
        let Some(task) = &self.task else {
            return Ok(());
        };
        if req.params["_meta"]["ghostTaskId"].as_str() != Some(task.as_str()) {
            return Err("TASK_SCOPE_MISMATCH");
        }
        if req.params["_meta"]["browserSessionId"].as_str() != Some(scope) {
            return Err("SESSION_SCOPE_MISMATCH");
        }
        if req.params["_meta"]["controlEpoch"].as_u64() != Some(self.epoch) {
            return Err("CONTROL_EPOCH_MISMATCH");
        }
        match self.phase {
            Phase::Agent => Ok(()),
            Phase::Refresh
                if matches!(
                    req.params["name"].as_str(),
                    Some("browser_snapshot" | "browser_tabs")
                ) =>
            {
                Ok(())
            }
            Phase::Refresh => Err("FRESH_OBSERVATION_REQUIRED"),
            Phase::Pausing => Err("HANDOFF_PENDING"),
            Phase::Human => Err("HUMAN_OWNS_BROWSER"),
            Phase::Closed => Err("SESSION_CLOSED"),
        }
    }
    pub fn observed(&mut self, name: &str, args: &Value, success: bool) {
        if self.phase != Phase::Refresh || !success {
            return;
        }
        if name == "browser_snapshot"
            && let Some(tab) = args["tab_id"].as_str()
        {
            self.fresh.remove(tab);
            self.inventory = false;
        }
        if name == "browser_tabs" {
            self.inventory = false;
        }
        if self.fresh.is_empty() && !self.inventory {
            self.phase = Phase::Agent;
        }
    }
    pub fn validate(&self, req: &Request, scope: &str) -> Result<(), &'static str> {
        let obj = req.params.as_object().ok_or("INVALID_CONTROL_ARGUMENTS")?;
        let extra: &[&str] = match req.method.as_str() {
            "ghost/status" => {
                return if obj.is_empty() {
                    Ok(())
                } else {
                    Err("INVALID_CONTROL_ARGUMENTS")
                };
            }
            "ghost/handoff" | "ghost/resume" => &["handoff_id"],
            "ghost/human" => &["handoff_id", "name", "arguments", "timeoutMs"],
            "ghost/cancel" => &["channel", "request_id"],
            _ => return Err("CONTROL_METHOD_DENIED"),
        };
        if obj.keys().any(|k| {
            !["session_id", "task_id", "epoch", "expected_state"].contains(&k.as_str())
                && !extra.contains(&k.as_str())
        }) {
            return Err("INVALID_CONTROL_ARGUMENTS");
        }
        if req.params["session_id"].as_str() != Some(scope)
            || req.params["task_id"].as_str() != self.task.as_deref()
        {
            return Err("CONTROL_SCOPE_MISMATCH");
        }
        if req.params["epoch"].as_u64() != Some(self.epoch) {
            return Err("CONTROL_EPOCH_MISMATCH");
        }
        if req.params["expected_state"].as_str() != Some(self.phase.label()) {
            return Err("CONTROL_STATE_MISMATCH");
        }
        match req.method.as_str() {
            "ghost/handoff" => {
                if !matches!(self.phase, Phase::Agent | Phase::Refresh) {
                    return Err("INVALID_HANDOFF_STATE");
                }
                let id = req.params["handoff_id"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .ok_or("HANDOFF_ID_REQUIRED")?;
                if self.used_handoffs.contains(id) {
                    return Err("HANDOFF_ID_REUSED");
                }
            }
            "ghost/resume" | "ghost/human" => {
                if self.phase != Phase::Human {
                    return Err("HUMAN_OWNERSHIP_REQUIRED");
                }
                if req.params["handoff_id"].as_str() != self.handoff.as_deref() {
                    return Err("HANDOFF_SCOPE_MISMATCH");
                }
            }
            "ghost/cancel"
                if !matches!(req.params["channel"].as_str(), Some("agent" | "human"))
                    || !req.params["request_id"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 128)
                        && req.params["request_id"].as_i64().is_none() =>
            {
                return Err("INVALID_CONTROL_ARGUMENTS");
            }
            _ => {}
        }
        Ok(())
    }
    pub fn begin_handoff(&mut self, req: Request) {
        let id = req.params["handoff_id"].as_str().unwrap().to_string();
        self.used_handoffs.insert(id.clone());
        self.handoff = Some(id);
        self.phase = Phase::Pausing;
        self.pending = Some(req);
    }
    pub fn finish_handoff(&mut self, runtime: &mut Runtime) -> Request {
        invalidate(runtime);
        self.epoch += 1;
        self.phase = Phase::Human;
        self.pending.take().unwrap()
    }
    pub fn resume(&mut self, runtime: &mut Runtime) {
        invalidate(runtime);
        self.epoch += 1;
        self.phase = Phase::Refresh;
        self.fresh = runtime.tabs.keys().cloned().collect();
        self.inventory = self.fresh.is_empty();
    }
}
pub(super) fn invalidate(runtime: &mut Runtime) {
    for tab in runtime.tabs.values_mut() {
        tab.invalidate();
    }
}
pub(super) fn response(req: &Request, state: &State, scope: &str, data: Value) -> Value {
    rpc_result(
        req.id.clone(),
        json!({"hostState":state.status(scope),"data":data}),
    )
}
pub(super) fn error(
    req: &Request,
    state: &State,
    scope: &str,
    message: &str,
    uncertain: bool,
) -> Value {
    let mut r = rpc_error(req.id.clone(), -32000, message);
    r["error"]["data"] = json!({"hostState":state.status(scope),"mayHaveActed":uncertain,"restartRequired":uncertain});
    r
}
pub(super) fn decorate(mut r: Value, state: &State, scope: &str) -> Value {
    if state.task.is_some() && r.get("result").is_some() {
        r["result"]["_meta"]["controlEpoch"] = json!(state.epoch);
        r["result"]["_meta"]["ownership"] = state.status(scope);
    }
    r
}
