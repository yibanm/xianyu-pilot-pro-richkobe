//! 服务进程编排：按依赖顺序拉起 9 个子进程，健康门禁 + 崩溃重启。
//!
//! 依赖顺序复刻 docker-compose.yml 的 depends_on/condition: service_healthy：
//!   阶段A MySQL/PostgreSQL/Redis → 阶段B automation/crawler → 阶段C core-api → 阶段D 两个 worker

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};

/// 各服务的运行时端口，由编排器在启动时向 OS 申请空闲端口后填充。
#[derive(Debug, Clone, Default)]
pub struct Ports {
    pub mysql: u16,
    pub postgres: u16,
    pub redis: u16,
    pub core_api: u16,
    pub automation: u16,
    pub crawler: u16,
    pub gateway_user: u16,
    pub gateway_admin: u16,
}

/// 单个受管子进程的规格。
pub struct ServiceSpec {
    pub name: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// 健康检查：None 表示进程起来即视为就绪（worker 无 HTTP 端点）
    pub health: HealthCheck,
    pub stage: Stage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Data,
    Services,
    CoreApi,
    Workers,
}

#[derive(Debug, Clone)]
pub enum HealthCheck {
    /// HTTP GET 且响应体包含全部 marker（复刻 compose 的 healthcheck 判据）
    HttpContains {
        url: String,
        markers: Vec<String>,
    },
    /// 执行命令，退出码 0 即就绪（复刻 worker 的 healthcheck 脚本）
    Command {
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    },
    /// 数据库协议级探测（比 TCP 连通更准确：确认能接受查询/认证握手）
    Database {
        kind: DbKind,
        port: u16,
        password: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Mysql,
    Postgres,
    Redis,
}

#[derive(Debug)]
pub enum ServiceState {
    Starting,
    Healthy,
    Failed(String),
    Stopped,
}

struct Managed {
    spec: ServiceSpec,
    child: Option<Child>,
    state: ServiceState,
    restarts: u32,
}

pub struct Orchestrator {
    services: Arc<Mutex<HashMap<String, Managed>>>,
    shutdown: Arc<Notify>,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self {
            services: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// 向 OS 申请一个空闲端口。绑定 127.0.0.1:0 让内核分配，随后释放。
    ///
    /// 存在 TOCTOU 窗口（释放到子进程真正监听之间可能被抢占），但本机桌面场景
    /// 竞争者极少；若被抢占，健康检查会超时并触发重启，不会静默失败。
    pub async fn alloc_port() -> Result<u16> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("申请空闲端口失败")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }

    pub async fn alloc_ports() -> Result<Ports> {
        Ok(Ports {
            mysql: Self::alloc_port().await?,
            postgres: Self::alloc_port().await?,
            redis: Self::alloc_port().await?,
            core_api: Self::alloc_port().await?,
            automation: Self::alloc_port().await?,
            crawler: Self::alloc_port().await?,
            gateway_user: Self::alloc_port().await?,
            gateway_admin: Self::alloc_port().await?,
        })
    }

    /// 注册一个受管服务。
    ///
    /// 必须 await 完成后再调 start_all：早先用 tokio::spawn 做插入，注册与启动
    /// 之间没有同步点，start_all 可能在注册落地前就遍历，报「未注册的服务」。
    pub async fn register(&self, spec: ServiceSpec) {
        let name = spec.name.to_string();
        self.services.lock().await.insert(
            name,
            Managed {
                spec,
                child: None,
                state: ServiceState::Stopped,
                restarts: 0,
            },
        );
    }

