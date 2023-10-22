## 写在前面

自从买了相机之后，拍的照片比之前多了好多，那几个朋友和我一样都是喜欢摄影且程序员，各自都有自己的网站，心里直痒痒，于是做了一个相册网站，使用React-Album作为前端，后端则是用Go做了个简单的HTTP请求服务器，实现摄影图集陈列。

后来觉得这个网站核心在于静态文件下载，而每一张图动辄几十M，遂决定直接做一个静态服务器做这个功能。

后来用Rust写了一个zero-copying的静态服务器，用sendfile+mmap，但，因为文件IO还是blocking的，所以开辟线程池，学Go的runtime实现，测了一下性能貌似还行，瓶颈也在于mmap使用的page cache释放不及时问题，于是想到了io_uring技术，这玩意解决了文件IO的blocking，而且还支持splice操作（存疑，还没有真的开始码），所以决定研究一下。

## 理论

### 架构



### 部分原理

### 小结

## 使用

## 参考

[io_uring_enter(2) — Linux manual page](https://man7.org/linux/man-pages/man2/io_uring_enter.2.html)

[Efficient IO with io_uring](https://kernel.dk/io_uring.pdf)

[io_uring.c](https://github.com/torvalds/linux/blob/4f82870119a46b0d04d91ef4697ac4977a255a9d/io_uring/io_uring.c#L3601)

[io_uring 的接口与实现](https://www.skyzh.dev/blog/2021-06-14-deep-dive-io-uring/)

[图解原理｜Linux I/O 神器之 io_uring](https://cloud.tencent.com/developer/article/2187655)


