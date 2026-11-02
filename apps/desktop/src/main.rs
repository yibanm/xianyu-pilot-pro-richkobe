// 关键：release 构建必须声明 windows 子系统，否则 exe 以控制台程序启动，
// 双击时会先弹出一个黑色命令行窗口（Tauri 官方模板首行即此属性）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;
mod gateway;
mod layout;
mod orchestrator;
mod secrets;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use orchestrator::{DbKind, HealthCheck, Orchestrator, Ports, ServiceSpec, Stage};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

/// 运行期共享状态。
///
/// orchestrator 与 admin_url 在启动流程完成后才填充，故用 Option：
/// 窗口先出现（显示启动进度），服务在后台拉起，命令调用时可能尚未就绪。
struct AppState {
    orchestrator: Mutex<Option<Arc<Orchestrator>>>,
    admin_url: Mutex<Option<String>>,
    layout: Mutex<Option<layout::Layout>>,
}

/// 启动进度回调。
///
/// 用 Arc 而非泛型闭包：闭包会捕获 WebviewWindow（非 Copy），泛型 F: Copy
/// 约束无法满足；且 run_startup 内部还要再包一层写日志，Arc 便于共享。
type ProgressFn = Arc<dyn Fn(String) + Send + Sync>;

/// 把 Rust 字符串安全编码为 JS 字符串字面量（含引号），供 window.eval 使用。
///
/// 启动进度与错误信息会含用户路径、换行、引号，直接拼进 JS 会造成语法错误
/// 甚至注入。用 serde_json 转义可一并覆盖全部特殊字符。
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn main() {
    let state = Arc::new(AppState {
        orchestrator: Mutex::new(None),
        admin_url: Mutex::new(None),
        layout: Mutex::new(None),
    });

    // setup 与 on_window_event 都是 move 闭包，会各自消费一个 Arc。
    // 这里为 on_window_event 预备独立副本，避免与 setup 争用同一个 state。
    let state_for_window = state.clone();

    let result = tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            service_status,
            open_admin,
            open_logs_dir,
            quit_app
        ])
        .setup(move |app| {
            // 窗口立即创建并显示启动页：绝不在建窗前做任何耗时/可能失败的工作。
            // 否则用户双击后长时间看不到界面，且启动失败时窗口一闪而过无从诊断。
            let win = WebviewWindowBuilder::new(app, "boot", WebviewUrl::App("index.html".into()))
                .title("闲鱼助手 · 科比版")
                .inner_size(720.0, 520.0)
                .resizable(false)
                .build()?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let app_state = state.clone();
                let window = win.clone();

                // 进度上报：通过 eval 调用启动页里的 __bootProgress()。
                // 用 eval 而非 Tauri 事件——事件需要在 capabilities 里声明权限，
                // 而启动页是打包进 frontendDist 的本地文件，eval 零配置可用。
                // 用 Arc<dyn Fn> 传递：闭包捕获的 WebviewWindow 不是 Copy，
                // 直接传闭包会因 Copy 约束编译失败。
                let progress: ProgressFn = Arc::new({
                    let w = window.clone();
                    move |msg: String| {
                        eprintln!("[boot] {msg}");
                        let _ = w.eval(&format!("window.__bootProgress({})", js_string(&msg)));
                    }
                });

                match run_startup(&app_state, progress).await {
                    Ok(url) => {
                        // 服务与网关就绪：新建承载真实 UI 的窗口，再关掉启动页。
                        // 不在同一窗口 location.replace 到外部 http URL——那受
                        // CSP 与 Tauri 导航权限限制，独立窗口最稳。
                        let parsed: Result<tauri::Url, _> = url.parse();
                        match parsed {
                            Ok(u) => {
                                match WebviewWindowBuilder::new(
                                    &handle,
                                    "main",
                                    WebviewUrl::External(u),
                                )
                                .title("闲鱼助手 · 科比版")
                                .inner_size(1280.0, 800.0)
                                .min_inner_size(900.0, 600.0)
                                .build()
                                {
                                    Ok(_) => {
                                        let _ = window.close();
                                    }
                                    Err(e) => {
                                        show_fatal(&window, &format!("打开主界面失败: {e}"));
                                    }
                                }
                            }
                            Err(e) => show_fatal(&window, &format!("网关 URL 非法: {e}")),
                        }
                    }
                    Err(e) => {
                        // 错误显示在启动页里（含日志目录按钮），不静默退出。
                        // 静默退出会让用户只看到窗口一闪而过，无从判断原因。
                        show_fatal(&window, &format!("{e:#}"));
                    }
                }
            });

            Ok(())
        })
        .on_window_event(move |window, event| {
            if !matches!(event, tauri::WindowEvent::Destroyed) {
                return;
            }
            let label = window.label().to_string();
            let app = window.app_handle().clone();
            let st = state_for_window.clone();

            // 清理子进程的两个时机：
            // 1. 主界面窗口关闭 —— 用户结束使用；
            // 2. 启动窗口关闭且主界面尚未建立 —— 启动失败被用户关掉，
            //    此时 mysqld/postgres 等可能已经拉起，必须回收，否则留下孤儿进程。
            // 启动成功后我们会关掉启动窗口并打开主窗口，那种情况不能清理，
            // 故用「主窗口是否已存在」区分，而不是单纯按 label 判断。
            let main_exists = app.get_webview_window("main").is_some();
            let should_shutdown = label == "main" || (label == "boot" && !main_exists);
            if !should_shutdown {
                return;
            }

            tauri::async_runtime::block_on(async move {
                if let Some(orch) = st.orchestrator.lock().await.take() {
                    orch.shutdown_all().await;
                }
            });
            // 启动失败路径下主窗口不存在，Tauri 默认在最后一个窗口关闭时退出，
            // 这里显式退出确保子进程清理后进程不会悬挂。
            if label == "boot" {
                app.exit(0);
            }
        })
        .run(tauri::generate_context!());

    if let Err(e) = result {
        eprintln!("Tauri 运行时错误: {e}");
        std::process::exit(1);
    }
}