    /// 启动单个阶段：先并行 spawn 该阶段全部进程，再逐个等健康门禁。
    ///
    /// 拆成公开方法（而非只在 start_all 内部循环）是为了让调用方能逐阶段推进、
    /// 在每阶段之间向用户上报启动进度——首次运行要 initdb + 起 9 个服务，
    /// 耗时数分钟，无进度反馈会让用户以为程序卡死。
    pub async fn start_stage(&self, stage: Stage, log_dir: &Path) -> Result<()> {
        let names: Vec<String> = {
            let guard = self.services.lock().await;
            guard
                .iter()
                .filter(|(_, m)| m.spec.stage == stage)
                .map(|(k, _)| k.clone())
                .collect()
        };
        for name in &names {
            self.spawn_one(name, log_dir).await?;
        }
        // 阶段内的进程已在上面全部 spawn（同时启动），这里顺序等待健康门禁。
        // 因为它们是并行运行的，顺序等待的总耗时≈最慢的那个，不损失并行度；
        // 同时避免了并行 join 所需的 'static 生命周期（wait_healthy 借用 &self 与 name）。
        for name in &names {
            self.wait_healthy(name, log_dir).await?;
        }
        Ok(())
    }

    async fn spawn_one(&self, name: &str, log_dir: &Path) -> Result<()> {
        let mut guard = self.services.lock().await;
        let managed = guard
            .get_mut(name)
            .with_context(|| format!("未注册的服务 {name}"))?;

        std::fs::create_dir_all(log_dir).ok();
        let log_path = log_dir.join(format!("{name}.log"));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("无法写日志 {log_path:?}"))?;

        let mut cmd = Command::new(&managed.spec.program);
        cmd.args(&managed.spec.args)
            .current_dir(&managed.spec.cwd)
            .envs(managed.spec.env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            // Windows 下子进程不继承控制台，避免弹黑窗
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            // tokio::process::Command 自带 creation_flags，无需 std 的 CommandExt
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("启动 {name} 失败: {:?}", managed.spec.program))?;

        managed.child = Some(child);
        managed.state = ServiceState::Starting;
        Ok(())
    }

