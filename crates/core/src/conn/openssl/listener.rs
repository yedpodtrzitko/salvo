//! openssl module
use std::io::{Error as IoError, Result as IoResult};
use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use openssl::ssl::{Ssl, SslAcceptor};
use pin_project::pin_project;
use tokio::io::ErrorKind;
use tokio::net::ToSocketAddrs;
use tokio_openssl::SslStream;

use super::OpensslConfig;

use crate::async_trait;
use crate::conn::{Accepted, Acceptor, SocketAddr, TcpListener, IntoConfigStream, TlsConnStream};

/// OpensslListener
#[pin_project]
pub struct OpensslListener<C, T> {
    #[pin]
    config_stream: C,
    inner: T,
    tls_acceptor: Option<Arc<SslAcceptor>>,
}

impl<C> OpensslListener<C, TcpListener>
where
    C: IntoConfigStream<OpensslConfig> + Send + 'static,
{
    /// Bind to socket address.
    #[inline]
    pub async fn bind(config: C, addr: impl ToSocketAddrs) -> OpensslListener<C::Stream, TcpListener> {
        Self::try_bind(config, addr).await.unwrap()
    }
    /// Try to bind to socket address.
    #[inline]
    pub async fn try_bind(
        config: C,
        addr: impl ToSocketAddrs,
    ) -> IoResult<OpensslListener<C::Stream, TcpListener>> {
        let inner = TcpListener::try_bind(addr).await?;
        Ok(OpensslListener {
            config_stream: config.into_stream()?,
            inner,
            tls_acceptor: None,
        })
    }
}

impl<C, T> OpensslListener<C, T>
where
    C: Stream + Send + 'static,
    C::Item: Into<OpensslConfig>,
{
    /// Create new OpensslListener with config stream.
    #[inline]
    pub fn new(config_stream: C, inner: T) -> Self {
        Self {
            inner,
            config_stream,
            tls_acceptor: None,
        }
    }
}

#[async_trait]
impl<C, T> Acceptor for OpensslListener<C, T>
where
    C: Stream + Send + Unpin + 'static,
    C::Item: Into<OpensslConfig>,
    T: Acceptor,
{
    type Conn = TlsConnStream<SslStream<T::Conn>>;
    type Error = IoError;

    /// Get the local address bound to this listener.
    fn local_addrs(&self) -> Vec<&SocketAddr> {
        self.inner.local_addrs()
    }

    #[inline]
    async fn accept(&mut self) -> Result<Accepted<Self::Conn>, Self::Error> {
        loop {
            tokio::select! {
                tls_config = self.config_stream.next() => {
                    if let Some(tls_config) = tls_config {
                        match tls_config.into().create_acceptor_builder() {
                            Ok(builder) => {
                                if self.tls_acceptor.is_some() {
                                    tracing::info!("tls config changed.");
                                } else {
                                    tracing::info!("tls config loaded.");
                                }
                                self.tls_acceptor = Some(Arc::new(builder.build()));
                            },
                            Err(err) => tracing::error!(error = %err, "invalid tls config."),
                        }
                    } else {
                        unreachable!()
                    }
                }
                accepted = self.inner.accept() => {
                    let Accepted{stream, local_addr, remote_addr} = accepted.map_err(|e|IoError::new(ErrorKind::Other, e.to_string()))?;
                    let tls_acceptor = match &self.tls_acceptor {
                        Some(tls_acceptor) => tls_acceptor.clone(),
                        None => return Err(IoError::new(ErrorKind::Other, "no valid tls config.")),
                    };
                    let fut = async move {
                        let ssl = Ssl::new(tls_acceptor.context()).map_err(|err|
                            IoError::new(ErrorKind::Other, err.to_string()))?;
                        let mut tls_stream = SslStream::new(ssl, stream).map_err(|err|
                            IoError::new(ErrorKind::Other, err.to_string()))?;
                        use std::pin::Pin;
                        Pin::new(&mut tls_stream).accept().await.map_err(|err|
                            IoError::new(ErrorKind::Other, err.to_string()))?;
                        Ok(tls_stream) };
                    let stream = TlsConnStream::new(fut);
                    return Ok(Accepted{stream, local_addr, remote_addr});
                }
            }
        }
    }
}
