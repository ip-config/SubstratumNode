// Copyright (c) 2017-2019, Substratum LLC (https://substratum.net) and/or its affiliates. All rights reserved.
extern crate node_lib;
extern crate sub_lib;
extern crate test_utils;

use self::sub_lib::utils::indicates_dead_stream;
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::env;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::process;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread;
use sub_lib::framer::Framer;
use sub_lib::main_tools::Command;
use sub_lib::main_tools::StdStreams;
use sub_lib::node_addr::NodeAddr;
use test_utils::data_hunk::DataHunk;
use test_utils::data_hunk_framer::DataHunkFramer;

pub const CONTROL_STREAM_PORT: u16 = 42511;

#[allow(dead_code)] // Lint says this isn't used. I say fooey.
pub fn main() {
    let mut streams: StdStreams = StdStreams {
        stdin: &mut io::stdin(),
        stdout: &mut io::stdout(),
        stderr: &mut io::stderr(),
    };
    let mut command = MockNode::new();
    let streams_ref: &mut StdStreams = &mut streams;
    let exit_code = command.go(streams_ref, &env::args().collect());
    process::exit(exit_code as i32);
}

struct MockNodeGuts {
    node_addr: NodeAddr,
    read_control_stream: TcpStream,
    write_control_stream_arc: Arc<Mutex<TcpStream>>,
    write_streams_arc: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
}

struct MockNode {
    control_stream_port: u16,
    guts: Option<MockNodeGuts>,
}

impl Command for MockNode {
    fn go(&mut self, streams: &mut StdStreams, args: &Vec<String>) -> u8 {
        let node_addr = match Self::interpret_args(args, streams.stderr) {
            Ok(p) => p,
            Err(msg) => {
                writeln!(streams.stderr, "{}", msg).unwrap();
                return 1;
            }
        };

        let listener = match TcpListener::bind(SocketAddr::new(
            node_addr.ip_addr(),
            self.control_stream_port,
        )) {
            Err(e) => {
                writeln!(
                    streams.stderr,
                    "Couldn't bind TcpListener to control port {}: {}",
                    self.control_stream_port, e
                )
                .unwrap();
                return 1;
            }
            Ok(listener) => listener,
        };
        let (control_stream, _) = match listener.accept() {
            Err(e) => {
                writeln!(
                    streams.stderr,
                    "Error accepting control stream on port {}: {}",
                    self.control_stream_port, e
                )
                .unwrap();
                return 1;
            }
            Ok(pair) => pair,
        };
        let write_control_stream = control_stream
            .try_clone()
            .expect("Error cloning control stream");
        self.guts = Some(MockNodeGuts {
            node_addr,
            read_control_stream: control_stream,
            write_control_stream_arc: Arc::new(Mutex::new(write_control_stream)),
            write_streams_arc: Arc::new(Mutex::new(HashMap::new())),
        });
        self.initialize(streams.stderr)
    }
}

impl MockNode {
    pub fn new() -> MockNode {
        MockNode {
            control_stream_port: CONTROL_STREAM_PORT,
            guts: None,
        }
    }

    pub fn node_addr(&self) -> &NodeAddr {
        &self.guts().node_addr
    }

    pub fn read_control_stream(&mut self) -> &mut TcpStream {
        &mut self.guts_mut().read_control_stream
    }

    pub fn write_control_stream(&self) -> MutexGuard<TcpStream> {
        self.guts()
            .write_control_stream_arc
            .lock()
            .expect("Write control stream poisoned")
    }

    pub fn write_control_stream_arc(&self) -> Arc<Mutex<TcpStream>> {
        self.guts().write_control_stream_arc.clone()
    }

    pub fn write_streams(&self) -> MutexGuard<HashMap<SocketAddr, TcpStream>> {
        self.guts()
            .write_streams_arc
            .lock()
            .expect("Write streams poisoned")
    }

    fn initialize(&mut self, stderr: &mut Write) -> u8 {
        let open_err_msgs = self
            .node_addr()
            .ports()
            .into_iter()
            .map(|port| self.open_port(port))
            .filter(|r| r.is_err())
            .map(|r| format!("{}", r.err().unwrap()))
            .collect::<Vec<String>>();
        if !open_err_msgs.is_empty() {
            writeln!(stderr, "{}", open_err_msgs.join("\n")).unwrap();
            return 1;
        }

        let local_addr = self.node_addr().ip_addr();
        let mut buf = [0u8; 65536];
        let mut framer = DataHunkFramer::new();
        loop {
            loop {
                match framer.take_frame() {
                    None => break,
                    Some(chunk) => {
                        let data_hunk: DataHunk = chunk.chunk.into();
                        let mut write_streams = self.write_streams();
                        if !write_streams.contains_key(&data_hunk.to) {
                            let mut stream = match TcpStream::connect(data_hunk.to) {
                                Err(e) => {
                                    writeln!(
                                        stderr,
                                        "Error connecting new stream from {} to {}, ignoring: {}",
                                        local_addr, data_hunk.to, e
                                    )
                                    .unwrap();
                                    continue;
                                }
                                Ok(s) => s,
                            };
                            write_streams.insert(data_hunk.to, stream.try_clone().unwrap());
                            Self::start_stream_reader(
                                stream,
                                self.write_control_stream_arc(),
                                data_hunk.to,
                            );
                        }
                        let mut write_stream = write_streams.get_mut(&data_hunk.to).unwrap();
                        if !Self::write_with_retry(write_stream, &data_hunk.data[..], data_hunk.to)
                        {
                            return 1;
                        }
                    }
                }
            }
            match self.read_control_stream().read(&mut buf) {
                Err(ref e) if indicates_dead_stream(e.kind()) => {
                    writeln!(stderr, "Read error from control stream: {}", e).unwrap();
                    self.write_control_stream()
                        .shutdown(Shutdown::Both)
                        .unwrap();
                    break;
                }
                Ok(len) => framer.add_data(&buf[..len]),
                _ => (),
            }
        }
        0
    }

