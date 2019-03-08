/*
 * Copyright 2019 Reiner Herrmann <reiner@reiner-h.de>
 * License: GPL-3+
 */

use std::env;
use std::error::Error;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

extern crate nix;
use nix::unistd::{setresgid, setresuid, Gid, Uid};

extern crate getopts;
use getopts::Options;

extern crate threadpool;
use threadpool::ThreadPool;

extern crate rtftp;

#[derive(Clone)]
struct Configuration {
    port: u16,
    uid: u32,
    gid: u32,
    ro: bool,
    wo: bool,
    threads: usize,
    dir: PathBuf,
}

#[derive(Clone)]
struct Tftpd {
    tftp: rtftp::Tftp,
    conf: Configuration,
}

impl Tftpd {
    pub fn new(conf: Configuration) -> Tftpd {
        Tftpd {
            tftp: rtftp::Tftp::new(),
            conf,
        }
    }

    fn file_allowed(&self, filename: &Path) -> Option<PathBuf> {
        /* get parent to check dir where file should be read/written */
        let path = Path::new(".").join(filename);
        let path = match path.parent() {
            Some(p) => p,
            None => return None,
        };
        let path = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => return None,
        };

        /* get last component to append to canonicalized path */
        let filename = match filename.file_name() {
            Some(f) => f,
            None => return None,
        };
        let path = path.join(filename);

        let cwd = match env::current_dir() {
            Ok(p) => p,
            Err(_) => return None,
        };

