//! A socket permit belongs to its IO, including keep-alive idle time and slow
//! response writes. HTTP middleware cannot enforce a connection lifetime cap.

use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::admission::Metrics;

pub(super) struct LimitedListener {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}

impl LimitedListener {
    pub fn new(listener: TcpListener, maximum: usize, metrics: Arc<Metrics>) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(maximum)),
            metrics,
        }
    }
}

impl Listener for LimitedListener {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                        self.metrics
                            .connection_rejections
                            .fetch_add(1, Ordering::Relaxed);
                        drop(stream);
                        continue;
                    };
                    self.metrics
                        .active_connections
                        .fetch_add(1, Ordering::Relaxed);
                    return (
                        Connection {
                            stream,
                            _permit: permit,
                            metrics: self.metrics.clone(),
                        },
                        address,
                    );
                }
                Err(error) => {
                    eprintln!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

pub(super) struct Connection {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
    metrics: Arc<Metrics>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.metrics
            .active_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::{oneshot, Notify};

    async fn small_response(stream: &mut BufReader<TcpStream>) {
        stream
            .get_mut()
            .write_all(b"GET /small HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut status = String::new();
        stream.read_line(&mut status).await.unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let mut body = [0; 2];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"ok");
    }

    async fn count(metrics: &Metrics, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while metrics.active_connections.load(Ordering::Relaxed) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn slow_response_and_keepalive_hold_socket_permits_until_io_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let metrics = Arc::new(Metrics::default());
        let limited = LimitedListener::new(listener, 2, metrics.clone());
        let started = Arc::new(Notify::new());
        let router = Router::new().route("/small", get(|| async { "ok" })).route(
            "/large",
            get({
                let started = started.clone();
                move || {
                    let started = started.clone();
                    async move {
                        // Exceeds the loopback send buffer: the handler completes,
                        // while its non-reading client's response remains in flight.
                        let body = vec![b'x'; 32 * 1024 * 1024];
                        started.notify_one();
                        body
                    }
                }
            }),
        );
        let (stop, shutdown) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(limited, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
                .unwrap();
        });
        let mut slow = TcpStream::connect(address).await.unwrap();
        slow.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        started.notified().await;
        let mut keepalive = BufReader::new(TcpStream::connect(address).await.unwrap());
        small_response(&mut keepalive).await;
        count(&metrics, 2).await;
        let mut overflow = TcpStream::connect(address).await.unwrap();
        let _ = overflow
            .write_all(b"GET /small HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await;
        let mut byte = [0];
        match tokio::time::timeout(Duration::from_secs(5), overflow.read(&mut byte))
            .await
            .unwrap()
        {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) => {}
            result => panic!("overflow connection received a response: {result:?}"),
        }
        assert_eq!(metrics.connection_rejections.load(Ordering::Relaxed), 1);
        // The same socket can issue another request without a second permit.
        small_response(&mut keepalive).await;
        count(&metrics, 2).await;
        drop(slow);
        drop(keepalive);
        count(&metrics, 0).await;
        let mut replacement = BufReader::new(TcpStream::connect(address).await.unwrap());
        small_response(&mut replacement).await;
        drop(replacement);
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        count(&metrics, 0).await;
    }

    #[tokio::test]
    async fn disconnected_running_mutation_drains_on_shutdown_and_reopens() {
        use crate::test_support::TestStore;
        use axum::routing::post;
        use shrunk::{Point, Query};
        use std::sync::Mutex;

        let database = TestStore::new();
        let (executor, worker) = database.executor(2);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let limited = LimitedListener::new(listener, 2, executor.metrics());
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Arc::new(Mutex::new(Some(wait)));
        let started = Arc::new(Notify::new());
        let router = Router::new().route(
            "/mutate",
            post({
                let executor = executor.clone();
                let started = started.clone();
                move || {
                    let executor = executor.clone();
                    let started = started.clone();
                    let wait = wait.clone();
                    async move {
                        executor
                            .run(move |store| {
                                started.notify_one();
                                wait.lock().unwrap().take().unwrap().recv().unwrap();
                                store.db.append("disconnected", &[Point::new(1, 9.0)])?;
                                Ok(serde_json::json!({"written": 1}))
                            })
                            .await
                            .map(axum::Json)
                    }
                }
            }),
        );
        let (stop, shutdown) = oneshot::channel();
        let server = tokio::spawn({
            let executor = executor.clone();
            async move {
                axum::serve(limited, router)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown.await;
                        executor.close();
                    })
                    .await
                    .unwrap();
            }
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST /mutate HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        started.notified().await;
        drop(client);
        stop.send(()).unwrap();
        // Finish closes admission too, and must wait for this accepted write.
        let drained = tokio::spawn(worker.finish());
        tokio::task::yield_now().await;
        assert!(!drained.is_finished());
        release.send(()).unwrap();
        drained.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let db = shrunk::Db::open_rw(&database.0.join("shrunk.db")).unwrap();
        assert_eq!(
            db.query(&Query::new("disconnected", i64::MIN, i64::MAX))
                .unwrap()
                .points,
            vec![Point::new(1, 9.0)]
        );
    }
}