/// 在启动页显示致命错误，并把日志目录路径一并给出。
fn show_fatal(window: &tauri::WebviewWindow, msg: &str) {
    eprintln!("[boot] 启动失败: {msg}");
    // 附加日志目录，让用户能自行取日志反馈
    let hint = match layout::detect_layout() {
        Ok(l) => format!("\n\n日志目录: {}", l.logs.display()),
        Err(_) => String::new(),
    };
    let _ = window.eval(&format!(
        "window.__bootError({})",
        js_string(&format!("{msg}{hint}"))
    ));
}

/// 完整启动流程。任一步失败返回 Err，由调用方显示到窗口。
async fn run_startup(app_state: &Arc<AppState>, progress: ProgressFn) -> Result<String> {
    progress("检查程序目录…".into());
    let layout = layout::detect_layout().context("定位目录布局失败")?;
    // MySQL/PG 在非 ASCII 路径下会以难以诊断的方式失败，启动即校验并给出明确指引
    layout::assert_ascii_path(&layout.root, "程序根目录")?;
    layout::ensure_dirs(&layout)?;
    *app_state.layout.lock().await = Some(layout.clone());

    // 主程序自身日志：windows_subsystem=windows 下 stderr 会丢失，
    // 出问题时必须能从文件回看，否则用户无从反馈、维护者无从诊断。
    let desktop_log = layout.logs.join("desktop.log");

    // 包一层：进度在上报到启动页的同时写入日志文件，形成可回溯的启动记录。
    // 保持 ProgressFn 类型不变，便于后续继续传递。
    let progress: ProgressFn = Arc::new({
        let inner = progress.clone();
        let log_path = desktop_log.clone();
        move |msg: String| {
            inner(msg.clone());
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                use std::io::Write;
                let _ = writeln!(f, "[boot] {msg}");
            }
        }
    });

    progress("准备密钥…".into());
    let secrets = secrets::Secrets::load_or_create(&layout.secrets_file())
        .context("加载/生成密钥失败")?;

    progress("分配本机端口…".into());
    let ports = Orchestrator::alloc_ports().await.context("分配端口失败")?;
    progress(format!(
        "端口就绪（MySQL {} / PostgreSQL {} / Redis {} / API {}）",
        ports.mysql, ports.postgres, ports.redis, ports.core_api
    ));

    if layout::is_first_run(&layout) {
        progress("首次运行：初始化数据库（约 1-3 分钟，请勿关闭窗口）…".into());
        bootstrap::bootstrap(&layout, &secrets, &ports)
            .await
            .context("数据库初始化失败")?;
        progress("数据库初始化完成".into());
    }

    let orchestrator = Arc::new(Orchestrator::new());
    // 创建后立即存入共享状态：若后续某个阶段启动失败，on_window_event 的清理
    // 逻辑仍能拿到 orchestrator 关掉已拉起的进程，不会留下孤儿 mysqld/postgres。
    *app_state.orchestrator.lock().await = Some(orchestrator.clone());

    register_services(&orchestrator, &layout, &secrets, &ports).await?;

    progress("启动数据库服务…".into());
    orchestrator
        .start_stage(Stage::Data, &layout.logs)
        .await
        .context("数据库启动失败")?;

    progress("启动自动化与爬虫服务…".into());
    orchestrator
        .start_stage(Stage::Services, &layout.logs)
        .await
        .context("业务服务启动失败")?;

    progress("启动核心后端（首次启动较慢，约 30-60 秒）…".into());
    orchestrator
        .start_stage(Stage::CoreApi, &layout.logs)
        .await
        .context("核心后端启动失败")?;

    progress("启动后台任务…".into());
    orchestrator
        .start_stage(Stage::Workers, &layout.logs)
        .await
        .context("后台任务启动失败")?;

    // 崩溃监督：服务意外退出时指数退避重启
    {
        let orch = orchestrator.clone();
        let log_dir = layout.logs.clone();
        tauri::async_runtime::spawn(async move {
            orch.supervise(&log_dir).await;
        });
    }

    progress("启动本地网关…".into());
    let user_addr = gateway::serve(build_user_site(&layout, &ports), ports.gateway_user)
        .await
        .context("用户端网关启动失败")?;
    let admin_addr = gateway::serve(build_admin_site(&layout, &ports), ports.gateway_admin)
        .await
        .context("管理后台网关启动失败")?;

    *app_state.admin_url.lock().await = Some(format!("http://{admin_addr}"));

    progress("全部服务就绪，正在打开界面…".into());
    Ok(format!("http://{user_addr}"))
}

