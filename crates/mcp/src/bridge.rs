//! Local IPC bridge connecting `layercraft-cli mcp` to a live LayerCraft Studio desktop window.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(not(unix))]
use std::net::{TcpListener, TcpStream};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How `layercraft-mcp` chooses between a running Studio instance and a headless session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpConnectMode {
    /// Connect to a live LayerCraft Studio window if one is listening; otherwise run headlessly.
    #[default]
    Auto,
    /// Require a running LayerCraft Studio window (error if none is reachable).
    Connect,
    /// Always run in an isolated headless session even if Studio is running.
    Standalone,
}

/// Request message sent from an MCP client process to a running Studio host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRequest {
    pub id: u64,
    pub op: BridgeOp,
}

/// Operation payload carried by [`BridgeRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeOp {
    Ping,
    ToolCall { name: String, arguments: Value },
    ReadResource { uri: String },
}

/// Response returned from the Studio host to the MCP client process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Metadata written alongside the IPC socket so MCP clients can inspect the live Studio host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioDiscoveryInfo {
    pub pid: u32,
    pub version: String,
    pub endpoint: String,
}

/// Default IPC endpoint path (`LAYERCRAFT_SOCKET` or `${XDG_RUNTIME_DIR:-/tmp}/layercraft-studio.sock` on Unix).
pub fn default_socket_path() -> PathBuf {
    if let Some(env_path) = std::env::var_os("LAYERCRAFT_SOCKET")
        && !env_path.is_empty()
    {
        return PathBuf::from(env_path);
    }

    #[cfg(unix)]
    {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
            && !runtime_dir.is_empty()
        {
            return PathBuf::from(runtime_dir).join("layercraft-studio.sock");
        }
        std::env::temp_dir().join("layercraft-studio.sock")
    }

    #[cfg(not(unix))]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
            && !local_app_data.is_empty()
        {
            return PathBuf::from(local_app_data).join("layercraft-studio.json");
        }
        std::env::temp_dir().join("layercraft-studio.json")
    }
}

fn discovery_path_for(endpoint: &Path) -> PathBuf {
    let file_name = endpoint
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("layercraft-studio");
    if let Some(stem) = file_name.strip_suffix(".sock") {
        endpoint.with_file_name(format!("{stem}.json"))
    } else if file_name.ends_with(".json") {
        endpoint.to_path_buf()
    } else {
        endpoint.with_file_name(format!("{file_name}.json"))
    }
}

/// Pending bridge request queued for execution on the Studio UI thread.
pub struct IncomingBridgeRequest {
    pub request: BridgeRequest,
    pub reply: SyncSender<BridgeResponse>,
}

/// Studio-side IPC listener receiving MCP requests and forwarding them to the UI thread.
pub struct StudioBridgeHost {
    endpoint: PathBuf,
    discovery_path: PathBuf,
    info: StudioDiscoveryInfo,
    rx: Receiver<IncomingBridgeRequest>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl StudioBridgeHost {
    /// Bind the local IPC listener and start accepting MCP client connections.
    pub fn bind(
        custom_endpoint: Option<PathBuf>,
        wake_ui: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let endpoint = custom_endpoint.unwrap_or_else(default_socket_path);
        let discovery_path = discovery_path_for(&endpoint);
        if let Some(parent) = endpoint.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        if let Some(parent) = discovery_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }

        let (tx, rx) = mpsc::channel::<IncomingBridgeRequest>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let workers = Arc::new(Mutex::new(Vec::new()));

        let (info, accept_thread) = Self::start_listener_with_handle(
            &endpoint,
            &discovery_path,
            tx,
            wake_ui,
            Arc::clone(&shutdown),
            Arc::clone(&workers),
        )?;

        let info_json = serde_json::to_string_pretty(&info)
            .map_err(|e| format!("failed serializing bridge discovery info: {e}"))?;
        fs::write(&discovery_path, info_json)
            .map_err(|e| format!("{}: {e}", discovery_path.display()))?;

        Ok(Self {
            endpoint,
            discovery_path,
            info,
            rx,
            shutdown,
            accept_thread: Some(accept_thread),
            workers,
        })
    }

