# Rust 代码阅读指南

本文结合 a2alinker 的实际代码介绍阅读顺序和常见语法，不要求先掌握完整的 Rust。

## 从哪里开始读

| 顺序 | 文件 | 关注的问题 |
| --- | --- | --- |
| 1 | [main.rs](../src/main.rs) | 程序如何启动，日志与参数如何初始化 |
| 2 | [cli.rs](../src/cli.rs) | serve、doctor 如何分派，配置如何覆盖 |
| 3 | [config.rs](../src/config.rs) | TOML 对应什么结构，默认值和校验在哪里 |
| 4 | [adapters/mod.rs](../src/adapters/mod.rs) | 所有 agent 共用什么接口和事件 |
| 5 | [runtime.rs](../src/runtime.rs) | 任务如何排队、执行、取消和保存结果 |
| 6 | [adapters/process.rs](../src/adapters/process.rs) | 子进程如何启动、交换 JSONL 和清理 |
| 7 | [adapters/pi.rs](../src/adapters/pi.rs)、[adapters/codex.rs](../src/adapters/codex.rs) | 不同原生协议如何实现统一接口 |
| 8 | [storage.rs](../src/storage.rs)、[a2a/mod.rs](../src/a2a/mod.rs) | 状态如何持久化，HTTP/SSE 如何调用调度器 |
| 9 | [skills.rs](../src/skills.rs) | 技能包如何发现、筛选、复制并验证 |

源文件开头的 `//!` 说明整个模块，函数和类型前的 `///` 会生成 API 文档，普通 `//` 解释局部逻辑。

## 常见语法在本项目中的含义

| 写法 | 怎么理解 | 实际例子 |
| --- | --- | --- |
| `struct` | 一组有名字的字段 | `Gateway` 保存配置、数据库、队列和控制通道 |
| `enum`、`match` | 一组明确的可能情况，并分别处理 | `AgentKind` 区分 pi 和 Codex |
| `impl Type` | 为类型定义方法 | `impl Gateway` 中定义提交与取消操作 |
| `trait`、`impl Trait for Type` | 定义公共接口，再为具体类型实现它 | 两个适配器实现 `AgentSession` |
| `pub` | 允许外部代码访问 | `lib.rs` 公开项目的各个模块 |
| `let mut` | 声明之后允许修改的变量 | 更新任务状态前使用 `let mut task` |
| `Option<T>` | 有值是 `Some(value)`，无值是 `None` | 未指定 `--model` 时不覆盖原生模型设置 |
| `Result<T, E>` | 成功是 `Ok(value)`，失败是 `Err(error)` | 文件读取、数据库查询和协议请求都可能失败 |
| `?` | 成功则取出值，失败则提前返回错误 | `Config::load(...)?` 把读取错误交给调用方 |
| `Vec<T>`、`HashMap<K,V>` | 可增长数组、键值映射 | 技能列表、任务控制表 |
| `#[derive(...)]` | 由编译期宏生成常用实现 | serde 生成配置的序列化、反序列化逻辑 |
| `#[cfg(unix)]` | 只在指定平台编译 | Unix 的进程组与 SIGTERM 处理 |

本项目经常使用 `anyhow::Result<T>`，相当于省略通用错误类型的 `Result<T, anyhow::Error>`。
`ensure!(条件, "错误")` 在条件不成立时返回错误；`context("说明")` 在原错误外补充上下文，便于定位失败步骤。

`unwrap()` 则不同：值为 `None` 或结果为 `Err` 时会 panic。生产代码中例如取出已设为 `piped` 的 stdin，依赖的是此前已建立的程序内部条件；客户端输入和正常运行时错误应使用校验与 `Result` 处理。panic 本身不等于内存不安全。

## 所有权与借用

Rust 通过所有权和借用约束数据访问和释放，许多资源在离开作用域时自动清理。

- `value: T` 通常把值的所有权交给被调用者；可复制类型也可能直接复制。
- `&T` 是共享借用，调用者保留所有权，被调用者只能读取。
- `&mut T` 是独占可变借用，允许修改，同时限制其他代码并发访问该值。
- `String` 拥有字符串内容，`&str` 借用字符串片段；`PathBuf` 和 `&Path` 也是类似关系。

