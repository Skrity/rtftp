use std::net::{SocketAddr,UdpSocket};
use std::fs::File;
use std::path::Path;
use std::error::Error;
use std::env;
use std::io;
use std::io::prelude::*;
use std::time::Duration;

extern crate nix;
use nix::unistd::{Gid,Uid,setresgid,setresuid};

extern crate getopts;
use getopts::Options;

struct Configuration {
    port: u16,
    uid: u32,
    gid: u32,
}

fn handle_wrq(_cl: &SocketAddr, _buf: &[u8]) -> Result<(), io::Error> {
    Ok(())
}

fn wait_for_ack(sock: &UdpSocket, expected_block: u16) -> Result<bool, io::Error> {
    let mut buf = [0; 4];
    match sock.recv(&mut buf) {
        Ok(_) => (),
        Err(ref error) if [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut].contains(&error.kind()) => {
            return Ok(false);
        }
        Err(err) => return Err(err),
    };

    let opcode = u16::from_be_bytes([buf[0], buf[1]]);
    let block_nr = u16::from_be_bytes([buf[2], buf[3]]);

    if opcode == 4 && block_nr == expected_block {
        return Ok(true)
    }

    Ok(false)
}

fn send_file(cl: &SocketAddr, filename: &str) -> Result<(), io::Error> {
    let file = File::open(filename);
    let mut file = match file {
        Ok(f) => f,
        Err(ref error) if error.kind() == io::ErrorKind::NotFound => {
            handle_error(cl, 1, "File not found")?;
            return Err(io::Error::new(io::ErrorKind::NotFound, "file not found"));
        },
        Err(_) => {
            handle_error(cl, 2, "Permission denied")?;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"));
        }
    };
    if !file.metadata()?.is_file() {
        handle_error(cl, 1, "File not found")?;
        return Err(io::Error::new(io::ErrorKind::NotFound, "file not found"));
    }

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(cl)?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut block_nr: u16 = 1;

    loop {
        let mut filebuf = [0; 512];
        let len = file.read(&mut filebuf);
        let len = match len {
            Ok(n) => n,
            Err(ref error) if error.kind() == io::ErrorKind::Interrupted => continue, /* retry */
            Err(err) => {
                handle_error(cl, 0, "File reading error")?;
                return Err(err);
            }
        };

        let mut sendbuf = vec![0x00, 0x03];  // opcode
        sendbuf.extend(block_nr.to_be_bytes().iter());
        sendbuf.extend(filebuf[0..len].iter());

        for _ in 1..5 {
            /* try a couple of times to send data, in case of timeouts
               or re-ack of previous data */
            socket.send(&sendbuf)?;
            match wait_for_ack(&socket, block_nr) {
                Ok(true) => break,
                Ok(false) => continue,
                Err(e) => return Err(e),
            };
        }

        if len < 512 {
            /* this was the last block */
            break;
        }

        /* increment with rollover on overflow */
        block_nr = block_nr.wrapping_add(1);
    }
    Ok(())
}

fn file_allowed(filename: &str) -> bool {
    let path = Path::new(".").join(&filename);
    let path = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    let path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };

    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(_) => return false,
    };

    return path.starts_with(cwd);
}

fn handle_rrq(cl: &SocketAddr, buf: &[u8]) -> Result<(), io::Error> {
    let mut iter = buf.iter();

    let dataerr = io::Error::new(io::ErrorKind::InvalidData, "invalid data received");

    let fname_len = iter.position(|&x| x == 0);
    let fname_len = match fname_len {
        Some(len) => len,
        None => return Err(dataerr),
    };
    let fname_begin = 0;
    let fname_end = fname_begin + fname_len;
    let filename = String::from_utf8(buf[fname_begin .. fname_end].to_vec());
    let filename = match filename {
        Ok(fname) => fname,
        Err(_) => return Err(dataerr),
    };

    let mode_len = iter.position(|&x| x == 0);
    let mode_len = match mode_len {
        Some(len) => len,
        None => return Err(dataerr),
    };
    let mode_begin = fname_end + 1;
    let mode_end = mode_begin + mode_len;
    let mode = String::from_utf8(buf[mode_begin .. mode_end].to_vec());
    let mode = match mode {
        Ok(m) => m.to_lowercase(),
        Err(_) => return Err(dataerr),
    };

    match mode.as_ref() {
        "octet" => (),
        _ => handle_error(cl, 0, "Unsupported mode")?,
    }

    match file_allowed(&filename) {
        true => (),
        false => {
            handle_error(cl, 2, "Permission denied")?;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "permission denied"));
        }
    }

    match send_file(&cl, &filename) {
        Ok(_) => println!("Sent {} to {}.", filename, cl),
        Err(_) => println!("Sending {} to {} failed.", filename, cl),
    }
    Ok(())
}