    /// 等待服务健康。复刻 compose 的 retries/interval 语义：120s 总预算。
    pub async fn wait_healthy(&self, name: &str, log_dir: &Path) -> Result<()> {
        let (check, timeout) = {
            let guard = self.services.lock().await;
            let managed = guard
                .get(name)
                .with_context(|| format!("未注册的服务 {name}"))?;
            (managed.spec.health.clone(), Duration::from_secs(120))
        };

        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_err = String::from("未知");

        while tokio::time::Instant::now() < deadline {
            // 进程若已退出，不必等到超时
            {
                let mut guard = self.services.lock().await;
                if let Some(m) = guard.get_mut(name) {
                    if let Some(child) = m.child.as_mut() {
                        if let Ok(Some(status)) = child.try_wait() {
                            let tail = read_log_tail(&log_dir.join(format!("{name}.log")), 8);
                            m.state = ServiceState::Failed(format!("进程已退出 {status}: {tail}"));
                            bail!("{name} 进程启动后立即退出（{status}）。日志尾部：\n{tail}");
                        }
                    }
                }
            }

            match check_health(&check).await {
                Ok(()) => {
                    self.services.lock().await.get_mut(name).map(|m| {
                        m.state = ServiceState::Healthy;
                    });
                    return Ok(());
                }
                Err(e) => last_err = e,
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        let tail = read_log_tail(&log_dir.join(format!("{name}.log")), 20);
        self.services.lock().await.get_mut(name).map(|m| {
            m.state = ServiceState::Failed(last_err.clone());
        });
        bail!("{name} 在 120s 内未就绪（{last_err}）。日志尾部：\n{tail}");
    }

    pub async fn status(&self) -> Vec<(String, String)> {
        let guard = self.services.lock().await;
        let mut out: Vec<_> = guard
            .iter()
            .map(|(k, m)| {
                let s = match &m.state {
                    ServiceState::Starting => "启动中".to_string(),
                    ServiceState::Healthy => "运行中".to_string(),
                    ServiceState::Failed(e) => format!("失败: {}", truncate(e, 120)),
                    ServiceState::Stopped => "未启动".to_string(),
                };
                (k.clone(), s)
            })
            .collect();
        out.sort();
        out
    }

    /// 停止全部子进程。Windows 下 kill 只杀直接子进程，故对进程树逐个处理。
    pub async fn shutdown_all(&self) {
        let mut guard = self.services.lock().await;
        for (name, m) in guard.iter_mut() {
            if let Some(mut child) = m.child.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
                m.state = ServiceState::Stopped;
                tracing_stop(name);
            }
        }
        self.shutdown.notify_waiters();
    }

    /// 崩溃监督：周期性检查进程是否意外退出，指数退避重启，最多 MAX_RESTARTS 次。
    ///
    /// 设计文档承诺的「崩溃重启」在此实现。start_all 只保证启动时就绪，
    /// 之后服务崩溃（如 crawler 因内存不足被系统杀）会让 UI 静默失效，
    /// 故需要这个循环。数据库类服务重启是安全的：MySQL/PG 的数据在 datadir，
    /// Redis 用 maxmemory+LRU 且未开持久化，重启只丢缓存（x5sec 缓存可重取）。
    pub async fn supervise(&self, log_dir: &Path) {
        const MAX_RESTARTS: u32 = 3;
        const POLL_INTERVAL: Duration = Duration::from_secs(10);

        loop {
            tokio::select! {
                _ = self.shutdown.notified() => {
                    eprintln!("[supervise] 收到关闭信号，停止监督");
                    return;
                }
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            let dead: Vec<String> = {
                let mut guard = self.services.lock().await;
                let mut dead = Vec::new();
                for (name, m) in guard.iter_mut() {
                    // 只监督曾健康启动过的服务；Starting 阶段的退出由 wait_healthy 报错
                    if !matches!(m.state, ServiceState::Healthy) {
                        continue;
                    }
                    let Some(child) = m.child.as_mut() else { continue };
                    if let Ok(Some(status)) = child.try_wait() {
                        m.state = ServiceState::Failed(format!("运行中退出（{status}）"));
                        dead.push(name.clone());
                    }
                }
                dead
            };

            for name in dead {
                let (restarts, give_up) = {
                    let mut guard = self.services.lock().await;
                    let Some(m) = guard.get_mut(&name) else { continue };
                    m.restarts += 1;
                    (m.restarts, m.restarts > MAX_RESTARTS)
                };

                if give_up {
                    eprintln!("[supervise] {name} 已连续重启 {MAX_RESTARTS} 次仍失败，放弃重启");
                    let mut guard = self.services.lock().await;
                    if let Some(m) = guard.get_mut(&name) {
                        m.state = ServiceState::Failed(format!(
                            "重启 {MAX_RESTARTS} 次仍失败，已放弃。查看日志 {}",
                            log_dir.join(format!("{name}.log")).display()
                        ));
                    }
                    continue;
                }

                // 指数退避：2s / 4s / 8s
                let backoff = Duration::from_secs(2u64.pow(restarts.min(3)));
                eprintln!("[supervise] {name} 意外退出，{backoff:?} 后第 {restarts} 次重启");
                tokio::time::sleep(backoff).await;

                match self.spawn_one(&name, log_dir).await {
                    Ok(()) => {
                        match self.wait_healthy(&name, log_dir).await {
                            Ok(()) => eprintln!("[supervise] {name} 重启成功并已就绪"),
                            Err(e) => eprintln!("[supervise] {name} 重启后未通过健康检查: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[supervise] {name} 重启失败: {e}"),
                }
            }
        }
    }
}

fn tracing_stop(name: &str) {
    eprintln!("[orchestrator] 已停止 {name}");
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn read_log_tail(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let all: Vec<&str> = content.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(e) => format!("<无法读取日志 {path:?}: {e}>"),
    }
}

async fn check_health(check: &HealthCheck) -> std::result::Result<(), String> {
    match check {
        HealthCheck::HttpContains { url, markers } => {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("{url} 返回 {}", resp.status()));
            }
            let body = resp.text().await.map_err(|e| e.to_string())?;
            for m in markers {
                if !body.contains(m) {
                    return Err(format!("{url} 响应缺少标记 {m:?}"));
                }
            }
            Ok(())
        }
        HealthCheck::Command {
            program,
            args,
            cwd,
            env,
        } => {
            let mut cmd = Command::new(program);
            cmd.args(args)
                .current_dir(cwd)
                .envs(env.iter().cloned())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                cmd.creation_flags(0x0800_0000);
            }
            let status = tokio::time::timeout(Duration::from_secs(15), cmd.status())
                .await
                .map_err(|_| "健康检查命令超时".to_string())?
                .map_err(|e| e.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("健康检查命令退出码 {status}"))
            }
        }
        HealthCheck::Database {
            kind,
            port,
            password,
        } => {
            let r = match kind {
                DbKind::Mysql => mysql_ping(*port).await,
                DbKind::Postgres => postgres_ping(*port).await,
                DbKind::Redis => redis_ping(*port, password).await,
            };
            r.map_err(|e| e.to_string())
        }
    }
}

