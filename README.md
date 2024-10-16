## 写在前面

自从买了相机之后，拍的照片比之前多了好多，那几个朋友和我一样都是喜欢摄影且程序员，各自都有自己的网站，心里直痒痒，于是做了一个相册网站，使用React-Album作为前端，后端则是用Go做了个简单的HTTP请求服务器，实现摄影图集陈列。

后来觉得这个网站核心在于静态文件下载，而每一张图动辄几十M，遂决定直接做一个静态服务器做这个功能。

后来用Rust写了一个zero-copying的静态服务器，用sendfile+mmap，但，因为文件IO还是blocking的，所以开辟线程池，学Go的runtime实现，测了一下性能貌似还行，瓶颈也在于mmap使用的page cache释放不及时问题，于是想到了io_uring技术，这玩意解决了文件IO的blocking，而且还支持splice操作（存疑，还没有真的开始码），所以决定研究一下。

## 理论

### 架构

首先，io_uring由两个环形数组+两个控制结构体组成。分别是提交数组(Submission)和完成数组(Completion)，两个控制结构体分别控制两个数组的读写指针索引，地址等。

为什么用环形数组，因为节省内存，方便共享，类似的技术在epoll也存在；两个控制结构体存在内核空间，并且通过mmap映射到用户空间，这样避免了拷贝，也实现了更新可见，当然这会引来并发访问问题。

这里需要注意的是，SQ/CQ Ring分别保存两个Ring的头尾指针，在SQ中，App更新尾指针来生产，Kernel更新头指针来消费。CQ相反，向/从 SQ/CQ 添加/移除 SQ项/CQ项 是通过先放置再更新指针来实现的，不过这里会使用内存屏障技术去保证更新可见。

![](./1.svg)

上述图是一个综述，包含了四种模式下的整体逻辑，我们会拆开一个一个讲。



一个简单的流程：

- 创建SQE：包含操作码指明要做什么操作，比如读，写文件，Socket连接等；还包含涉及到的文件FD等，缓冲区这些。
- 提交SQE：把SQE添加到SQ中，并且更新尾指针。
- 调用enter()进入Kernel侧，尝试从SQ消费一个SQE，更新头指针，然后根据具体的任务类型提交到线程池(io-wq)/轮询等去做实际的执行。
- 挂起，等待中断到达。
- 构造CQE，提交到CQ，更新CQ尾指针，判断是否数量满足预期。
- 返回。
- App侧消费CQE，更新头指针。

这里注意到，任务可能是由io-wq线程池去完成的，这是一个内核创建的轻量级线程池，用来处理任务，类似我们创建线程池处理文件阻塞调用，不过它做的不只是这些。

### 部分原理

在正式开始之前，先拆开讲讲各个部分的原理。

上述已经展开了涉及到的几个主要结构，CQ-Ring，SQ-Ring，CQ，SQ，CQE，SQE。

CQE最简单，先说它：

它的组成简单如下：

``` c
struct cqe {
  __u64 user_data;
  __s32 res;
  __u32 flags;
};
```

res代表操作结果，而user_data则是用来溯源对应的SQE的关键字段。它是对应的SQE中的user_data的拷贝。比如可以在SQE设置一个指针，然后从CQE的user_data读取指针，读到指针指向的buffer，然后操作的结果被预先读入了buffer，之后就可以得到操作结果了。

接下来是SQE：

``` c
struct sqe {
  __u8 opcode;
  __u9 flags;
  __u16 io_priority;
  __s32 fd;
  __u64 offset;
  __u64 addr;
  __u32 len;
  union {
    __kernel_rwf_t rw_flags;
    __u32 fsync_flags;
    __u16 poll_events;
    __u32 sync_rang_flags;
    __u32 msg_flags;
  };
  __u64 user_data;
  union {
    __u16 buf_index;
    __u64 __padding2[3];
  };
}
```

opcode很好理解，指出了此次任务的类型，比如读写文件，还是某些系统调用，还是网络相关的操作。

后面的padding实现了64位对齐，或者保留用于后续添加字段。

其他部分看字段名基本可以理解，比如offset指出此次操作针对addr上数据的偏移，以及需要len字节的数据。



