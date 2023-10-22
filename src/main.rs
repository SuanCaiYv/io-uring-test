use std::{fs::OpenOptions, io, os::{fd::AsRawFd, unix::prelude::OpenOptionsExt}};

use io_uring::{cqueue, opcode, squeue, types, IoUring};

fn main() -> io::Result<()> {
    let mut ring: IoUring<squeue::Entry, cqueue::Entry> = IoUring::builder()
        .setup_iopoll()
        .setup_sqpoll(500)
        .build(1024)?;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(040000)
        .open("/home/ubuntu/index.html")?;
    let mut buf = vec![0; 1024];

    let read_e = opcode::Read::new(types::Fd(file.as_raw_fd()), buf.as_mut_ptr(), buf.len() as _)
        .build()
        .user_data(1234);

    // Note that the developer needs to ensure
    // that the entry pushed into submission queue is valid (e.g. fd, buffer).
    unsafe {
        ring.submission()
            .push(&read_e)
            .expect("submission queue is full");
    }

    ring.submit_and_wait(1)?;

    let cqe = ring.completion().next().expect("completion queue is empty");

    println!("{}", cqe.result());

    Ok(())
}
