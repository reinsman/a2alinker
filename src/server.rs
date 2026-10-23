//! 库的 HTTP 服务入口：绑定可用端口、创建执行器，并按调用方提供的信号关闭。

use std::{future::Future, io::ErrorKind, net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use tokio::net::TcpListener;

use crate::{Options, a2a, runtime::Gateway};

pub struct Server {
    listener: TcpListener,
    router: Router,
    gateway: Arc<Gateway>,
}

impl Server {
    /// 端口占用时逐个递增；权限错误、非法地址等其他错误直接返回。
    /// 启动时检查原生协议，但不会发送模型 prompt。
    pub async fn bind(mut options: Options) -> Result<Self> {
        options.resolve()?;
        let listener = bind_available(options.server.listen).await?;
        options.server.listen = listener.local_addr()?;
        let gateway = Gateway::open(options).await?;
        let router = match a2a::router(gateway.clone()) {
            Ok(router) => router,
            Err(error) => {
                gateway.shutdown().await;
                return Err(error);
            }
        };
        Ok(Self {
            listener,
            router,
            gateway,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.gateway.options.server.listen
    }

    pub fn gateway(&self) -> &Arc<Gateway> {
        &self.gateway
    }

    /// 库不安装进程信号处理器；调用方决定何时关闭服务。
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let Self {
            listener,
            router,
            gateway,
        } = self;
        let storage = gateway.options.instance_directory()?;
        tracing::info!(listen = %gateway.options.server.listen, agent = gateway.options.agent.kind.as_str(), name = gateway.options.identity.name, workspace = %gateway.options.agent.workspace.display(), storage_root = %gateway.options.storage.directory.display(), storage = %storage.display(), "A2A server ready");
        tracing::info!(
            event = "a2a.endpoints",
            discovery_url = format!(
                "{}/.well-known/agent-card.json",
                gateway.options.public_url()
            ),
            rpc_url = format!("{}/a2a", gateway.options.public_url()),
            "GET Agent Card 发现能力，POST /a2a 提交消息；流式调用使用 SSE"
        );
        let stopping = gateway.clone();
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown.await;
                stopping.shutdown().await;
            })
            .await;
        gateway.shutdown().await;
        result.context("A2A server failed")
    }
}

async fn bind_available(mut address: SocketAddr) -> Result<TcpListener> {
    loop {
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrInUse => {
                let next = address
                    .port()
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("no available port at or above {}", address.port()))?;
                tracing::info!(listen = %address, next_port = next, "端口已占用，尝试下一个端口");
                address.set_port(next);
            }
            Err(error) => return Err(error).with_context(|| format!("cannot listen on {address}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn occupied_ports_are_skipped_and_other_errors_are_returned() {
        let held = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let initial = held.local_addr().unwrap();
        if initial.port() < u16::MAX {
            let listener = bind_available(initial).await.unwrap();
            assert!(listener.local_addr().unwrap().port() > initial.port());
        }
        assert!(
            bind_available("192.0.2.1:10100".parse().unwrap())
                .await
                .is_err()
        );
    }
}
