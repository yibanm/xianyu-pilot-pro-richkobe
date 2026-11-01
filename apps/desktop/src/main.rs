mod bootstrap;
mod gateway;
mod layout;
mod orchestrator;
mod secrets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use orchestrator::{DbKind, HealthCheck, Orchestrator, Ports, ServiceSpec, Stage};

/// 运行时状态，交给 Tauri command 供前端查询服务状态。
struct AppState {
    orchestrator: Arc<Orchestrator>,
    admin_url: String,
}

fn main() -> Result<()> {
    // 编排与网关全部跑在 Tauri 内置的 tokio runtime 上。
    // 不用 #[tokio::main]：Windows 的事件循环必须留在主线程，
    // 而 Tauri 的 async_runtime 本身就是 tokio，两者可以共用。
    let prepared = tauri::async_runtime::block_on(prepare())?;

    let state = Arc::new(AppState {
        orchestrator: prepared.orchestrator,
        admin_url: prepared.admin_url.clone(),
    });

    let user_url = prepared.user_url.clone();

    tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![service_status, open_admin])
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(user_url.parse().context("网关 URL 非法")?),
            )
            .title("闲鱼助手 · 科比版")
            .inner_size(1280.0, 800.0)
            .min_inner_size(900.0, 600.0)
            .build()?;
            Ok(())
        })
        .on_window_event(move |window, event| {
            // 窗口关闭即停掉全部子进程，避免留下孤儿 mysqld/postgres/node
            if let tauri::WindowEvent::Destroyed = event {
                if window.label() == "main" {
                    let orch = state.orchestrator.clone();
                    tauri::async_runtime::block_on(orch.shutdown_all());
                }
            }
        })
        .run(tauri::generate_context!())
        .context("Tauri 运行时错误")?;

    Ok(())
}

struct Prepared {
    orchestrator: Arc<Orchestrator>,
    user_url: String,
    admin_url: String,
}

/// 全部启动工作：布局校验 → 密钥 → 端口 → 首次初始化 → 编排 → 网关。
async fn prepare() -> Result<Prepared> {
    let layout = layout::detect_layout().context("定位目录布局失败")?;
    // MySQL/PG 在非 ASCII 路径下会以难以诊断的方式失败，启动即校验
    layout::assert_ascii_path(&layout.root, "程序根目录")?;
    layout::ensure_dirs(&layout)?;

    let secrets = secrets::Secrets::load_or_create(&layout.secrets_file())
        .context("加载/生成密钥失败")?;

    let ports = Orchestrator::alloc_ports().await.context("分配端口失败")?;
    eprintln!(
        "[main] 端口: mysql={} pg={} redis={} core-api={} automation={} crawler={} gw-user={} gw-admin={}",
        ports.mysql, ports.postgres, ports.redis, ports.core_api,
        ports.automation, ports.crawler, ports.gateway_user, ports.gateway_admin
    );

    if layout::is_first_run(&layout) {
        eprintln!("[main] 首次运行，初始化数据库…");
        bootstrap::bootstrap(&layout, &secrets, &ports)
            .await
            .context("数据库初始化失败")?;
        eprintln!("[main] 数据库初始化完成");
    }

    let orchestrator = Arc::new(Orchestrator::new());
    register_services(&orchestrator, &layout, &secrets, &ports)?;

    eprintln!("[main] 启动服务…");
    orchestrator
        .start_all(&layout.logs)
        .await
        .context("服务启动失败")?;
    eprintln!("[main] 服务全部就绪");

    // 崩溃监督：后台循环，服务意外退出时指数退避重启（最多 3 次）。
    // clone log_dir 而非 move layout——layout 后面还要用于构建网关站点配置。
    {
        let orch = orchestrator.clone();
        let log_dir = layout.logs.clone();
        tauri::async_runtime::spawn(async move {
            orch.supervise(&log_dir).await;
        });
    }

    let user_addr = gateway::serve(build_user_site(&layout, &ports), ports.gateway_user)
        .await
        .context("user-web 网关启动失败")?;
    let admin_addr = gateway::serve(build_admin_site(&layout, &ports), ports.gateway_admin)
        .await
        .context("admin-web 网关启动失败")?;

    let user_url = format!("http://{user_addr}");
    let admin_url = format!("http://{admin_addr}");
    eprintln!("[main] 网关就绪: user={user_url} admin={admin_url}");

    Ok(Prepared {
        orchestrator,
        user_url,
        admin_url,
    })
}

