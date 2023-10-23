use std::{io, net::TcpListener, os::fd::{AsRawFd, RawFd}, ptr, collections::VecDeque};

use io_uring::{cqueue, opcode, squeue, types, IoUring};
use slab::Slab;

#[derive(Debug, Clone)]
enum Token {
    Accept,
    Poll {
        fd: RawFd,
    },
    Read {
        fd: RawFd,
        buf_idx: usize,
    },
    Write {
        fd: RawFd,
        buf_idx: usize,
        offset: usize,
        len: usize,
    }
}

fn main() -> io::Result<()> {
    let mut backlog = VecDeque::new();
    let mut token_vec = Slab::with_capacity(1024);
    let mut ring: IoUring<squeue::Entry, cqueue::Entry> = IoUring::builder()
        // .setup_iopoll()
        .setup_sqpoll(500)
        .build(1024)?;
    let listener = TcpListener::bind(("0.0.0.0", 8190))?;
    let (submitter, mut sq, mut cq) = ring.split();
    let accept_idx = token_vec.insert(Token::Accept);
    let accept = opcode::Accept::new(
        types::Fd(listener.as_raw_fd()),
        ptr::null_mut(),
        ptr::null_mut(),
    ).build().user_data(accept_idx as _);
    unsafe { sq.push(&accept) };
    sq.sync();
    loop {
        match submitter.submit_and_wait(1) {
            Ok(_) => {},
            Err(e) => {
                println!("submit_and_wait error: {:?}", e);
                break;
            }
        }
        cq.sync();
        loop {
            if sq.is_full() {
                _ = submitter.submit();
            }
            sq.sync();
            match backlog.pop_front() {
                Some(sqe) => unsafe {
                    _ = sq.push(&sqe);
                },
                None => break,
            }
        }
        unsafe { sq.push(&accept) };
        for cqe in &mut cq {
            let res = cqe.result();
            let token_idx = cqe.user_data() as usize;
            if res < 0 {
                eprintln!("cqe error: {:?}", io::Error::from_raw_os_error(-res));
                continue;
            }

            let token = &mut token_vec[token_idx];
        }
    }

    Ok(())
}