fn handle_error(cl: &SocketAddr, code: u16, msg: &str) -> Result<(), io::Error> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(cl)?;

    let mut buf = vec![0x00, 0x05];  // opcode
    buf.extend(code.to_be_bytes().iter());
    buf.extend(msg.as_bytes());

    socket.send(&buf)?;
    Ok(())
}

fn handle_client(cl: &SocketAddr, buf: &[u8]) -> Result<(), io::Error> {
    let opcode = u16::from_be_bytes([buf[0], buf[1]]);

    match opcode {
        1 /* RRQ */ => handle_rrq(&cl, &buf[2..])?,
        2 /* WRQ */ => handle_wrq(&cl, &buf[2..])?,
        5 /* ERROR */ => println!("Received ERROR from {}", cl),
        _ => handle_error(cl, 4, "Unexpected opcode")?,
    }
    Ok(())
}

fn drop_privs(uid: u32, gid: u32) -> Result<(), Box<Error>> {
    let root_uid = Uid::from_raw(0);
    let root_gid = Gid::from_raw(0);
    let unpriv_uid = Uid::from_raw(uid);
    let unpriv_gid = Gid::from_raw(gid);

    if Gid::current() != root_gid && Gid::effective() != root_gid
        && Uid::current() != root_uid && Uid::effective() != root_uid {
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

fn usage(opts: Options, error: Option<String>) {
    match error {
        None => {},
        Some(err) => println!("{}\n", err),
    }
    println!("{}", opts.usage("RusTFTP"));

}

fn parse_commandline<'a>(args: &'a Vec<String>) -> Result<Configuration, &'a str> {
    let mut conf = Configuration{
        port: 69,
        uid: 65534,
        gid: 65534,
    };
    let mut opts = Options::new();
    opts.optflag("h", "help", "display usage information");
    opts.optopt("p", "port", format!("port to listen on (default: {})", conf.port).as_ref(), "PORT");
    opts.optopt("u", "uid", format!("user id to run as (default: {})", conf.uid).as_ref(), "UID");
    opts.optopt("g", "gid", format!("group id to run as (default: {})", conf.gid).as_ref(), "GID");
    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(err) => {
            usage(opts, Some(err.to_string()));
            return Err("Parsing error");
        }
    };
    if matches.opt_present("h") {
        usage(opts, None);
        return Err("usage");
    }

    conf.port = match matches.opt_get_default::<u16>("p", conf.port) {
        Ok(p) => p,
        Err(err) => {
            usage(opts, Some(err.to_string()));
            return Err("port");
        }
    };
    conf.uid = match matches.opt_get_default::<u32>("u", conf.uid) {
        Ok(u) => u,
        Err(err) => {
            usage(opts, Some(err.to_string()));
            return Err("uid");
        }
    };
    conf.gid = match matches.opt_get_default::<u32>("g", conf.gid) {
        Ok(g) => g,
        Err(err) => {
            usage(opts, Some(err.to_string()));
            return Err("gid");
        }
    };

    return Ok(conf);
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let conf = match parse_commandline(&args) {
        Ok(c) => c,
        Err(_) => return,
    };

    let socket = match UdpSocket::bind(format!("0.0.0.0:{}", conf.port)) {
        Ok(s) => s,
        Err(err) => {
            println!("Binding a socket failed: {}", err);
            return;
        }
    };
    match drop_privs(conf.uid, conf.gid) {
        Ok(_) => (),
        Err(err) => {
            println!("Dropping privileges failed: {}", err);
            return;
        }
    };

    loop {
        let mut buf = [0; 2048];
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(args) => args,
            Err(err) => {
                println!("Receiving data from socket failed: {}", err);
                break;
            }
        };

        match handle_client(&src, &buf[0..n]) {
            /* errors intentionally ignored */
            _ => (),
        }
    }
}