#[tauri::command]
async fn service_status(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<(String, String)>, String> {
    let guard = state.orchestrator.lock().await;
    match guard.as_ref() {
        Some(orch) => Ok(orch.status().await),
        None => Ok(vec![("(启动中)".into(), "服务尚未就绪".into())]),
    }
}

#[tauri::command]
async fn open_admin(state: tauri::State<'_, Arc<AppState>>, app: tauri::AppHandle) -> Result<(), String> {
    let url_str = state
        .admin_url
        .lock()
        .await
        .clone()
        .ok_or_else(|| "管理后台尚未就绪".to_string())?;
    if app.get_webview_window("admin").is_some() {
        return Ok(());
    }
    let url: tauri::Url = url_str.parse().map_err(|e| format!("admin URL 非法: {e}"))?;
    WebviewWindowBuilder::new(&app, "admin", WebviewUrl::External(url))
        .title("闲鱼助手 · 管理后台")
        .inner_size(1280.0, 800.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 打开日志目录，便于用户反馈问题时取日志。
#[tauri::command]
async fn open_logs_dir(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    let dir = match state.layout.lock().await.clone() {
        Some(l) => l.logs,
        None => layout::detect_layout()
            .map_err(|e| format!("{e:#}"))?
            .logs,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("打开目录失败: {e}"))?;
    Ok(())
}

/// 退出应用（启动失败时用户可主动关闭，确保子进程被清理）。
#[tauri::command]
async fn quit_app(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    if let Some(orch) = state.orchestrator.lock().await.take() {
        orch.shutdown_all().await;
    }
    app.exit(0);
    Ok(())
}

/// 注册全部受管服务。
///
/// 必须是 async：内部逐个 await orchestrator.register()。
/// 不能用 `async_runtime::handle().block_on()` —— 本函数在 async 上下文中被调用，
/// 在已运行的 tokio runtime 里嵌套 block_on 会直接 panic。
async fn register_services(
    orch: &Arc<Orchestrator>,
    layout: &layout::Layout,
    secrets: &secrets::Secrets,
    ports: &Ports,
) -> Result<()> {
    // ---- 阶段 A：数据库 ----
    let mysql_cnf = win(&layout.config_dir().join("my.ini"));
    orch.register(ServiceSpec {
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
    }).await;

    orch.register(ServiceSpec {
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
    }).await;

    orch.register(ServiceSpec {
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
    }).await;

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

    orch.register(ServiceSpec {
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
    }).await;

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
        // 不内置 Chromium：复用系统 Chrome/Edge（见 crawler/browserChannel.ts）
        ("SLIDER_BROWSER_CHANNEL".into(), "msedge".into()),
    ];

    orch.register(ServiceSpec {
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
    }).await;

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

    orch.register(ServiceSpec {
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
    }).await;

    // ---- 阶段 D：两个 worker ----
    let mut worker_env = common.clone();
    worker_env.push(("WORKER_INTERVAL_SECONDS".into(), "60".into()));
    worker_env.push((
        "WORKER_HEARTBEAT_FILE".into(),
        win(&layout.data.join("automation-worker-heartbeat")),
    ));

    orch.register(ServiceSpec {
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
            env: vec![(
                "WORKER_HEARTBEAT_FILE".into(),
                win(&layout.data.join("automation-worker-heartbeat")),
            )],
        },
        stage: Stage::Workers,
    }).await;

    let mut crawler_worker_env = crawler_env.clone();
    // compose 里 crawler-worker 靠 xvfb 提供 DISPLAY 让 goofish.ts 走有头模式。
    // Windows 有真实桌面不需要 xvfb，但代码按 DISPLAY 判断，故显式给值。
    crawler_worker_env.push(("DISPLAY".into(), ":0".into()));

    orch.register(ServiceSpec {
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
    }).await;

    Ok(())
}

/// user-web 站点的反代规则，逐条对齐 apps/user-web/nginx.conf。
fn build_user_site(layout: &layout::Layout, ports: &Ports) -> gateway::SiteConfig {
    let core = Arc::new(Mutex::new(format!(
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
    let core = Arc::new(Mutex::new(format!(
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