#[tauri::command]
async fn service_status(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<(String, String)>, String> {
    Ok(state.orchestrator.status().await)
}

#[tauri::command]
async fn open_admin(state: tauri::State<'_, Arc<AppState>>, app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    // 管理后台是独立端口，用第二个窗口打开（两个前端 Vite base 都是 /，无法共用端口）
    let url: tauri::Url = state.admin_url.parse().map_err(|e| format!("admin URL 非法: {e}"))?;
    if app.get_webview_window("admin").is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "admin", WebviewUrl::External(url))
        .title("闲鱼助手 · 管理后台")
        .inner_size(1280.0, 800.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn register_services(
    orch: &Arc<Orchestrator>,
    layout: &layout::Layout,
    secrets: &secrets::Secrets,
    ports: &Ports,
) -> Result<()> {
    let rt = tauri::async_runtime::handle();

    // ---- 阶段 A：数据库 ----
    let mysql_cnf = win(&layout.config_dir().join("my.ini"));
    rt.block_on(orch.register(ServiceSpec {
        name: "mysql",
        program: layout.mysqld(),
        args: vec![format!("--defaults-file={mysql_cnf}")],
        cwd: layout.mysql_home(),
        env: vec![],
        health: HealthCheck::Database {
            kind: DbKind::Mysql,
            port: ports.mysql,
            password: String::new(),
        },
        stage: Stage::Data,
    }));

    rt.block_on(orch.register(ServiceSpec {
        name: "postgres",
        program: layout.postgres_bin(),
        args: vec!["-D".into(), win(&layout.postgres_data())],
        cwd: layout.postgres_home(),
        // 清空 locale，避免中文 Windows 的 collation 干扰（阶段 0 实测用 --no-locale 初始化）
        env: vec![("LC_ALL".into(), "C".into())],
        health: HealthCheck::Database {
            kind: DbKind::Postgres,
            port: ports.postgres,
            password: String::new(),
        },
        stage: Stage::Data,
    }));

    rt.block_on(orch.register(ServiceSpec {
        name: "redis",
        program: layout.redis_server(),
        args: vec![
            "--port".into(),
            ports.redis.to_string(),
            "--requirepass".into(),
            secrets.redis_password.clone(),
            "--save".into(),
            String::new(),
            "--appendonly".into(),
            "no".into(),
            // 官方 .env.production.example 给 Redis 2GB，那是服务器预设；
            // 桌面版限 128MB + LRU 淘汰，实测常驻约 48MB
            "--maxmemory".into(),
            "134217728".into(),
            "--maxmemory-policy".into(),
            "allkeys-lru".into(),
            "--dir".into(),
            win(&layout.redis_data()),
        ],
        cwd: layout.redis_data(),
        env: vec![],
        health: HealthCheck::Database {
            kind: DbKind::Redis,
            port: ports.redis,
            password: secrets.redis_password.clone(),
        },
        stage: Stage::Data,
    }));

    // ---- 共用环境变量（compose 里 automation 与 backend 共享的那组）----
    let common: Vec<(String, String)> = vec![
        ("MYSQL_HOST".into(), "127.0.0.1".into()),
        ("MYSQL_PORT".into(), ports.mysql.to_string()),
        ("MYSQL_USER".into(), secrets.mysql_app_user.clone()),
        ("MYSQL_PASSWORD".into(), secrets.mysql_app_password.clone()),
        ("MYSQL_DATABASE".into(), secrets.mysql_database.clone()),
        ("INTERNAL_API_TOKEN".into(), secrets.internal_api_token.clone()),
        ("COOKIE_CRYPTO_SECRET".into(), secrets.cookie_crypto_secret.clone()),
        ("ADMIN_JWT_SECRET".into(), secrets.admin_jwt_secret.clone()),
        ("ADMIN_JWT_ISSUER".into(), "xianyu-core-api".into()),
        ("ADMIN_JWT_AUDIENCE".into(), "xianyu-user-api".into()),
        ("JWT_ISSUER".into(), "xianyu-core-api".into()),
        ("JWT_AUDIENCE".into(), "xianyu-user-api".into()),
        ("REDIS_HOST".into(), "127.0.0.1".into()),
        ("REDIS_PORT".into(), ports.redis.to_string()),
        ("REDIS_PASSWORD".into(), secrets.redis_password.clone()),
        (
            "CORE_API_BASE_URL".into(),
            format!("http://127.0.0.1:{}", ports.core_api),
        ),
        (
            "CRAWLER_SERVICE_URL".into(),
            format!("http://127.0.0.1:{}", ports.crawler),
        ),
    ];

    // ---- 阶段 B：automation + crawler ----
    let mut automation_env = common.clone();
    automation_env.push(("SERVER_PORT".into(), ports.automation.to_string()));
    // 绑回环：原 run-fast.py 写死 0.0.0.0，桌面版不应对局域网暴露
    automation_env.push(("SERVER_HOST".into(), "127.0.0.1".into()));

    rt.block_on(orch.register(ServiceSpec {
        name: "automation",
        program: layout.python(),
        args: vec!["run-fast.py".into()],
        cwd: layout.automation_dir(),
        env: automation_env,
        health: HealthCheck::HttpContains {
            url: format!("http://127.0.0.1:{}/health", ports.automation),
            markers: vec!["Python backend is running".into()],
        },
        stage: Stage::Services,
    }));

    let crawler_env: Vec<(String, String)> = vec![
        ("CRAWLER_DB_HOST".into(), "127.0.0.1".into()),
        ("CRAWLER_DB_PORT".into(), ports.postgres.to_string()),
        ("CRAWLER_DB_USER".into(), secrets.crawler_db_user.clone()),
        ("CRAWLER_DB_PASSWORD".into(), secrets.crawler_db_password.clone()),
        ("CRAWLER_DB".into(), secrets.crawler_db.clone()),
        ("REDIS_HOST".into(), "127.0.0.1".into()),
        ("REDIS_PORT".into(), ports.redis.to_string()),
        ("REDIS_PASSWORD".into(), secrets.redis_password.clone()),
        ("PORT".into(), ports.crawler.to_string()),
        ("INTERNAL_API_TOKEN".into(), secrets.internal_api_token.clone()),
        ("COOKIE_CRYPTO_SECRET".into(), secrets.cookie_crypto_secret.clone()),
        // HEADLESS=true 是 sliderSolver 的既定配置，compose 注释明写「严禁改动」
        ("HEADLESS".into(), "true".into()),
        // 桌面版并发降到 2/1：官方 compose 默认 4/2 是给服务器 8GB+ 内存的
        ("CRAWLER_BROWSER_CONCURRENCY".into(), "2".into()),
        ("CRAWLER_BROWSER_CONCURRENCY_PER_TENANT".into(), "1".into()),
    ];

    rt.block_on(orch.register(ServiceSpec {
        name: "crawler-service",
        program: layout.node(),
        args: vec!["dist/server.js".into()],
        cwd: layout.crawler_dir(),
        env: crawler_env.clone(),
        health: HealthCheck::HttpContains {
            url: format!("http://127.0.0.1:{}/api/health", ports.crawler),
            markers: vec!["ok".into()],
        },
        stage: Stage::Services,
    }));

    // ---- 阶段 C：core-api ----
    // JDBC URL 逐参数对齐 compose 的 SPRING_DATASOURCE_URL
    let jdbc = format!(
        "jdbc:mysql://127.0.0.1:{port}/{db}?useUnicode=true&characterEncoding=utf8&useSSL=false\
         &serverTimezone=Asia/Shanghai&allowPublicKeyRetrieval=true&rewriteBatchedStatements=true",
        port = ports.mysql,
        db = secrets.mysql_database
    );

    let mut core_env = common.clone();
    core_env.extend([
        ("SPRING_DATASOURCE_URL".into(), jdbc),
        (
            "SPRING_DATASOURCE_USERNAME".into(),
            secrets.mysql_app_user.clone(),
        ),
        (
            "SPRING_DATASOURCE_PASSWORD".into(),
            secrets.mysql_app_password.clone(),
        ),
        // 首次运行需要 seed 出默认 admin 账号（compose 的本地体验流程）
        ("ADMIN_SEED_ENABLED".into(), "true".into()),
        ("SCHEMA_RUNTIME_MUTATIONS_ENABLED".into(), "true".into()),
        (
            "AUTOMATION_BASE_URL".into(),
            format!("http://127.0.0.1:{}", ports.automation),
        ),
        (
            "CRAWLER_BASE_URL".into(),
            format!("http://127.0.0.1:{}", ports.crawler),
        ),
        ("SERVER_PORT".into(), ports.core_api.to_string()),
        ("SERVER_ADDRESS".into(), "127.0.0.1".into()),
        ("XIANYU_UPLOAD_ROOT_DIR".into(), win(&layout.uploads())),
        ("JWT_EXPIRE_SECONDS".into(), "86400".into()),
        ("MEDIA_COOKIE_SECURE".into(), "false".into()),
        ("PAYMENT_SANDBOX_ENABLED".into(), "true".into()),
        ("SPRING_PROFILES_ACTIVE".into(), "prod".into()),
    ]);

    let jar = layout
        .core_api_jar()
        .join("xianyu-assistant-admin-backend-1.0.0.jar");

    rt.block_on(orch.register(ServiceSpec {
        name: "core-api",
        program: layout.java(),
        // JVM 参数沿用官方 dev-start.ps1 的低内存组合，另限堆 512MB
        args: vec![
            "-Xmx512m".into(),
            "-XX:+TieredCompilation".into(),
            "-XX:TieredStopAtLevel=1".into(),
            "-XX:+UseParallelGC".into(),
            "-Djava.awt.headless=true".into(),
            "-Dfile.encoding=UTF-8".into(),
            "-jar".into(),
            win(&jar),
        ],
        cwd: layout.core_api_jar(),
        env: core_env,
        // compose 的 healthcheck 判据就是 grep '"UP"'
        health: HealthCheck::HttpContains {
            url: format!("http://127.0.0.1:{}/api/health", ports.core_api),
            markers: vec!["UP".into()],
        },
        stage: Stage::CoreApi,
    }));

    // ---- 阶段 D：两个 worker ----
    let mut worker_env = common.clone();
    worker_env.push(("WORKER_INTERVAL_SECONDS".into(), "60".into()));
    worker_env.push((
        "WORKER_HEARTBEAT_FILE".into(),
        win(&layout.data.join("automation-worker-heartbeat")),
    ));

    rt.block_on(orch.register(ServiceSpec {
        name: "automation-worker",
        program: layout.python(),
        args: vec!["run-worker.py".into()],
        cwd: layout.automation_dir(),
        env: worker_env,
        // compose 用 `python -m app.worker_healthcheck` 检查心跳文件
        health: HealthCheck::Command {
            program: layout.python(),
            args: vec!["-m".into(), "app.worker_healthcheck".into()],
            cwd: layout.automation_dir(),
            env: vec![("WORKER_HEARTBEAT_FILE".into(), win(&layout.data.join("automation-worker-heartbeat")))],
        },
        stage: Stage::Workers,
    }));

    let mut crawler_worker_env = crawler_env.clone();
    // compose 里 crawler-worker 靠 xvfb 提供 DISPLAY 让 goofish.ts 走有头模式。
    // Windows 有真实桌面，不需要 xvfb，但代码是按 DISPLAY 判断的，
    // 故显式给出一个 DISPLAY 值让它走有头分支（降低 Baxia 风控触发率）。
    crawler_worker_env.push(("DISPLAY".into(), ":0".into()));

    rt.block_on(orch.register(ServiceSpec {
        name: "crawler-worker",
        program: layout.node(),
        args: vec!["dist/worker.js".into()],
        cwd: layout.crawler_dir(),
        env: crawler_worker_env,
        health: HealthCheck::Command {
            program: layout.node(),
            args: vec!["dist/healthcheck.js".into()],
            cwd: layout.crawler_dir(),
            env: vec![],
        },
        stage: Stage::Workers,
    }));

    Ok(())
}

/// user-web 站点的反代规则，逐条对齐 apps/user-web/nginx.conf。
fn build_user_site(layout: &layout::Layout, ports: &Ports) -> gateway::SiteConfig {
    let core = Arc::new(tokio::sync::Mutex::new(format!(
        "http://127.0.0.1:{}",
        ports.core_api
    )));
    let rule = |prefix: &'static str, timeout: u64, carousel: bool| gateway::ProxyRule {
        prefix,
        strip_prefix: false,
        target: core.clone(),
        read_timeout: Duration::from_secs(timeout),
        carousel_rewrite: carousel,
    };
    gateway::SiteConfig {
        root: layout.user_web_dist(),
        name: "user-web",
        proxies: vec![
            // 长前缀在前：carousel 重写规则必须先于通用 /uploads/ 命中
            rule("/uploads/images/carousel_", 300, true),
            rule("/uploads/", 300, false),
            // SSE 必须 3600s（nginx 原值），否则长连接被网关掐断
            rule("/api/sse/", 3600, false),
            rule("/api/", 300, false),
            rule("/ai/", 300, false),
        ],
    }
}

/// admin-web 站点的反代规则，逐条对齐 apps/admin-web/nginx.conf。
fn build_admin_site(layout: &layout::Layout, ports: &Ports) -> gateway::SiteConfig {
    let core = Arc::new(tokio::sync::Mutex::new(format!(
        "http://127.0.0.1:{}",
        ports.core_api
    )));
    let rule = |prefix: &'static str, timeout: u64| gateway::ProxyRule {
        prefix,
        strip_prefix: false,
        target: core.clone(),
        read_timeout: Duration::from_secs(timeout),
        carousel_rewrite: false,
    };
    gateway::SiteConfig {
        root: layout.admin_web_dist(),
        name: "admin-web",
        proxies: vec![
            rule("/uploads/", 300),
            // admin-api 是 60s（nginx 原值），与 user-web 的 /api/ 300s 不同
            rule("/admin-api/", 60),
        ],
    }
}

fn win(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}
