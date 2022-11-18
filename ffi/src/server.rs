use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use pyo3::{prelude::*, types::PyTuple};
use tokio::{
    net::UdpSocket,
    sync::broadcast::{self, Sender as BroadcastSender},
    sync::mpsc::{self, channel, unbounded_channel},
    sync::Notify,
};
use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
use windows::core::{HSTRING, PCWSTR};
use windows::w;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};
use x25519_dalek::PublicKey;
use mitmproxy_rs::MAX_PACKET_SIZE;
use mitmproxy_rs::messages::TransportCommand;

use mitmproxy_rs::network::{NetworkTask};
use mitmproxy_rs::packet_sources::{
    PacketSourceBuilder, PacketSourceTask, WinDivertBuilder, WireGuardBuilder,
};
use mitmproxy_rs::shutdown::ShutdownTask;
use crate::task::PyInteropTask;
use crate::tcp_stream::event_queue_unavailable;
use crate::util::{py_to_socketaddr, socketaddr_to_py, string_to_key};

#[derive(Debug)]
pub struct Server {
    /// queue of events to be sent to the Python interop task
    event_tx: mpsc::UnboundedSender<TransportCommand>,
    /// channel for notifying subtasks of requested server shutdown
    sd_trigger: BroadcastSender<()>,
    /// channel for getting notified of successful server shutdown
    sd_barrier: Arc<Notify>,
    /// flag to indicate whether server shutdown is in progress
    closing: bool,
}

impl Server {
    pub fn send_datagram(
        &self,
        data: Vec<u8>,
        src_addr: &PyTuple,
        dst_addr: &PyTuple,
    ) -> PyResult<()> {
        let cmd = TransportCommand::SendDatagram {
            data,
            src_addr: py_to_socketaddr(src_addr)?,
            dst_addr: py_to_socketaddr(dst_addr)?,
        };

        self.event_tx.send(cmd).map_err(event_queue_unavailable)?;
        Ok(())
    }

    pub fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            log::info!("Shutting down.");

            // notify tasks to shut down
            let _ = self.sd_trigger.send(());
        }
    }

    pub fn wait_closed<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let barrier = self.sd_barrier.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            barrier.notified().await;
            Ok(())
        })
    }
}

impl Server {
    /// Set up and initialize a new WireGuard server.
    pub async fn init(
        packet_source_builder: impl PacketSourceBuilder,
        py_tcp_handler: PyObject,
        py_udp_handler: PyObject,
    ) -> Result<Self> {
        log::debug!("Initializing WireGuard server ...");

        // initialize channels between the WireGuard server and the virtual network device
        let (wg_to_smol_tx, wg_to_smol_rx) = channel(256);
        let (smol_to_wg_tx, smol_to_wg_rx) = channel(256);

        // initialize channels between the virtual network device and the python interop task
        // - only used to notify of incoming connections and datagrams
        let (smol_to_py_tx, smol_to_py_rx) = channel(256);
        // - used to send data and to ask for packets
        // This channel needs to be unbounded because write() is not async.
        let (py_to_smol_tx, py_to_smol_rx) = unbounded_channel();

        let event_tx = py_to_smol_tx.clone();

        // initialize barriers for handling graceful shutdown
        let (sd_trigger, _sd_watcher) = broadcast::channel(1);
        let sd_barrier = Arc::new(Notify::new());

        let wg_task =
            packet_source_builder.build(wg_to_smol_tx, smol_to_wg_rx, sd_trigger.subscribe());

        // initialize virtual network device
        let nw_task = NetworkTask::new(
            smol_to_wg_tx,
            wg_to_smol_rx,
            smol_to_py_tx,
            py_to_smol_rx,
            sd_trigger.subscribe(),
        )?;

        // initialize Python interop task
        // Note: The current asyncio event loop needs to be determined here on the main thread.
        let py_loop: PyObject = Python::with_gil(|py| {
            let py_loop = pyo3_asyncio::tokio::get_current_loop(py)?.into_py(py);
            Ok::<PyObject, PyErr>(py_loop)
        })?;

        let py_task = PyInteropTask::new(
            py_loop,
            py_to_smol_tx,
            smol_to_py_rx,
            py_tcp_handler,
            py_udp_handler,
            sd_trigger.subscribe(),
        );

        // spawn tasks
        let wg_handle = tokio::spawn(async move { wg_task.run().await });
        let net_handle = tokio::spawn(async move { nw_task.run().await });
        let py_handle = tokio::spawn(async move { py_task.run().await });

        // initialize and run shutdown handler
        let sd_task = ShutdownTask::new(
            py_handle,
            wg_handle,
            net_handle,
            sd_trigger.clone(),
            sd_barrier.clone(),
        );
        tokio::spawn(async move { sd_task.run().await });

        log::debug!("WireGuard server successfully initialized.");

        Ok(Server {
            event_tx,
            sd_trigger,
            sd_barrier,
            closing: false,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close()
    }
}

#[pyclass]
#[derive(Debug)]
pub struct WindowsProxy {
    server: Server,
}

#[pymethods]
impl WindowsProxy {
    pub fn send_datagram(
        &self,
        data: Vec<u8>,
        src_addr: &PyTuple,
        dst_addr: &PyTuple,
    ) -> PyResult<()> {
        self.server.send_datagram(data, src_addr, dst_addr)
    }

    pub fn close(&mut self) {
        self.server.close()
    }

    pub fn wait_closed<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        self.server.wait_closed(py)
    }
}

impl WindowsProxy {
    pub async fn init(py_tcp_handler: PyObject, py_udp_handler: PyObject) -> Result<Self> {
        let pipe_name = format!(
            r"\\.\pipe\mitmproxy-transparent-proxy-{}",
            std::process::id()
        );

        let pipe_name = r"\\.\pipe\mitmproxy-transparent-proxy";

        let server = ServerOptions::new()
            .pipe_mode(PipeMode::Message)
            .first_pipe_instance(true)
            .max_instances(1)
            .in_buffer_size((MAX_PACKET_SIZE + 1) as u32)
            .out_buffer_size((MAX_PACKET_SIZE + 1) as u32)
            .create(pipe_name)?;

        unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                w!("cmd.exe"),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }


        let windows_task_builder = WinDivertBuilder::new(server);

        let server = Server::init(windows_task_builder, py_tcp_handler, py_udp_handler).await?;
        Ok(WindowsProxy { server })
    }
}

/// A running WireGuard server.
///
/// A new server can be started by calling the `start_server` coroutine. Its public API is intended
/// to be similar to the API provided by
/// [`asyncio.Server`](https://docs.python.org/3/library/asyncio-eventloop.html#asyncio.Server)
/// from the Python standard library.
#[pyclass]
#[derive(Debug)]
pub struct WireGuardServer {
    /// local address of the WireGuard UDP socket
    local_addr: SocketAddr,
    server: Server,
}

#[pymethods]
impl WireGuardServer {
    /// Send an individual UDP datagram using the specified source and destination addresses.
    ///
    /// The `src_addr` and `dst_addr` arguments are expected to be `(host: str, port: int)` tuples.
    pub fn send_datagram(
        &self,
        data: Vec<u8>,
        src_addr: &PyTuple,
        dst_addr: &PyTuple,
    ) -> PyResult<()> {
        self.server.send_datagram(data, src_addr, dst_addr)
    }

    /// Request the WireGuard server to gracefully shut down.
    ///
    /// The server will stop accepting new connections on its UDP socket, but will flush pending
    /// outgoing data before shutting down.
    pub fn close(&mut self) {
        self.server.close()
    }

    /// Wait until the WireGuard server has shut down.
    ///
    /// This coroutine will yield once pending data has been flushed and all server tasks have
    /// successfully terminated after calling the `Server.close` method.
    pub fn wait_closed<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        self.server.wait_closed(py)
    }

    /// Get the local socket address that the WireGuard server is listening on.
    pub fn getsockname(&self, py: Python) -> PyObject {
        socketaddr_to_py(py, self.local_addr)
    }

    pub fn __repr__(&self) -> String {
        format!("WireGuardServer({})", self.local_addr)
    }
}

impl WireGuardServer {
    pub async fn init(
        host: String,
        port: u16,
        private_key: String,
        peer_public_keys: Vec<String>,
        py_tcp_handler: PyObject,
        py_udp_handler: PyObject,
    ) -> Result<Self> {
        let private_key = string_to_key(private_key)?;

        let peer_public_keys = peer_public_keys
            .into_iter()
            .map(string_to_key)
            .collect::<PyResult<Vec<PublicKey>>>()?;

        // bind to UDP socket(s)
        let socket_addrs = if host.is_empty() {
            vec![
                SocketAddr::new("0.0.0.0".parse().unwrap(), port),
                SocketAddr::new("::".parse().unwrap(), port),
            ]
        } else {
            vec![SocketAddr::new(host.parse()?, port)]
        };

        let socket = UdpSocket::bind(socket_addrs.as_slice()).await?;
        let local_addr = socket.local_addr()?;

        log::debug!(
            "WireGuard server listening for UDP connections on {} ...",
            socket_addrs
                .iter()
                .map(|addr| addr.to_string())
                .collect::<Vec<String>>()
                .join(" and ")
        );

        // initialize WireGuard server
        let mut wg_task_builder = WireGuardBuilder::new(socket, private_key);
        for key in peer_public_keys {
            wg_task_builder.add_peer(key, None)?;
        }

        let server = Server::init(wg_task_builder, py_tcp_handler, py_udp_handler).await?;
        Ok(WireGuardServer { local_addr, server })
    }
}


/// Start a WireGuard server that is configured with the given parameters:
///
/// - `host`: The host address for the WireGuard UDP socket.
/// - `port`: The listen port for the WireGuard server. The default port for WireGuard is `51820`.
/// - `private_key`: The private X25519 key for the WireGuard server as a base64-encoded string.
/// - `peer_public_keys`: List of public X25519 keys for WireGuard peers as base64-encoded strings.
/// - `handle_connection`: A coroutine that will be called for each new `TcpStream`.
/// - `receive_datagram`: A function that will be called for each received UDP datagram.
///
/// The `receive_datagram` function will be called with the following arguments:
///
/// - payload of the UDP datagram as `bytes`
/// - source address as `(host: str, port: int)` tuple
/// - destination address as `(host: str, port: int)` tuple
#[pyfunction]
pub fn start_server(
    py: Python<'_>,
    host: String,
    port: u16,
    private_key: String,
    peer_public_keys: Vec<String>,
    handle_connection: PyObject,
    receive_datagram: PyObject,
) -> PyResult<&PyAny> {
    pyo3_asyncio::tokio::future_into_py(py, async move {
        let server = WireGuardServer::init(
            host,
            port,
            private_key,
            peer_public_keys,
            handle_connection,
            receive_datagram,
        )
            .await?;
        Ok(server)
    })
}

#[pyfunction]
pub fn start_windows_transparent_proxy(
    py: Python<'_>,
    handle_connection: PyObject,
    receive_datagram: PyObject,
) -> PyResult<&PyAny> {
    pyo3_asyncio::tokio::future_into_py(py, async move {
        let server = WindowsProxy::init(handle_connection, receive_datagram).await?;
        Ok(server)
    })
}
