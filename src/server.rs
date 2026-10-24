//! 库的服务生命周期：每次 start 创建独立实例，句柄负责请求关闭并等待清理。

use std::{io::ErrorKind, net::SocketAddr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{Options, a2a, runtime::Gateway};

/// A2A 服务入口；库不初始化日志、不安装信号处理器，也不修改进程级环境变量。
pub struct Server;

/// 一个运行实例的控制句柄。不同句柄有独立的监听端口、任务队列和实例目录锁。
/// Drop 会请求后台关闭；需要确认进程和锁已释放时，必须 await shutdown 或 wait。
#[must_use = "保留句柄管理实例；丢弃句柄会请求关闭服务"]
#[derive(Debug)]
pub struct ServerHandle {
    name: String,
    address: SocketAddr,
    stop: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl Server {
    /// 返回前完成参数校验、端口绑定和原生协议预检，不请求模型推理。
    /// 端口占用时递增；同一个 Tokio 运行时可以启动多个独立实例。
    pub async fn start(mut options: Options) -> Result<ServerHandle> {
        options.resolve()?;
        let listener = bind_available(options.server.listen).await?;
        let address = listener.local_addr()?;
        options.server.listen = address;
        let gateway = Gateway::open(options).await?;
        let router = match a2a::router(gateway.clone()) {
            Ok(router) => router,
            Err(error) => {
                gateway.shutdown().await;
                return Err(error);
            }
        };
        let name = gateway.options.identity.name.clone();
        let stop = CancellationToken::new();
        let task = tokio::spawn(serve(listener, router, gateway, stop.clone()));
        Ok(ServerHandle {
            name,
            address,
            stop,
            task: Some(task),
        })
    }
}

impl ServerHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// 表示服务后台任务已退出，不代表上游模型账号或网络是否可用。
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// 只发出关闭请求；可重复调用，不阻塞 daemon 的管理循环。
    pub fn request_shutdown(&self) {
        self.stop.cancel();
    }

    /// 等待 HTTP 服务、任务 worker 和原生进程清理完成，返回后台运行结果。
    /// 取消此等待不会丢失 JoinHandle，也不会停止服务，可继续等待或主动关闭。
    pub async fn wait(&mut self) -> Result<()> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await.context("A2A service task failed");
        self.task = None;
        result?
    }

    /// 请求关闭并等待清理；返回后可用同一个名字重新启动实例。
    pub async fn shutdown(mut self) -> Result<()> {
        self.request_shutdown();
        self.wait().await
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn serve(
    listener: TcpListener,
    router: Router,
    gateway: Arc<Gateway>,
    stop: CancellationToken,
) -> Result<()> {
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
            stop.cancelled().await;
            stopping.shutdown().await;
        })
        .await;
    gateway.shutdown().await;
    gateway.store.close().await;
    result.context("A2A server failed")
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
