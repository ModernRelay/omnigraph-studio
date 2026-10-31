//! Who waits for a write the gateway holds for the person (spec 013). The gateway gives the
//! write up, unsent, once nobody waits for it: when the CLI that sent it closes its connection.
//! On Unix the agent's wrapper `exec`s the CLI, so a command the agent stops by its process id
//! is the CLI, and its connection closes. On Windows the wrapper is a batch file and the CLI
//! runs under `cmd`: stopped by its process id, `cmd` ends and the CLI goes on waiting, its
//! connection open, so the person could still allow a write the agent was told had failed. The
//! agent's job ends the CLI only when the agent itself ends. So on Windows the gateway also
//! holds the processes the CLI was started under, from its parent up to Studio, as they are
//! when the write comes, and gives the write up when one of them ends: whoever ran the command
//! no longer waits for it.

use std::net::SocketAddr;

/// How often the processes are looked at while a write waits for the person.
#[cfg(windows)]
const EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// The processes a gateway client was started under, held as they were when its request came.
pub(crate) struct Starters {
    #[cfg(windows)]
    held: Vec<windows::Process>,
}

impl Starters {
    /// The processes the client of the connection from `client` to `server` was started under,
    /// from its parent up to Studio. None on other platforms, nor when Studio cannot tell which
    /// process the client is.
    pub(crate) async fn of(client: Option<SocketAddr>, server: Option<SocketAddr>) -> Self {
        #[cfg(windows)]
        if let (Some(SocketAddr::V4(client)), Some(SocketAddr::V4(server))) = (client, server) {
            return Self {
                held: smol::unblock(move || windows::starters(client, server)).await,
            };
        }
        #[cfg(not(windows))]
        let _ = (client, server);
        Self {
            #[cfg(windows)]
            held: Vec::new(),
        }
    }

    /// Resolves once one of them has ended; never when there are none.
    pub(crate) async fn ended(self) {
        #[cfg(windows)]
        if !self.held.is_empty() {
            while !self.held.iter().any(windows::Process::ended) {
                smol::Timer::after(EVERY).await;
            }
            return;
        }
        std::future::pending::<()>().await
    }
}

/// The process holding the client end of the IPv4 connection from `client` to `server`, in
/// the rows of an IPv4 table of TCP connections and their owners as Windows lists them
/// (`MIB_TCPTABLE_OWNER_PID`): the number of rows, then six fields to a row, the state, the
/// local and remote addresses and ports, and the owner's process id. Addresses and ports are
/// in network byte order, a port in the low two bytes of its field. Read as fields, so no row
/// is read past the table Windows wrote.
#[cfg(any(windows, test))]
fn owner(
    table: &[u32],
    client: std::net::SocketAddrV4,
    server: std::net::SocketAddrV4,
) -> Option<u32> {
    let port = |field: u32| u16::from_be(field as u16);
    let count = usize::try_from(*table.first()?).ok()?;
    let (rows, _) = table.get(1..)?.as_chunks::<6>();
    rows.iter()
        .take(count)
        .find(|row| {
            row[1].to_ne_bytes() == client.ip().octets()
                && port(row[2]) == client.port()
                && row[3].to_ne_bytes() == server.ip().octets()
                && port(row[4]) == server.port()
        })
        .map(|row| row[5])
}