当使用io_uring相关接口时，需要自己处理很多东西，比如setup()调用，返回一个fd代表背后的实例，但是也会返回一堆偏移量，指出SQ-Ring和CQ-Ring针对实例结构体的偏移，因为我们需要更新这两个Ring结构体的指针，所以需要自己计算去构造，同时也需要自己去做mmap，得到访问权限。

目前Linux针对io_uring提供的接口就两个，一个构造，一个万能接口：io_uring_setup()和io_uring_enter()。

所以我们会详细说说enter()调用，因为提交任务，推动任务执行，获取完成项，轮询操作等都是它完成的。

在详细开始之前，需要说一下io_uring在构造时可以选定不同的模式：

- 默认：App侧主动提交SQE，陷入内核，消费SQE，推动任务执行，比如提交到io-wq，挂起，等待中断，构造CQE，提交CQE，等待数量达到，返回，消费CQE。
- IO_POLL：App侧主动提交SQE，陷入内核，消费SQE，提交任务，轮询任务执行状态直到完成状态出现，构造CQE，提交CQE，等到数量达到，返回，消费CQE。
- SQ_POLL：App侧主动提交SQE，唤醒内核POLL线程(如果需要)，内核消费SQE，推动任务执行，提交给io-wq，挂起App侧；等待中断，中断到达，构造CQE，提交CQE，数量达到唤醒App侧，App侧全程不需要陷入内核(如果不需要唤醒)。
- IO_POLL + SQ_POLL：App侧主动提交SQE，唤醒内核POLL线程(如果需要)，内核消费SQE，提交任务，App侧轮询任务执行状态直到完成状态出现，构造CQE，提交CQE，数量达到，返回。

这里的IO_POLL和SQ_POLL很有误导性，它们是完全不同的两个方面，一个管控任务执行方式，一个管控SQ消费方式。

来看一个执行图：

![](./2.svg)

在这里可以看到不同参数对于执行流的影响。在使用enter()调用时，如果需要等待指定数量的事件完成，则会触发阻塞，这里的阻塞可能是轮询产生的忙等待，也可能是等待中断唤醒的挂起。

如果设置了SQ_POLL，则SQE推动，提交给驱动去处理，或者加入内核的任务队列，则是由Kernel侧的线程去完成，否则则是App侧调用enter()的线程自己去完成。

而如果设置了IO_POLL，则需要App侧在任务推动之后(无论是谁推动的)，主动去poll驱动的ready状态；否则挂起，此时任务由io-wq执行，并在中断到达时提交到CQ，并且在数量满足时唤醒调用enter()的线程。

这里的io-wq并不是中断配置下的默认选择，相反可能直接立即执行，比如文件已经存在page cache中，此时直接在当前线程处理即可；这个细节比较复杂，涉及到具体的逻辑，后面会展开细说。

顺便一提，io-wq的线程数量是 CPU数量*4和SQ Entries之间的最小值。如果是在执行文件IO，底层文件系统或者内核支持异步读写，那么则设置kiocb(内核IO控制块，所有的IO都要经由它给内核记录跟踪)的完成回调(比如把此次操作结果放入完成队列)，然后使用异步读写方法(目前是file_opeartions里的read_iter/write_iter)提交给文件系统。如果不支持，则阻塞在wq线程不断循环读写直到满足要求，等同于平时的文件读写操作。

另外，在构造时，默认CQ的大小是SQ的二倍，因为有时App侧拉取不及时会导致CQE堆积，所以App侧需要留意这件事。因为一般来说，App侧把SQE提交到队列就算完事了，之后就可以复用SQE，而Kernel或者IO_POLL执行SQE是需要时间的，所以可能导致App侧提交了两圈的SQE但是CQE未来得及收割。

### 高级特性

io_uring经历了好几轮的迭代，现在能找到的资料大多比较零散，最近有搜罗了一堆资料，决定讲讲高级特性部分。

首先就是io-wq的设计，它并不是简单的线程池。

io-wq内部有两个池，分别执行两种IO任务。根据任务是否可以在有限时间内完成，可以把任务分为：

- 有限时间任务(bounded work)，一般指的是文件操作，块设备读取等。
- 无限时间任务(unbounded work)，一般指网络IO，字符输入设备读取等，因为你不知道什么时候远端数据才会到达，但是文件IO取决于磁盘，这基本有一个最大范围。

对于有限时间的任务，io-wq的线程池大小默认是min(sq队列大小，4倍可用CPU)。对于无限时间任务，默认池大小是RLIMIT_NPROC。这两个池的大小可以设置。