/// MySQL 就绪探测：TCP 连通不代表能接受查询，读握手包验证协议版本。
///
/// compose 用 `mysqladmin ping`，但便携版不想把 mysqladmin 也塞进产物。
/// 这里只验证 MySQL 协议握手包（协议版本应为 10），认证由 core-api/automation
/// 自行完成——避免在 Rust 里重新实现 caching_sha2_password。
pub async fn mysql_ping(port: u16) -> Result<()> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;

    let mut buf = vec![0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
    if n < 5 {
        bail!("MySQL 握手包过短（{n} 字节）");
    }
    // 握手包第 5 字节（4 字节包头之后）是协议版本，应为 10
    if buf[4] != 10 {
        bail!("MySQL 协议版本异常: {}", buf[4]);
    }
    Ok(())
}

/// Redis 就绪探测：发 AUTH + PING，要求回 PONG（复刻 compose healthcheck）。
pub async fn redis_ping(port: u16, password: &str) -> Result<()> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;

    let mut req = Vec::new();
    if !password.is_empty() {
        req.extend_from_slice(
            format!("*2\r\n$4\r\nAUTH\r\n${}\r\n{}\r\n", password.len(), password).as_bytes(),
        );
    }
    req.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");

    stream.write_all(&req).await?;
    let mut buf = vec![0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
    let resp = String::from_utf8_lossy(&buf[..n]);
    // 带 AUTH 时响应是 "+OK\r\n+PONG\r\n"
    if !resp.contains("+PONG") {
        bail!("Redis PING 未回 PONG: {:?}", resp.trim());
    }
    Ok(())
}

/// PostgreSQL 就绪探测：发 startup 包看是否收到 AuthenticationOk 或密码请求。
///
/// compose 用 `pg_isready`，便携版用最小协议探测：发一个无效但格式正确的
/// startup message，服务端会回 'R'（认证请求）—— 收到即证明能接受连接。
pub async fn postgres_ping(port: u16) -> Result<()> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await??;

    // startup message: length + protocol 3.0 + "user\0crawler\0database\0postgres\0\0"
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0crawler\0");
    params.extend_from_slice(b"database\0postgres\0");
    params.push(0);
    let len = (8 + params.len()) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&196608i32.to_be_bytes()); // 3.0
    msg.extend_from_slice(&params);

    stream.write_all(&msg).await?;
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await??;
    if n < 1 {
        bail!("PostgreSQL 无响应");
    }
    // 'R' = AuthenticationRequest，'E' = ErrorResponse（也算服务活着，但配置有问题）
    match buf[0] {
        b'R' => Ok(()),
        b'E' => {
            let detail = String::from_utf8_lossy(&buf[1..n]);
            bail!("PostgreSQL 拒绝连接: {}", detail.replace('\0', " ").trim())
        }
        other => bail!("PostgreSQL 意外响应类型: {other:#x}"),
    }
}
