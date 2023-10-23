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

这里注意到，任务可能是由io-wq线程池去完成的，这是一个内核创建的轻量级线程池，用来处理任务，类似我们创建线程池处理文件阻塞调用，不过他做的不只是这些。

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

### 小结

io_uring的调用虽然只有两个，但是隐藏了复杂的分支流程，作为用户只要简单的使用即可，不过最好还是使用封装好的库，比如liburing，替我们做了很多不必要的封装。

## 使用

最后我们来一些Rust中的使用，毕竟了解这个库一开始就是为了在Rust中使用。

## 参考

[io_uring_enter(2) — Linux manual page](https://man7.org/linux/man-pages/man2/io_uring_enter.2.html)

[Efficient IO with io_uring](https://kernel.dk/io_uring.pdf)

[io_uring.c](https://github.com/torvalds/linux/blob/4f82870119a46b0d04d91ef4697ac4977a255a9d/io_uring/io_uring.c#L3601)

[io_uring 的接口与实现](https://www.skyzh.dev/blog/2021-06-14-deep-dive-io-uring/)

[图解原理｜Linux I/O 神器之 io_uring](https://cloud.tencent.com/developer/article/2187655)