    fn guts(&self) -> &MockNodeGuts {
        self.guts.as_ref().expect("MockNode uninitialized")
    }

    fn guts_mut(&mut self) -> &mut MockNodeGuts {
        self.guts.as_mut().expect("MockNode uninitialized")
    }

    fn usage(stderr: &mut Write) -> u8 {
        writeln! (stderr, "Usage: MockNode <IP address>:<port>,[<port>,...] where <IP address> is the address MockNode is running on and <port> is between 1024 and 65535").unwrap ();
        return 1;
    }

    fn interpret_args(args: &Vec<String>, stderr: &mut Write) -> Result<NodeAddr, String> {
        if args.len() != 2 {
            Self::usage(stderr);
            return Err(String::new());
        }
        let node_addr = NodeAddr::from_str(&args[1][..])?;
        Ok(node_addr)
    }

    fn open_port(&mut self, port: u16) -> Result<(), String> {
        let local_addr = SocketAddr::new(self.node_addr().ip_addr(), port);
        let listener = match TcpListener::bind(local_addr) {
            Err(e) => {
                return Err(format!(
                    "Couldn't bind TcpListener to {}: {}",
                    local_addr, e
                ))
            }
            Ok(listener) => listener,
        };
        let write_control_stream_arc = self.guts().write_control_stream_arc.clone();
        let write_streams_arc = self.guts().write_streams_arc.clone();
        thread::spawn(move || loop {
            let (stream, peer_addr) = match listener.accept() {
                Err(e) => {
                    eprintln!("Error accepting stream on port {}; continuing: {}", port, e);
                    continue;
                }
                Ok(p) => p,
            };
            {
                let mut write_streams = write_streams_arc
                    .lock()
                    .expect("write_streams_arc is poisoned");
                write_streams.insert(peer_addr, stream.try_clone().unwrap());
            }
            Self::start_stream_reader(stream, write_control_stream_arc.clone(), peer_addr);
        });
        Ok(())
    }