#[cfg(windows)]
mod windows {
    use std::collections::HashMap;
    use std::net::SocketAddrV4;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // `owner` reads the table as its fields: a count, then rows of six.
    const _: () = assert!(size_of::<MIB_TCPROW_OWNER_PID>() == 6 * size_of::<u32>());
    const _: () = assert!(std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table) == size_of::<u32>());

    /// The most processes above the client held: a chain from the CLI to Studio is a handful
    /// (`cmd`, the agent's shell, the agent, the launcher).
    const DEPTH: usize = 16;

    /// A process held open: while the handle is held, its number is not given to another, and
    /// it can be asked whether it ended.
    pub(super) struct Process(HANDLE);

    // SAFETY: a process handle names a kernel object, usable from any thread; this one is only
    // waited on, queried and closed.
    unsafe impl Send for Process {}

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: the handle is this value's own, from `OpenProcess`, and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    impl Process {
        fn open(pid: u32) -> Option<Self> {
            let access = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
            // SAFETY: plain values in, a handle or null out.
            let handle = unsafe { OpenProcess(access, 0, pid) };
            (!handle.is_null()).then_some(Self(handle))
        }

        /// When it started, in 100-nanosecond steps.
        fn started(&self) -> Option<u64> {
            let mut times = [FILETIME::default(); 4];
            let [created, exited, kernel, user] = &mut times;
            // SAFETY: four times the call writes, and a handle with the query right.
            let read = unsafe { GetProcessTimes(self.0, created, exited, kernel, user) };
            (read != 0)
                .then(|| u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
        }

        pub(super) fn ended(&self) -> bool {
            // SAFETY: a zero wait only asks; the handle has the synchronize right.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    /// The process holding the client end of the connection from `client` to `server`.
    fn client_process(client: SocketAddrV4, server: SocketAddrV4) -> Option<u32> {
        let mut size = 0u32;
        // The table can grow between the call that sizes it and the one that fills it.
        for _ in 0..4 {
            let mut table = vec![0u32; (size as usize).div_ceil(size_of::<u32>())];
            // SAFETY: the buffer holds at least `size` bytes, as the call is told, and is
            // aligned for the table's fields; asked with none, the call only says how many.
            let result = unsafe {
                GetExtendedTcpTable(
                    table.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    u32::from(AF_INET),
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            match result {
                NO_ERROR => return super::owner(&table, client, server),
                ERROR_INSUFFICIENT_BUFFER => {}
                _ => return None,
            }
        }
        None
    }

    /// Each running process's parent, by process id.
    fn parents() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        // SAFETY: plain values in, a snapshot handle or `INVALID_HANDLE_VALUE` out.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return parents;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: the snapshot is open, and `entry` is a sized entry the calls fill.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: the snapshot handle is this function's own, closed once.
        unsafe { CloseHandle(snapshot) };
        parents
    }

    /// The processes the client of the connection from `client` to `server` was started
    /// under, from its parent up to Studio, each held open; none when Studio cannot tell which
    /// process the client is. The walk stops at a parent that is gone: a process started after
    /// its child holds the number of a parent that ended, and a process whose parent ended
    /// long ago is ordinary on Windows (Explorer's, for one), so a parent already gone is not
    /// taken for a command stopped.
    pub(super) fn starters(client: SocketAddrV4, server: SocketAddrV4) -> Vec<Process> {
        let mut chain = Vec::new();
        let Some(mut pid) = client_process(client, server) else {
            return chain;
        };
        let Some(mut child_started) = Process::open(pid).and_then(|client| client.started()) else {
            return chain;
        };
        let parents = parents();
        // SAFETY: no arguments.
        let studio = unsafe { GetCurrentProcessId() };
        while chain.len() < DEPTH {
            let Some(&parent) = parents.get(&pid) else {
                break;
            };
            if parent == studio || parent == 0 || parent == pid {
                break;
            }
            let Some(process) = Process::open(parent) else {
                break;
            };
            match process.started() {
                Some(started) if started <= child_started => child_started = started,
                _ => break,
            }
            chain.push(process);
            pid = parent;
        }
        chain
    }

    #[cfg(test)]
    pub(super) fn held(starters: &super::Starters) -> usize {
        starters.held.len()
    }
}

/// A CLI for the Windows tests that waits for a write's answer: this test binary, started
/// through the agent's batch-file wrapper, connects to the gateway's port, sends a request (over
/// TLS, trusting the certificate its home holds, as the CLI does), and waits until the
/// connection closes.
#[cfg(all(test, windows))]
pub(crate) mod stand_in {
    use std::path::{Path, PathBuf};

    /// Set for this test binary when it runs as the CLI, to the port it connects to.
    const PORT: &str = "STUDIO_TEST_WAITING_CLI_PORT";
    /// A file holding what it sends once connected.
    const SENDS: &str = "STUDIO_TEST_WAITING_CLI_SENDS";
    /// The test that is the CLI when this test binary runs as one.
    const TEST: &str = "requester::stand_in::stands_in_for_a_waiting_cli";

    /// What [`start`] runs this test binary as. Run as a test, it does nothing.
    #[test]
    fn stands_in_for_a_waiting_cli() {
        use std::io::Write as _;
        let Some(port) = std::env::var(PORT)
            .ok()
            .and_then(|port| port.parse::<u16>().ok())
        else {
            return;
        };
        let connection = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let Some(sends) = std::env::var_os(SENDS) else {
            wait(connection);
        };
        // As the CLI reaches the gateway: over TLS, trusting the certificate its home holds.
        let home = std::env::var_os("OMNIGRAPH_HOME").unwrap();
        let ca = std::fs::read_to_string(Path::new(&home).join("ca.pem")).unwrap();
        let session = futures_rustls::rustls::ClientConnection::new(
            crate::gateway_tls::client_trusting(&ca),
            crate::gateway_tls::loopback(),
        )
        .unwrap();
        let mut stream = futures_rustls::rustls::StreamOwned::new(session, connection);
        stream.write_all(&std::fs::read(sends).unwrap()).unwrap();
        stream.flush().unwrap();
        wait(stream);
    }

    /// Reads `connection` until it closes, then ends this process.
    fn wait(mut connection: impl std::io::Read) -> ! {
        let mut byte = [0u8; 1];
        while matches!(connection.read(&mut byte), Ok(1..)) {}
        std::process::exit(0);
    }

    /// A conversation's home under `root` for the gateway on `port`, whose wrapper runs this test
    /// binary, and the wrapper started as that CLI, sending a request to the gateway whose
    /// certificate is given, when one is.
    pub(crate) fn start(
        root: &Path,
        port: u16,
        sends: Option<(&[u8], &str)>,
    ) -> (crate::cli_home::CliHome, std::process::Child) {
        std::fs::remove_dir_all(root).ok();
        std::fs::create_dir_all(root).unwrap();
        let home = crate::cli_home::CliHome::write(
            root,
            crate::cli_home::GatewayAccess {
                port,
                token: "t",
                certificate: sends.map_or("", |(_, certificate)| certificate),
            },
            "knowledge",
        )
        .unwrap();
        assert!(home.use_cli(&std::env::current_exe().unwrap()).unwrap());
        let wrapper = run(&home.command(), port, sends.map(|(sends, _)| (root, sends)));
        (home, wrapper)
    }

    /// `program` started as the CLI for `port`: the wrapper, or this test binary itself.
    pub(crate) fn run(
        program: &Path,
        port: u16,
        sends: Option<(&Path, &[u8])>,
    ) -> std::process::Child {
        let mut command = std::process::Command::new(program);
        command
            .args(["--exact", TEST, "--nocapture", "--"])
            .env(PORT, port.to_string())
            .stdout(std::process::Stdio::null());
        if let Some((root, sends)) = sends {
            let file: PathBuf = root.join("sends");
            std::fs::write(&file, sends).unwrap();
            command.env(SENDS, file);
        }
        command.spawn().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// A row of Windows's table of TCP connections, its fields as Windows writes them.
    fn row(local: SocketAddrV4, remote: SocketAddrV4, pid: u32) -> [u32; 6] {
        let address = |at: SocketAddrV4| u32::from_ne_bytes(at.ip().octets());
        let port = |at: SocketAddrV4| u32::from(at.port().to_be());
        [
            5,
            address(local),
            port(local),
            address(remote),
            port(remote),
            pid,
        ]
    }

    /// The client is the process holding the connection's client end: its local address and
    /// port are the client's, its remote ones the gateway's. The gateway's own end, the other
    /// way round, and rows past the count or the table are not it.
    #[test]
    fn the_client_is_the_owner_of_the_connections_client_end() {
        let loopback = Ipv4Addr::LOCALHOST;
        let gateway = SocketAddrV4::new(loopback, 49152);
        let cli = SocketAddrV4::new(loopback, 61000);
        let other = SocketAddrV4::new(loopback, 61001);
        let rows = [
            row(gateway, cli, 100),
            row(other, gateway, 300),
            row(cli, gateway, 200),
        ];
        let table = |count: u32| -> Vec<u32> {
            std::iter::once(count)
                .chain(rows.iter().flatten().copied())
                .collect()
        };
        assert_eq!(owner(&table(3), cli, gateway), Some(200));
        assert_eq!(owner(&table(3), other, gateway), Some(300));
        assert_eq!(owner(&table(2), cli, gateway), None, "past the count");
        assert_eq!(owner(&table(9)[..13], cli, gateway), None, "past the table");
        assert_eq!(owner(&[], cli, gateway), None);
    }

    /// Whether `ended` resolves within `wait`.
    #[cfg(windows)]
    fn ends_within(
        mut ended: &mut (impl futures::future::FusedFuture<Output = ()> + Unpin),
        wait: std::time::Duration,
    ) -> bool {
        smol::block_on(async {
            futures::select! {
                () = ended => true,
                _ = futures::FutureExt::fuse(smol::Timer::after(wait)) => false,
            }
        })
    }

    /// The listener's next connection, within a minute.
    #[cfg(windows)]
    fn accept(listener: &std::net::TcpListener) -> std::net::TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match listener.accept() {
                Ok((connection, _)) => {
                    connection.set_nonblocking(false).unwrap();
                    return connection;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the CLI never connected"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    /// The agent stops a command run through its batch-file wrapper by the wrapper's process
    /// id: `cmd` ends, and the CLI under it goes on, its connection open. The processes held
    /// when its request came say one of them ended. Until then they say nothing, and a CLI
    /// Studio started itself has none.
    #[cfg(windows)]
    #[test]
    fn a_command_stopped_by_its_id_is_seen_though_the_cli_waits_on() {
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("studio-requester-{}", std::process::id()));
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (home, mut wrapper) = stand_in::start(&root, port, None);
        let connection = accept(&listener);
        let starters = smol::block_on(Starters::of(
            connection.peer_addr().ok(),
            connection.local_addr().ok(),
        ));
        assert_eq!(
            windows::held(&starters),
            1,
            "cmd, under this test as Studio"
        );
        let mut ended = Box::pin(futures::FutureExt::fuse(starters.ended()));
        assert!(!ends_within(&mut ended, Duration::from_millis(600)));

        wrapper.kill().unwrap();
        wrapper.wait().unwrap();
        connection.set_nonblocking(true).unwrap();
        let read = std::io::Read::read(&mut &connection, &mut [0u8; 1]);
        assert!(
            matches!(&read, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "the CLI still holds its connection: {read:?}"
        );
        assert!(ends_within(&mut ended, Duration::from_secs(10)));
        // Closing the connection ends the stand-in.
        drop(connection);

        let cli = std::env::current_exe().unwrap();
        let mut direct = stand_in::run(&cli, port, None);
        let connection = accept(&listener);
        let starters = smol::block_on(Starters::of(
            connection.peer_addr().ok(),
            connection.local_addr().ok(),
        ));
        assert_eq!(
            windows::held(&starters),
            0,
            "watched by its connection alone"
        );
        drop(connection);
        direct.wait().unwrap();
        drop(home);
        std::fs::remove_dir_all(&root).ok();
    }
}
