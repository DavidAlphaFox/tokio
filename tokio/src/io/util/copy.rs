use crate::io::{AsyncRead, AsyncWrite, ReadBuf};

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug)]
pub(super) struct CopyBuffer {
    read_done: bool,
    need_flush: bool,
    pos: usize,
    cap: usize,
    amt: u64,
    buf: Box<[u8]>,
}

impl CopyBuffer {
    pub(super) fn new() -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            amt: 0,
            buf: vec![0; super::DEFAULT_BUF_SIZE].into_boxed_slice(), //默认构建一个8k的buffer，默认放在堆上
        }
    }

    fn poll_fill_buf<R>(
        &mut self,
        cx: &mut Context<'_>,
        reader: Pin<&mut R>,
    ) -> Poll<io::Result<()>>
    where
        R: AsyncRead + ?Sized,
    {
        let me = &mut *self;
        let mut buf = ReadBuf::new(&mut me.buf); //构建reader buffer
        buf.set_filled(me.cap); // 双位置指针的buffer，pos记录数据在开始位置，cap记录数据结束位置

        let res = reader.poll_read(cx, &mut buf); //让reader填充buffer
        if let Poll::Ready(Ok(())) = res { //填充成功
            let filled_len = buf.filled().len(); //得到buffer已经填充了多少
            me.read_done = me.cap == filled_len; //如果填充的数量和自身的容量大小相同，说明reader已经完成了工作，因为本次接收的数据为0
            me.cap = filled_len; //我们当前填充了多少
        }
        res
    }

    fn poll_write_buf<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<usize>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        let me = &mut *self;
        match writer.as_mut().poll_write(cx, &me.buf[me.pos..me.cap]) {
            Poll::Pending => {
                // Top up the buffer towards full if we can read a bit more
                // data - this should improve the chances of a large write
                if !me.read_done && me.cap < me.buf.len() {
                    ready!(me.poll_fill_buf(cx, reader.as_mut()))?;
                }
                Poll::Pending
            }
            res => res,
        }
    }

    pub(super) fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        ready!(crate::trace::trace_leaf(cx));
        #[cfg(any(
            feature = "fs",
            feature = "io-std",
            feature = "net",
            feature = "process",
            feature = "rt",
            feature = "signal",
            feature = "sync",
            feature = "time",
        ))]
        // Keep track of task budget
        let coop = ready!(crate::runtime::coop::poll_proceed(cx));
        loop {
            // If our buffer is empty, then we need to read some data to
            // continue.
            if self.pos == self.cap && !self.read_done { //读取没有关闭，并且我们的数据已经发送完成了
                self.pos = 0;
                self.cap = 0;

                match self.poll_fill_buf(cx, reader.as_mut()) {
                    Poll::Ready(Ok(())) => {
                        #[cfg(any(
                            feature = "fs",
                            feature = "io-std",
                            feature = "net",
                            feature = "process",
                            feature = "rt",
                            feature = "signal",
                            feature = "sync",
                            feature = "time",
                        ))]
                        coop.made_progress();
                    } //填充buffer，推进进度
                    Poll::Ready(Err(err)) => {
                        #[cfg(any(
                            feature = "fs",
                            feature = "io-std",
                            feature = "net",
                            feature = "process",
                            feature = "rt",
                            feature = "signal",
                            feature = "sync",
                            feature = "time",
                        ))]
                        coop.made_progress();
                        return Poll::Ready(Err(err));
                    }
                    Poll::Pending => {
                        // Try flushing when the reader has no progress to avoid deadlock
                        // when the reader depends on buffered writer.
                        if self.need_flush {
                            ready!(writer.as_mut().poll_flush(cx))?;
                            #[cfg(any(
                                feature = "fs",
                                feature = "io-std",
                                feature = "net",
                                feature = "process",
                                feature = "rt",
                                feature = "signal",
                                feature = "sync",
                                feature = "time",
                            ))]
                            coop.made_progress();
                            self.need_flush = false;
                        } //强制的flush writer

                        return Poll::Pending;
                    }
                }
            }

            // If our buffer has some data, let's write it out!
            while self.pos < self.cap {
                let i = ready!(self.poll_write_buf(cx, reader.as_mut(), writer.as_mut()))?; //进行writer的输出
                #[cfg(any(
                    feature = "fs",
                    feature = "io-std",
                    feature = "net",
                    feature = "process",
                    feature = "rt",
                    feature = "signal",
                    feature = "sync",
                    feature = "time",
                ))]
                coop.made_progress();
                if i == 0 { //写出错了，对面有可能关闭的pipe
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero byte into writer",
                    )));
                } else {
                    self.pos += i; //调整指针位置
                    self.amt += i as u64; //增加吞吐量
                    self.need_flush = true; //标记需要flush
                }
            }

            // If pos larger than cap, this loop will never stop.
            // In particular, user's wrong poll_write implementation returning
            // incorrect written length may lead to thread blocking.
            debug_assert!(
                self.pos <= self.cap,
                "writer returned length larger than input slice"
            );

            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.pos == self.cap && self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?; //刷写出端
                #[cfg(any(
                    feature = "fs",
                    feature = "io-std",
                    feature = "net",
                    feature = "process",
                    feature = "rt",
                    feature = "signal",
                    feature = "sync",
                    feature = "time",
                ))]
                coop.made_progress();
                return Poll::Ready(Ok(self.amt)); //返回总共传递了多少数据
            }
        }
    }
}

/// A future that asynchronously copies the entire contents of a reader into a
/// writer.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
struct Copy<'a, R: ?Sized, W: ?Sized> {
    reader: &'a mut R,
    writer: &'a mut W,
    buf: CopyBuffer,
}

cfg_io_util! {
    /// Asynchronously copies the entire contents of a reader into a writer.
    ///
    /// This function returns a future that will continuously read data from
    /// `reader` and then write it into `writer` in a streaming fashion until
    /// `reader` returns EOF or fails.
    ///
    /// On success, the total number of bytes that were copied from `reader` to
    /// `writer` is returned.
    ///
    /// This is an asynchronous version of [`std::io::copy`][std].
    ///
    /// A heap-allocated copy buffer with 8 KB is created to take data from the
    /// reader to the writer, check [`copy_buf`] if you want an alternative for
    /// [`AsyncBufRead`]. You can use `copy_buf` with [`BufReader`] to change the
    /// buffer capacity.
    ///
    /// [std]: std::io::copy
    /// [`copy_buf`]: crate::io::copy_buf
    /// [`AsyncBufRead`]: crate::io::AsyncBufRead
    /// [`BufReader`]: crate::io::BufReader
    ///
    /// # Errors
    ///
    /// The returned future will return an error immediately if any call to
    /// `poll_read` or `poll_write` returns an error.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::io;
    ///
    /// # async fn dox() -> std::io::Result<()> {
    /// let mut reader: &[u8] = b"hello";
    /// let mut writer: Vec<u8> = vec![];
    ///
    /// io::copy(&mut reader, &mut writer).await?;
    ///
    /// assert_eq!(&b"hello"[..], &writer[..]);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn copy<'a, R, W>(reader: &'a mut R, writer: &'a mut W) -> io::Result<u64>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        Copy {
            reader,
            writer,
            buf: CopyBuffer::new()
        }.await
    }
}

impl<R, W> Future for Copy<'_, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<u64>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let me = &mut *self;

        me.buf
            .poll_copy(cx, Pin::new(&mut *me.reader), Pin::new(&mut *me.writer))
    }
}