        match path.strip_prefix(cwd) {
            Ok(p) => Some(p.to_path_buf()),
            Err(_) => None,
        }
    }

    fn handle_wrq(&mut self, socket: &UdpSocket, cl: &SocketAddr, buf: &[u8]) -> Result<(String), io::Error> {
        let (filename, mode, mut options) = self.tftp.parse_file_mode_options(buf)?;
        self.tftp.init_tftp_options(&socket, &mut options)?;

        match mode.as_ref() {
            "octet" => (),
            _ => {
                self.tftp.send_error(&socket, 0, "Unsupported mode")?;
                return Err(io::Error::new(io::ErrorKind::Other, "unsupported mode"));
            }
        }

        let path = match self.file_allowed(&filename) {
            Some(p) => p,
            None => {
                let err = format!("Sending {} to {} failed (permission check failed).", filename.display(), cl);
                self.tftp.send_error(&socket, 2, "Permission denied")?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, err));
            }
        };

        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(ref err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let error = format!("Receiving {} from {} failed ({}).", path.display(), cl, err);
                self.tftp.send_error(&socket, 6, "File already exists")?;
                return Err(io::Error::new(err.kind(), error));
            }
            Err(err) => {
                let error = format!("Receiving {} from {} failed ({}).", path.display(), cl, err);
                self.tftp.send_error(&socket, 6, "Permission denied")?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, error));
            }
        };

        self.tftp.ack_options(&socket, &options, false)?;
        match self.tftp.recv_file(&socket, &mut file) {
            Ok(_) => Ok(format!("Received {} from {}.", path.display(), cl)),
            Err(ref err) => {
                let error = format!("Receiving {} from {} failed ({}).", path.display(), cl, err);
                self.tftp.send_error(&socket, 0, "Receiving error")?;
                Err(io::Error::new(err.kind(), error))
            }
        }
    }

    fn handle_rrq(&mut self, socket: &UdpSocket, cl: &SocketAddr, buf: &[u8]) -> Result<(String), io::Error> {
        let (filename, mode, mut options) = self.tftp.parse_file_mode_options(buf)?;
        self.tftp.init_tftp_options(&socket, &mut options)?;

        match mode.as_ref() {
            "octet" => (),
            _ => {
                self.tftp.send_error(&socket, 0, "Unsupported mode")?;
                return Err(io::Error::new(io::ErrorKind::Other, "unsupported mode"));
            }
        }

        let path = match self.file_allowed(&filename) {
            Some(p) => p,
            None => {
                let err = format!("Sending {} to {} failed (permission check failed).", filename.display(), cl);
                self.tftp.send_error(&socket, 2, "Permission denied")?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, err));
            }
        };

        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(ref error) if error.kind() == io::ErrorKind::NotFound => {
                let err = format!("Sending {} to {} failed ({}).", path.display(), cl, error.to_string());
                self.tftp.send_error(&socket, 1, "File not found")?;
                return Err(io::Error::new(io::ErrorKind::NotFound, err));
            }
            Err(error) => {
                let err = format!("Sending {} to {} failed ({}).", path.display(), cl, error.to_string());
                self.tftp.send_error(&socket, 2, "Permission denied")?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, err));
            }
        };
        if !file.metadata()?.is_file() {
            self.tftp.send_error(&socket, 1, "File not found")?;
            return Err(io::Error::new(io::ErrorKind::NotFound, "file not found"));
        }

        if let Some(opt) = options.get_mut("tsize") {
            *opt = file.metadata()?.len().to_string();
        }
        self.tftp.ack_options(&socket, &options, true)?;
        match self.tftp.send_file(&socket, &mut file) {
            Ok(_) => Ok(format!("Sent {} to {}.", path.display(), cl)),
            Err(err) => {
                let error = format!("Sending {} to {} failed ({}).", path.display(), cl, err.to_string());
                Err(std::io::Error::new(err.kind(), error))
            }
        }
    }

    pub fn handle_client(&mut self, cl: &SocketAddr, buf: &[u8]) -> Result<String, io::Error> {
        let socket = UdpSocket::bind("[::]:0")?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        socket.connect(cl)?;

        match u16::from_be_bytes([buf[0], buf[1]]) {  // opcode
            o if o == rtftp::Opcodes::RRQ as u16 => {
                if self.conf.wo {
                    self.tftp.send_error(&socket, 4, "reading not allowed")?;
                    Err(io::Error::new(io::ErrorKind::Other, "unallowed mode"))
                } else {
                    self.handle_rrq(&socket, &cl, &buf[2..])
                }
            }
            o if o == rtftp::Opcodes::WRQ as u16 => {
                if self.conf.ro {
                    self.tftp.send_error(&socket, 4, "writing not allowed")?;
                    Err(io::Error::new(io::ErrorKind::Other, "unallowed mode"))
                } else {
                    self.handle_wrq(&socket, &cl, &buf[2..])
                }
            }
            o if o == rtftp::Opcodes::ERROR as u16 => Ok(format!("Received ERROR from {}", cl)),
            _ => {
                self.tftp.send_error(&socket, 4, "Unexpected opcode")?;
                Err(io::Error::new(io::ErrorKind::Other, "unexpected opcode"))
            }
        }
    }

    fn drop_privs(&self, uid: u32, gid: u32) -> Result<(), Box<Error>> {
        let root_uid = Uid::from_raw(0);
        let root_gid = Gid::from_raw(0);
        let unpriv_uid = Uid::from_raw(uid);
        let unpriv_gid = Gid::from_raw(gid);

        if Gid::current() != root_gid
            && Gid::effective() != root_gid
            && Uid::current() != root_uid
            && Uid::effective() != root_uid
        {
            /* already unprivileged user */
            return Ok(());
        }

        if Gid::current() == root_gid || Gid::effective() == root_gid {
            setresgid(unpriv_gid, unpriv_gid, unpriv_gid)?;
        }

        if Uid::current() == root_uid || Uid::effective() == root_uid {
            setresuid(unpriv_uid, unpriv_uid, unpriv_uid)?;
        }

        Ok(())
    }

    pub fn start(&mut self) {
        let socket = match UdpSocket::bind(format!("[::]:{}", self.conf.port)) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("Binding a socket failed: {}", err);
                return;
            }
        };
        match self.drop_privs(self.conf.uid, self.conf.gid) {
            Ok(_) => (),
            Err(err) => {
                eprintln!("Dropping privileges failed: {}", err);
                return;
            }
        };

        match env::set_current_dir(&self.conf.dir) {
            Ok(_) => (),
            Err(err) => {
                eprintln!("Changing directory to {} failed ({}).", &self.conf.dir.display(), err);
                return;
            }
        }

        let pool = ThreadPool::new(self.conf.threads);
        loop {
            let mut buf = [0; 2048];
            let (n, src) = match socket.recv_from(&mut buf) {
                Ok(args) => args,
                Err(err) => {
                    eprintln!("Receiving data from socket failed: {}", err);
                    break;
                }
            };

            let mut worker = self.clone();
            pool.execute(move || {
                match worker.handle_client(&src, &buf[0..n]) {
                    Ok(msg) => println!("{}", msg),
                    Err(err) => println!("{}", err),
                }
            });
        }
    }
}

