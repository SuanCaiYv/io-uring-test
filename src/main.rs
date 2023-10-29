use std::{
    collections::VecDeque,
    io,
    net::TcpListener,
    os::fd::{AsRawFd, RawFd},
    ptr,
};

use io_uring::{cqueue, opcode, squeue, types, IoUring};
use slab::Slab;

#[derive(Debug, Clone)]
enum Token {
    Accept,
    Poll {
        fd: RawFd,
        read: bool,
        buf_idx: usize,
        offset: usize,
        // size of bytes need to be sent
        // or the size of the buffer can be filled.
        len: usize,
    },
    Read {
        fd: RawFd,
        buf_idx: usize,
    },
    Write {
        fd: RawFd,
        buf_idx: usize,
        // offset + len should equal to the length of the buffer
        offset: usize,
        // size of bytes need to be sent
        len: usize,
    },
}

fn main() -> io::Result<()> {
    let mut backlog = VecDeque::new();
    let mut token_vec = Slab::with_capacity(1024);
    let mut buffer_pool = Vec::with_capacity(1024);
    let mut buffer_alloc = Slab::with_capacity(1024);
    let listener = TcpListener::bind(("0.0.0.0", 8190))?;

    let mut ring: IoUring<squeue::Entry, cqueue::Entry> = IoUring::builder()
        // .setup_iopoll()
        .setup_sqpoll(500)
        .build(1024)?;
    let (submitter, mut sq, mut cq) = ring.split();

    let accept_idx = token_vec.insert(Token::Accept);
    let accept = opcode::Accept::new(
        types::Fd(listener.as_raw_fd()),
        ptr::null_mut(),
        ptr::null_mut(),
    )
    .build()
    .user_data(accept_idx as _);
    unsafe {
        _ = sq.push(&accept);
    }
    sq.sync();

    loop {
        match submitter.submit_and_wait(1) {
            Ok(_) => {}
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
        unsafe {
            _ = sq.push(&accept);
        }
        for cqe in &mut cq {
            let res = cqe.result();
            let token_idx = cqe.user_data() as usize;
            if res < 0 {
                eprintln!("cqe error: {:?}", io::Error::from_raw_os_error(-res));
                continue;
            }

            let token = &mut token_vec[token_idx];
            match *token {
                Token::Accept => {
                    println!("new connection");
                    let (buf_idx, _buf) = match buffer_pool.pop() {
                        Some(buf_index) => (buf_index, &mut buffer_alloc[buf_index]),
                        None => {
                            let buf = vec![0u8; 2048].into_boxed_slice();
                            let buf_entry = buffer_alloc.vacant_entry();
                            let buf_index = buf_entry.key();
                            (buf_index, buf_entry.insert(buf))
                        }
                    };
                    let token = token_vec.insert(Token::Poll {
                        fd: res as _,
                        read: true,
                        buf_idx,
                        offset: 0,
                        len: 2048,
                    });
                    let poll = opcode::PollAdd::new(types::Fd(res as _), libc::POLLIN as _)
                        .build()
                        .user_data(token as _);
                    unsafe {
                        if sq.push(&poll).is_err() {
                            backlog.push_back(poll);
                        }
                    }
                }
                Token::Poll {
                    fd,
                    read,
                    buf_idx,
                    offset,
                    len,
                } => {
                    if read {
                        *token = Token::Read { fd, buf_idx };
                        let buf = &mut buffer_alloc[buf_idx][offset..];
                        let read =
                            opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), len as _)
                                .build()
                                .user_data(token_idx as _);
                        unsafe {
                            if sq.push(&read).is_err() {
                                backlog.push_back(read);
                            }
                        }
                    } else {
                        *token = Token::Write {
                            fd,
                            buf_idx,
                            offset,
                            len,
                        };
                        let buf = &buffer_alloc[buf_idx][offset..];
                        let write = opcode::Send::new(types::Fd(fd), buf.as_ptr(), len as _)
                            .build()
                            .user_data(token_idx as _);
                        unsafe {
                            if sq.push(&write).is_err() {
                                backlog.push_back(write);
                            }
                        }
                    }
                }
                Token::Read { fd, buf_idx } => {
                    if res == 0 {
                        println!("connection closed");
                        buffer_pool.push(buf_idx);
                        token_vec.remove(token_idx);
                        unsafe { libc::close(fd) };
                        continue;
                    }
                    let len = res as usize;
                    let buf = &buffer_alloc[buf_idx][..len];
                    println!("server read: {}", String::from_utf8_lossy(buf).to_string());
                    *token = Token::Poll {
                        fd,
                        read: false,
                        buf_idx,
                        offset: 0,
                        len,
                    };
                    let poll = opcode::PollAdd::new(types::Fd(fd), libc::POLLOUT as _)
                        .build()
                        .user_data(token_idx as _);
                    unsafe {
                        if sq.push(&poll).is_err() {
                            backlog.push_back(poll);
                        }
                    }
                }
                Token::Write {
                    fd,
                    buf_idx,
                    offset,
                    len,
                } => {
                    if res == 0 {
                        println!("connection closed");
                        buffer_pool.push(buf_idx);
                        token_vec.remove(token_idx);
                        unsafe { libc::close(fd) };
                        continue;
                    }
                    let sent = res as usize;
                    if sent < len {
                        *token = Token::Poll {
                            fd,
                            read: false,
                            buf_idx,
                            offset: offset + sent,
                            len: len - sent,
                        };
                        let poll = opcode::PollAdd::new(types::Fd(fd), libc::POLLOUT as _)
                            .build()
                            .user_data(token_idx as _);
                        unsafe {
                            if sq.push(&poll).is_err() {
                                backlog.push_back(poll);
                            }
                        }
                    } else {
                        *token = Token::Poll {
                            fd,
                            read: true,
                            buf_idx,
                            offset: 0,
                            len: 2048,
                        };
                        let poll = opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as _)
                            .build()
                            .user_data(token_idx as _);
                        unsafe {
                            if sq.push(&poll).is_err() {
                                backlog.push_back(poll);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