    #[cfg(unix)]
    fn start_listener_with_handle(
        endpoint: &Path,
        _discovery_path: &Path,
        tx: Sender<IncomingBridgeRequest>,
        wake_ui: Arc<dyn Fn() + Send + Sync>,
        shutdown: Arc<AtomicBool>,
        workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    ) -> Result<(StudioDiscoveryInfo, JoinHandle<()>), String> {
        if endpoint.exists() {
            if UnixStream::connect(endpoint).is_ok() {
                return Err(format!(
                    "another LayerCraft Studio instance is already listening at {}",
                    endpoint.display()
                ));
            }
            let _ = fs::remove_file(endpoint);
        }
        let listener = UnixListener::bind(endpoint)
            .map_err(|e| format!("failed binding {}: {e}", endpoint.display()))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed setting nonblocking socket: {e}"))?;

        let info = StudioDiscoveryInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            endpoint: endpoint.to_string_lossy().into_owned(),
        };
        let ping_info = info.clone();

        let handle = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let tx_c = tx.clone();
                        let wake_c = Arc::clone(&wake_ui);
                        let shutdown_c = Arc::clone(&shutdown);
                        let info_c = ping_info.clone();
                        let w_handle = thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                            let Ok(writer_stream) = stream.try_clone() else {
                                return;
                            };
                            handle_client_stream(
                                stream,
                                writer_stream,
                                tx_c,
                                wake_c,
                                shutdown_c,
                                &info_c,
                            );
                        });
                        if let Ok(mut list) = workers.lock() {
                            list.retain(|h| !h.is_finished());
                            list.push(w_handle);
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok((info, handle))
    }

    #[cfg(not(unix))]
    fn start_listener_with_handle(
        _endpoint: &Path,
        discovery_path: &Path,
        tx: Sender<IncomingBridgeRequest>,
        wake_ui: Arc<dyn Fn() + Send + Sync>,
        shutdown: Arc<AtomicBool>,
        workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    ) -> Result<(StudioDiscoveryInfo, JoinHandle<()>), String> {
        if discovery_path.exists()
            && let Ok(txt) = fs::read_to_string(discovery_path)
            && let Ok(existing) = serde_json::from_str::<StudioDiscoveryInfo>(&txt)
            && TcpStream::connect(&existing.endpoint).is_ok()
        {
            return Err(format!(
                "another LayerCraft Studio instance is already listening at {}",
                existing.endpoint
            ));
        }
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("failed binding TCP: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("failed querying bridge TCP address: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed setting nonblocking socket: {e}"))?;

        let info = StudioDiscoveryInfo {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            endpoint: local_addr.to_string(),
        };
        let ping_info = info.clone();

        let handle = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let tx_c = tx.clone();
                        let wake_c = Arc::clone(&wake_ui);
                        let shutdown_c = Arc::clone(&shutdown);
                        let info_c = ping_info.clone();
                        let w_handle = thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                            let Ok(writer_stream) = stream.try_clone() else {
                                return;
                            };
                            handle_client_stream(
                                stream,
                                writer_stream,
                                tx_c,
                                wake_c,
                                shutdown_c,
                                &info_c,
                            );
                        });
                        if let Ok(mut list) = workers.lock() {
                            list.retain(|h| !h.is_finished());
                            list.push(w_handle);
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok((info, handle))
    }

    /// Drain all currently queued MCP bridge requests without blocking the UI thread.
    pub fn drain_requests(&self) -> Vec<IncomingBridgeRequest> {
        let mut out = Vec::new();
        while let Ok(item) = self.rx.try_recv() {
            out.push(item);
        }
        out
    }

    /// Endpoint string of the active socket or discovery file.
    pub fn endpoint_display(&self) -> String {
        self.info.endpoint.clone()
    }

    /// Host discovery metadata.
    pub fn info(&self) -> &StudioDiscoveryInfo {
        &self.info
    }
}