fn usage(opts: Options, program: String, error: Option<String>) {
    if let Some(err) = error {
        println!("{}\n", err);
    }
    println!("{}", opts.usage(format!("RusTFTP\n\n{} [options]", program).as_str()));
}

fn parse_commandline(args: &[String]) -> Result<Configuration, &str> {
    let program = args[0].clone();
    let mut conf = Configuration {
        port: 69,
        uid: 65534,
        gid: 65534,
        ro: false,
        wo: false,
        threads: 2,
        dir: env::current_dir().expect("Can't get current directory"),
    };
    let mut opts = Options::new();
    opts.optflag("h", "help", "display usage information");
    opts.optopt("d", "directory", "directory to serve (default: current directory)", "DIRECTORY");
    opts.optopt("p", "port", format!("port to listen on (default: {})", conf.port).as_ref(), "PORT");
    opts.optopt("u", "uid", format!("user id to run as (default: {})", conf.uid).as_ref(), "UID");
    opts.optopt("g", "gid", format!("group id to run as (default: {})", conf.gid).as_ref(), "GID");
    opts.optflag("r", "read-only", "allow only reading/downloading of files (RRQ)");
    opts.optflag("w", "write-only", "allow only writing/uploading of files (WRQ)");
    opts.optopt("t", "threads", format!("number of worker threads (default: {})", conf.threads).as_ref(), "N");
    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(err) => {
            usage(opts, program, Some(err.to_string()));
            return Err("Parsing error");
        }
    };
    if matches.opt_present("h") {
        usage(opts, program, None);
        return Err("usage");
    }

    conf.port = match matches.opt_get_default("p", conf.port) {
        Ok(p) => p,
        Err(err) => {
            usage(opts, program, Some(err.to_string()));
            return Err("port");
        }
    };
    conf.uid = match matches.opt_get_default("u", conf.uid) {
        Ok(u) => u,
        Err(err) => {
            usage(opts, program, Some(err.to_string()));
            return Err("uid");
        }
    };
    conf.gid = match matches.opt_get_default("g", conf.gid) {
        Ok(g) => g,
        Err(err) => {
            usage(opts, program, Some(err.to_string()));
            return Err("gid");
        }
    };
    conf.threads = match matches.opt_get_default("t", conf.threads) {
        Ok(t) => t,
        Err(err) => {
            usage(opts, program, Some(err.to_string()));
            return Err("threads");
        }
    };
    conf.ro = matches.opt_present("r");
    conf.wo = matches.opt_present("w");
    if conf.ro && conf.wo {
        usage(opts, program, Some(String::from("Only one of r (read-only) and w (write-only) allowed")));
        return Err("ro and wo");
    }
    if matches.opt_present("d") {
        conf.dir = match matches.opt_str("d") {
            Some(d) => Path::new(&d).to_path_buf(),
            None => {
                usage(opts, program, None);
                return Err("directory");
            }
        };
    }

    Ok(conf)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let conf = match parse_commandline(&args) {
        Ok(c) => c,
        Err(_) => return,
    };

    Tftpd::new(conf).start();
}