io-wq对于有限时间任务的处理，和普通处理文件一样，开辟线程阻塞读取，完成之后加入CQ。

io-wq对于无限时间任务的处理，比较“智能”，它会先判断是否支持non-blocking，如果不支持直接加入线程池；如果支持，尝试请求一下。如果未就绪，加入内部的类似epoll的机制，然后用一个线程去监听。这个特性需要5.7之后的内核支持，即IOURING_FEAT_FAST_POLL。

然后就是non-blocking判断。

一个任务提交到io_uring，会先尝试走non-blocking，如果得不到结果再入池，但是你可以指明这个任务不要进行non-blocking尝试，直接入池，比如文件操作。但可能会很低效，如果你没有把握就不要开启这个标识。

其实这里有一个模糊点，Linux的文件IO天然不支持non-blocking，所以对文件操作进行就绪判断是无意义的，但是对于网络IO直接入池又是低效的，因为网络IO支持non-blocking。我猜测这是早期的设计问题。因为早期并没有内部poll机制去处理non-blocking的网络IO，而是都扔到无限时间池处理的，所以这个取消尝试的标识可能是用于这里的。因为早期引入有限时间和无限时间的队列就是因为那时网络IO是直接提交的，和文件IO一样，但是避免阻塞到文件IO(网络不知何时有数据或者可写)操作，所以开辟了默认池更大的线程池给网络IO用。

之后是IOPOLL的设置，首先SQE加入到SQ环不会提交给内核，因为内核感知不到来新单了，需要调用`io_uring_enter()`去告诉内核来订单了；如果设置了IOPOLL，会在调用`enter`提交SQE时loop住，即忙轮询，轮询IO就绪，有些外设或者技术处理IO很快，比提交中断唤醒这一套还快，所以此时忙轮询是有意义的。

之后等待收割。

SQPOLL只是替你把SQE转给内核处理，省去了你更新SQ环之后再调用`enter`的麻烦。

对于完成队列，除了loop住等待CQE，还可以做到类似epoll的时间通知机制，在有新的CQE时通知你，这是eventfd技术，需要使用`io_uring_register`，选择IOURING_REGISTER_EVENTFD标识进行注册得到一个eventfd，加入你的epoll即可。

接下来是io-wq线程池数量的小细节

io-wq不是和io_uring实例绑定的，是和创建它的线程绑定的，举个例子：设定无限时间任务线程池数量为8。

通过一个线程创建两个io_uring实例，无论怎么测最后所有的实例的无限时间任务线程池数量都是8

通过四个线程各创建两个io_uring实例，线程池创建的线程总数是32。

此外如果通过限制RLIMIT_NPROC，则会影响同一UID下的所有数量，即只要是同一UID发起的io_uring，它们的线程池线程数总数不会超过RLIMIT_NPROC。

### 小结

io_uring的调用虽然只有两个，但是隐藏了复杂的分支流程，作为用户只要简单的使用即可，不过最好还是使用封装好的库，比如liburing，替我们做了很多不必要的封装。

#### 2024-04-25追加

最近重新思考了io_uring的实现，或者说使用。

首先是默认情况下的一个流程：用户程序申请一个sq_ring的空间，其实就是数组中的一项，然后构造请求；之后把这个任务放到请求来的地址上，此时请求已经入队，但是呢，内核是不知道的，或者说io_uring是不知道的，你只是更新了sq_ring罢了。所以你还需要调用`io_uring_enter`函数，陷入内核态，然后遍历sq_ring，得到提交的新任务，然后处理。

你可以选择在`enter`函数里指出需要的完成数，或者不指明，不指明的话就在把任务提交给后台线程池或者什么的之后就返回了。然后用户程序调用带有完成数的`io_uring_enter`去block住，直到有任务OK了；之后遍历cq_ring，获取完成的任务。

如果是IO_POLL模式的话，上述都是一样的，只有`io_uring_enter`此时block住不再是中断唤醒，而是改成了内部不断loop轮训io_uring直到有任务完成才会返回。

如果是SQ_POLL模式的话，陷入内核的过程被取消了(如果内核线程没休眠的话)，用户程序只管更新sq_ring然后轮训cq_ring就好，剩下的工作由内核线程完成。