impl Drop for StudioBridgeHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        if let Ok(mut list) = self.workers.lock() {
            for handle in list.drain(..) {
                let _ = handle.join();
            }
        }
        #[cfg(unix)]
        let _ = fs::remove_file(&self.endpoint);
        let _ = fs::remove_file(&self.discovery_path);
    }
}

fn handle_client_stream<R: Read, W: Write>(
    read_stream: R,
    mut write_stream: W,
    tx: Sender<IncomingBridgeRequest>,
    wake_ui: Arc<dyn Fn() + Send + Sync>,
    shutdown: Arc<AtomicBool>,
    info: &StudioDiscoveryInfo,
) {
    let mut reader = BufReader::new(read_stream);
    let mut line = String::new();

    while !shutdown.load(Ordering::Relaxed) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let req: BridgeRequest = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        let resp = BridgeResponse {
                            id: 0,
                            ok: false,
                            result: Value::Null,
                            error: Some(format!("invalid bridge request: {e}")),
                        };
                        if write_json_line(&mut write_stream, &resp).is_err() {
                            break;
                        }
                        continue;
                    }
                };

                let resp = match &req.op {
                    BridgeOp::Ping => BridgeResponse {
                        id: req.id,
                        ok: true,
                        result: serde_json::to_value(info).unwrap_or(Value::Null),
                        error: None,
                    },
                    BridgeOp::ToolCall { .. } | BridgeOp::ReadResource { .. } => {
                        let req_id = req.id;
                        let (rep_tx, rep_rx) = mpsc::sync_channel::<BridgeResponse>(1);
                        if tx
                            .send(IncomingBridgeRequest {
                                request: req,
                                reply: rep_tx,
                            })
                            .is_err()
                        {
                            BridgeResponse {
                                id: req_id,
                                ok: false,
                                result: Value::Null,
                                error: Some("LayerCraft Studio closed".to_owned()),
                            }
                        } else {
                            (wake_ui)();
                            wait_for_ui_reply(&rep_rx, req_id, &shutdown)
                        }
                    }
                };

                if write_json_line(&mut write_stream, &resp).is_err() {
                    break;
                }
            }
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
}

fn wait_for_ui_reply(
    rep_rx: &Receiver<BridgeResponse>,
    req_id: u64,
    shutdown: &AtomicBool,
) -> BridgeResponse {
    for _ in 0..600 {
        if shutdown.load(Ordering::Relaxed) {
            return BridgeResponse {
                id: req_id,
                ok: false,
                result: Value::Null,
                error: Some("LayerCraft Studio shutting down".to_owned()),
            };
        }
        match rep_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(resp) => return resp,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return BridgeResponse {
                    id: req_id,
                    ok: false,
                    result: Value::Null,
                    error: Some("LayerCraft Studio dropped request".to_owned()),
                };
            }
        }
    }
    BridgeResponse {
        id: req_id,
        ok: false,
        result: Value::Null,
        error: Some("LayerCraft Studio timed out handling request".to_owned()),
    }
}

fn write_json_line<W: Write, T: Serialize>(out: &mut W, msg: &T) -> std::io::Result<()> {
    let text = serde_json::to_string(msg)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
    writeln!(out, "{text}")?;
    out.flush()
}

struct ConnPair {
    #[cfg(unix)]
    reader: BufReader<UnixStream>,
    #[cfg(unix)]
    writer: UnixStream,
    #[cfg(not(unix))]
    reader: BufReader<TcpStream>,
    #[cfg(not(unix))]
    writer: TcpStream,
}