    fn start_stream_reader(
        mut stream: TcpStream,
        write_control_stream_arc: Arc<Mutex<TcpStream>>,
        peer_addr: SocketAddr,
    ) {
        thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match stream.read(&mut buf) {
                    Err(ref e) if indicates_dead_stream(e.kind()) => {
                        eprintln!("Read error from {}: {}", peer_addr, e);
                        stream.shutdown(Shutdown::Both).is_ok();
                        break;
                    }
                    Ok(0) => {
                        eprintln!("{} shut down stream", peer_addr);
                        stream.shutdown(Shutdown::Both).is_ok();
                    }
                    Ok(len) => {
                        let data_hunk = DataHunk::new(
                            peer_addr,
                            stream.local_addr().unwrap(),
                            Vec::from(&buf[..len]),
                        );
                        let serialized: Vec<u8> = data_hunk.into();
                        {
                            let mut write_control_stream = write_control_stream_arc
                                .lock()
                                .expect("Control stream poisoned");

                            if !Self::write_with_retry(
                                write_control_stream.borrow_mut(),
                                &serialized[..],
                                peer_addr,
                            ) {
                                break;
                            }
                        }
                    }
                    _ => (),
                }
            }
        });
    }

    fn write_with_retry(stream: &mut TcpStream, buf: &[u8], peer_addr: SocketAddr) -> bool {
        match stream.write(buf) {
            Err(ref e) if indicates_dead_stream(e.kind()) => {
                eprintln!("Write error to {}: {}", peer_addr, e);
                stream.shutdown(Shutdown::Both).is_ok();
                false
            }
            Err(_) => Self::write_with_retry(stream, buf, peer_addr),
            Ok(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::io::Write;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::Duration;
    use test_utils::test_utils::find_free_port;
    use test_utils::test_utils::FakeStreamHolder;

    #[test]
    fn cant_start_with_no_node_ref() {
        let mut holder = FakeStreamHolder::new();
        let mut subject = MockNode::new();

        let result = subject.go(&mut holder.streams(), &vec![String::from("binary")]);

        assert_eq!(result, 1);
        let stderr = holder.stderr;
        assert_eq! (stderr.get_string (), String::from ("Usage: MockNode <IP address>:<port>,[<port>,...] where <IP address> is the address MockNode is running on and <port> is between 1024 and 65535\n\n"));
    }

    #[test]
    fn cant_start_with_bad_node_ref() {
        let mut holder = FakeStreamHolder::new();
        let mut subject = MockNode::new();

        let result = subject.go(
            &mut holder.streams(),
            &vec![String::from("binary"), String::from("Booga")],
        );

        assert_eq!(result, 1);
        let stderr = holder.stderr;
        assert_eq!(
            stderr.get_string(),
            String::from(
                "NodeAddr should be expressed as '<IP address>:<port>,<port>,...', not 'Booga'\n"
            )
        );
    }

    #[test]
    fn opens_mentioned_port() {
        let control_stream_port = 42512;
        let clandestine_port = find_free_port();
        thread::spawn(move || {
            let mut subject = MockNode::new();
            subject.control_stream_port = control_stream_port;
            let mut streams: StdStreams = StdStreams {
                stdin: &mut io::stdin(),
                stdout: &mut io::stdout(),
                stderr: &mut io::stderr(),
            };
            subject.go(
                &mut streams,
                &vec![
                    String::from("binary"),
                    format!("127.0.0.1:{}", clandestine_port),
                ],
            );
        });
        thread::sleep(Duration::from_millis(100));
        let mut control_stream = TcpStream::connect(
            SocketAddr::from_str(format!("127.0.0.1:{}", control_stream_port).as_str()).unwrap(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        let mut write_stream = TcpStream::connect(
            SocketAddr::from_str(format!("127.0.0.1:{}", clandestine_port).as_str()).unwrap(),
        )
        .unwrap();
        let mut buf = [0u8; 100];

        write_stream.write(&[1, 2, 3, 4]).unwrap();

        let size = control_stream.read(&mut buf).unwrap();
        assert_eq!(size, 20);
        let data = Vec::from(&buf[..20]);
        let data_hunk: DataHunk = data.into();
        assert_eq!(data_hunk.from, write_stream.local_addr().unwrap());
        assert_eq!(data_hunk.to, write_stream.peer_addr().unwrap());
        assert_eq!(data_hunk.data, vec!(1, 2, 3, 4));
    }

    #[test]
    fn can_instruct_transmission_of_data() {
        let clandestine_port = find_free_port();
        let control_stream_port = 42514;
        thread::spawn(move || {
            let mut subject = MockNode::new();
            subject.control_stream_port = control_stream_port;
            let mut streams: StdStreams = StdStreams {
                stdin: &mut io::stdin(),
                stdout: &mut io::stdout(),
                stderr: &mut io::stderr(),
            };
            subject.go(
                &mut streams,
                &vec![
                    String::from("binary"),
                    format!("127.0.0.1:{}", clandestine_port),
                ],
            );
        });
        thread::sleep(Duration::from_millis(100));
        let mut control_stream = TcpStream::connect(
            SocketAddr::from_str(format!("127.0.0.1:{}", control_stream_port).as_str()).unwrap(),
        )
        .unwrap();
        let echo_server = TcpEchoServer::start();
        let data_hunk = DataHunk::new(
            control_stream.local_addr().unwrap(),
            SocketAddr::new(
                control_stream.local_addr().unwrap().ip(),
                echo_server.port(),
            ),
            vec![1, 2, 3, 4],
        );
        let data: Vec<u8> = data_hunk.into();

        control_stream.write(&data[..]).unwrap();

        let mut buf = [0u8; 16384];
        let size = control_stream.read(&mut buf).unwrap();
        assert_eq!(size, 20);
        let data = Vec::from(&buf[..20]);
        let data_hunk: DataHunk = data.into();
        assert_eq!(
            data_hunk.from,
            SocketAddr::new(
                control_stream.local_addr().unwrap().ip(),
                echo_server.port()
            )
        );
        assert_eq!(data_hunk.to.ip(), control_stream.local_addr().unwrap().ip());
        assert_eq!(data_hunk.data, vec!(1, 2, 3, 4));
    }

    struct TcpEchoServer {
        port: u16,
    }

    impl TcpEchoServer {
        pub fn start() -> TcpEchoServer {
            let listener =
                TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0))
                    .unwrap();
            let port = listener.local_addr().unwrap().port();
            thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let mut buf = [0u8; 1024];
                loop {
                    match listener.accept() {
                        Err(e) => {
                            println!(
                                "TcpEchoServer couldn't listen on port {}; retrying: {}",
                                port, e
                            );
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        Ok((mut stream, _)) => loop {
                            match stream.read(&mut buf) {
                                Err(e) => {
                                    println!("TcpEchoServer couldn't read: {}", e);
                                    break;
                                }
                                Ok(len) if len == 0 => break,
                                Ok(len) => stream.write(&buf[..len]).unwrap(),
                            };
                        },
                    }
                }
            });
            TcpEchoServer { port }
        }

        pub fn port(&self) -> u16 {
            self.port
        }
    }
}