二者结合的话，使用起来和只用SQ_POLL一样，提交完毕后等待完成事件的内核线程也不再被中断唤醒，而是轮训直到完成事件发生。

注意，上述的轮训和阻塞一样，都是执行在用户线程上的，io_uring本身不会替你做这些事，用户程序在轮训时就像里面跑了一个loop似的。

如果内核支持，还会开启FAST_POLL特性，让那些支持non-blocking的socket挂在类似epoll的机制上，所有提交给此io_uring的non-blocking都由这个类epoll完成；而对于文件操作，则是放到线程池执行。

当你提交任务时，io_uring会聪明的try一次，然后在失败后判断是否non-blocking，如果你不想丢到类epoll去执行，你可以在创建SQE的时候，设置flag为IOSQE_ASYNC，强迫这个任务放入线程池或者开辟新的线程来跑，前者适用有限时间操作，后者是类似socket这种操作。为什么io_uring会try一次呢？因为可能要读的文件刚好在page cache，或者socket的buffer刚好空了，可写状态。

## 使用

在正式开始讨论用法之前，你必须保证有一个Linux环境，且内核版本(建议5.13以上)符合要求。如果你是Windows，考虑WSL2，如果你是Linux原生勇者，那可以直接进行下一步。

如果你是macOS用户，也不是很难办，要么使用Docker或者OrbStack(推荐)搭建一个虚拟主机；要么使用Multipass搭建一个云服务器，我选择了后者，一方面因为性能更好，另一方面嘛，简单。

Multipass是Ubuntu官方提供的云服务器环境搭建工具，你可以用它在macOS上快速搭建类似阿里云或者腾讯云这种的云服务器。如果你是Arm架构的macOS，搭建出来的也是Arm的Linux，我用起来是没什么问题，Linux本身对于Arm支持比WindowsOnArm强太多了；如果你是老的Intel，那自然是X64架构的。

环境搭建完成，开始准备。

如果你愿意大费周章的去自己做mmap，计算偏移量，设置构造参数，那可以直接使用io_uring_setup()和io_uring_enter()这两个系统调用。不过呢，对于C来说，有liburing可以直接使用，它封装了很多的细节。

如果是Go语言，目前也有一些第三方的包可以调用，对于Java语言直接考虑使用Netty等网络库。

最后我们来一些Rust中的使用，毕竟了解这个库一开始就是为了在Rust中使用。

``` rust
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

```

用到的依赖：

``` toml
[dependencies]
io-uring = "0.6.2"
slab = "0.4.9"
libc = "0.2.149"
```

这里给出了一个“简单的”Tcp Echo服务器。如你所见，写了很多，而仅仅做到了Echo功能。

这里我们选用Tokio封装的io-uring，查看源码就知道Tokio做的封装很纯(简)粹(陋)，Rust中还有很多针对io-uring的封装，可以根据自己的爱好选用。

另外一提，Tokio有一个异步化的io-uring库，如果你喜欢async可以考虑，或者自己封装裸操作，比如定义几个结构体实现Future之类的。

上述代码有一个有趣的backlog，它用于处理环满的时候，把额外的SQE保存下来，后面循环时再提交。之后就是简单易懂的循环，从Accept->新连接到达->PollAdd_Read->可读->Recv->PollAdd_Write->可写->Send->循环往复。

Tokio的思路是使用一个枚举来保存user_data字段，里面包含此次操作完成之后需要进行的处理，这是一种状态机思想。

## 参考

[io_uring_enter(2) — Linux manual page](https://man7.org/linux/man-pages/man2/io_uring_enter.2.html)

[Efficient IO with io_uring](https://kernel.dk/io_uring.pdf)

[io_uring.c](https://github.com/torvalds/linux/blob/4f82870119a46b0d04d91ef4697ac4977a255a9d/io_uring/io_uring.c#L3601)

[io_uring 的接口与实现](https://www.skyzh.dev/blog/2021-06-14-deep-dive-io-uring/)

[图解原理｜Linux I/O 神器之 io_uring](https://cloud.tencent.com/developer/article/2187655)

[io_uring 使用教程| io_uring 完全指南 | io_uring 实践指导 | io_uring 资料参考](https://blog.csdn.net/u010180372/article/details/123931574)

[Missing Manuals - io_uring worker pool](https://blog.cloudflare.com/missing-manuals-io_uring-worker-pool/)