impl ConnPair {
    fn connect(endpoint: &Path, read_timeout: Duration) -> Result<Self, String> {
        #[cfg(unix)]
        {
            let stream = UnixStream::connect(endpoint)
                .map_err(|e| format!("failed connecting to {}: {e}", endpoint.display()))?;
            let _ = stream.set_read_timeout(Some(read_timeout));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
            let writer = stream
                .try_clone()
                .map_err(|e| format!("failed cloning socket stream: {e}"))?;
            Ok(Self {
                reader: BufReader::new(stream),
                writer,
            })
        }

        #[cfg(not(unix))]
        {
            let discovery = discovery_path_for(endpoint);
            let txt = fs::read_to_string(&discovery)
                .map_err(|e| format!("{}: {e}", discovery.display()))?;
            let info: StudioDiscoveryInfo = serde_json::from_str(&txt)
                .map_err(|e| format!("invalid discovery info {}: {e}", discovery.display()))?;
            let stream = TcpStream::connect(&info.endpoint)
                .map_err(|e| format!("failed connecting to {}: {e}", info.endpoint))?;
            let _ = stream.set_read_timeout(Some(read_timeout));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
            let writer = stream
                .try_clone()
                .map_err(|e| format!("failed cloning TCP stream: {e}"))?;
            Ok(Self {
                reader: BufReader::new(stream),
                writer,
            })
        }
    }

    fn exchange(&mut self, req: &BridgeRequest) -> Result<BridgeResponse, String> {
        write_json_line(&mut self.writer, req).map_err(|e| format!("bridge write error: {e}"))?;
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .map_err(|e| format!("bridge read error: {e}"))?;
        if n == 0 {
            return Err("bridge connection closed (EOF)".to_owned());
        }
        serde_json::from_str(line.trim()).map_err(|e| format!("invalid bridge response: {e}"))
    }
}

/// Persistent client talking to a running [`StudioBridgeHost`], with transparent reconnect.
#[derive(Default)]
pub struct StudioBridgeClient {
    conn: Option<ConnPair>,
    next_id: u64,
}

impl StudioBridgeClient {
    /// Construct a fresh bridge client.
    pub fn new() -> Self {
        Self {
            conn: None,
            next_id: 1,
        }
    }

    /// Clear any cached socket connection.
    pub fn reset(&mut self) {
        self.conn = None;
    }

    /// Probe whether a live LayerCraft Studio instance is responding at `custom_endpoint` (or default).
    pub fn probe_studio(custom_endpoint: Option<&Path>) -> Option<StudioDiscoveryInfo> {
        let ep = custom_endpoint
            .map(PathBuf::from)
            .unwrap_or_else(default_socket_path);
        #[cfg(unix)]
        if !ep.exists() {
            return None;
        }
        #[cfg(not(unix))]
        if !discovery_path_for(&ep).exists() {
            return None;
        }
        let mut pair = ConnPair::connect(&ep, Duration::from_secs(2)).ok()?;
        let resp = pair
            .exchange(&BridgeRequest {
                id: 0,
                op: BridgeOp::Ping,
            })
            .ok()?;
        if !resp.ok {
            return None;
        }
        serde_json::from_value::<StudioDiscoveryInfo>(resp.result).ok()
    }

    /// Send an operation to the live Studio bridge, reconnecting once if a cached stream closed.
    pub fn send_request(
        &mut self,
        endpoint: &Path,
        op: BridgeOp,
    ) -> Result<BridgeResponse, String> {
        let req_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let req = BridgeRequest { id: req_id, op };

        if let Some(pair) = self.conn.as_mut() {
            match pair.exchange(&req) {
                Ok(resp) => return Ok(resp),
                Err(_) => {
                    self.conn = None;
                }
            }
        }

        let mut pair = ConnPair::connect(endpoint, Duration::from_secs(120))?;
        let resp = pair.exchange(&req)?;
        self.conn = Some(pair);
        Ok(resp)
    }
}