例如 `SkillSet::discover(&config)` 只借用配置；`skills.snapshot(...)` 取得技能集合的所有权，修改为副本路径后返回。

`Arc<T>` 允许多个持有者共享同一个对象。`gateway.clone()` 克隆的是引用计数句柄，不会复制数据库或重新启动服务。它不自动让内部数据可修改；本项目通过 `Mutex` 保护需要修改的共享数据。

`Weak<T>` 是不增加强引用计数的弱引用。worker 空闲等待时持有 `Weak<Gateway>`，需要执行工作时才 `upgrade()`；若网关已释放，会得到 `None` 并退出。

`Box<dyn AgentSession>` 拥有一个具体会话，但调用方只使用公共接口，因此调度器不需要分别编写 pi 和 Codex 的执行流程。

## async、通道与自动清理

`async fn` 返回一个 Future，可以理解成尚待推进的异步工作；`.await` 等待它的结果，并在等待 I/O 时允许运行时执行其他任务。异步操作不一定新建操作系统线程，`tokio::spawn` 才会把一个 Future 提交为独立调度的后台任务。

`tokio::select!` 同时等待几个异步操作，例如 agent 事件、取消信号和超时。某个分支就绪后处理该分支；循环中的其他工作继续等待。`tokio::pin!(run)` 将同一次会话执行固定下来，使循环能持续等待它，而不是每轮重新发起推理。

`mpsc` 是多个发送者、一个接收者的通道：HTTP 提交任务到队列，唯一 worker 接收并串行执行；agent 事件与交互回复也使用各自的通道。通道有容量上限，写满时等待或拒绝，形成流量约束。

`broadcast` 通知多个等待者重新读取状态。它只传递“有变化”的提醒，完整事件保存在 SQLite 中，因此 SSE 不依赖通知保存历史。

`Drop` 在值被释放时执行清理。`JsonlProcess` 会终止进程组并中止读取任务，临时目录会删除文件，锁的 guard 会释放锁。`Drop` 不能执行异步等待；正常关机还会显式调用 `Gateway::shutdown().await` 等待后台 worker 结束。

SQLite 写操作通过事务保证完整性。例如创建任务时，任务、去重记录和初始事件一起提交；中途 `?` 返回错误，没有提交的事务会回滚。读取 SSE 初始快照时，任务内容和事件游标也在同一事务里读取。

## 一次请求经过哪些函数

1. `a2a::rpc` 认证并检查 JSON-RPC 格式，`dispatch` 解析具体方法。
2. `Gateway::submit` 校验消息、检查去重，预留队列位置后写入数据库。
3. `worker` 接收工作，`execute` 创建或恢复原生会话，再调用 `session.run`。
4. 适配器把原生消息转为 `AgentEvent`，`Gateway::event` 更新 Task 和 Artifact 并保存事件。
5. HTTP 等待者与 SSE 订阅者读取最新状态；`Gateway::finish` 保存终态并释放任务控制资源。

`doctor` 走的是版本检查、临时配置、技能检查和会话握手流程，不会走上述 `session.run`，因此不请求模型推理。

## unsafe 检查结果与约束

此前有 3 处显式 `unsafe`：生产代码 1 处用于结束 Unix 进程组，测试代码 2 处用于检查和结束测试子进程，均为直接调用 `libc::kill`。
现在改用 `nix::sys::signal` 提供的安全接口，仓库自身的 Rust 源码和测试不再包含 `unsafe` 实现。进程组号经过有检查的整数转换，并拒绝特殊组号；测试仅把 `ESRCH` 作为进程不存在的结果。

[Cargo.toml](../Cargo.toml) 配置了 `unsafe_code = "forbid"`，编译器会拒绝本包中新加入的 unsafe 代码。该限制适用于本包的库、可执行程序和测试；第三方依赖有自己的实现，仍可能在内部使用 unsafe，不能据此声称整个依赖树都不含 unsafe。

可运行 `cargo check --all-targets --locked` 检查此约束，运行 `cargo doc --no-deps --document-private-items --locked` 生成包含中文注释的本地文档，入口为 `target/doc/a2alinker/index.html`。
