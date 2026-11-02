//! 首次运行初始化：建数据目录、初始化两个数据库、建库建用户授权。
//!
//! 表结构**不在这里创建** —— 三个服务启动时各自建表（实测确认）：
//!   - core-api：`SchemaCompatibilityRunner` 是 ApplicationRunner + @Order(HIGHEST_PRECEDENCE)
//!   - crawler-service：`server.ts` 启动时调 `runMigrations()`
//!   - automation-service：`main.py` 启动时调 `Base.metadata.create_all`
//! 因此本模块只负责「数据库实例存在 + 库和账号可用」。

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::layout::Layout;
use crate::orchestrator::{mysql_ping, postgres_ping, Ports};
use crate::secrets::Secrets;

/// MySQL 就绪等待预算。首次 initdb 要建系统库，比后续启动慢。
const MYSQL_INIT_TIMEOUT_SECS: u64 = 180;
const POSTGRES_INIT_TIMEOUT_SECS: u64 = 120;

/// 执行首次初始化。调用前须已确认 `is_first_run()` 为真。
///
/// `step` 回调用于把每个子步骤（initdb / 启动 / 建库 / 关闭）上报到启动页——
/// 首启耗时数分钟，无步骤级反馈时用户只能看到「初始化数据库…」干等，
/// 且出问题时日志里也看不出卡在哪一步（实测踩过：desktop.log 停在第 4 行）。
pub async fn bootstrap(
    layout: &Layout,
    secrets: &Secrets,
    ports: &Ports,
    step: crate::ProgressFn,
) -> Result<()> {
    // 数据库初始化对路径编码敏感，这里再校验一次（detect 后用户可能移动过目录）
    crate::layout::assert_ascii_path(&layout.mysql_data(), "MySQL 数据目录")?;
    crate::layout::assert_ascii_path(&layout.postgres_data(), "PostgreSQL 数据目录")?;
    crate::layout::assert_ascii_path(&layout.mysql_home(), "MySQL 程序目录")?;
    crate::layout::assert_ascii_path(&layout.postgres_home(), "PostgreSQL 程序目录")?;

    init_mysql(layout, secrets, ports, &step).await?;
    init_postgres(layout, secrets, ports, &step).await?;

    Ok(())
}

// ---------------------------------------------------------------- MySQL

async fn init_mysql(
    layout: &Layout,
    secrets: &Secrets,
    ports: &Ports,
    step: &crate::ProgressFn,
) -> Result<()> {
    let datadir = &layout.mysql_data();

    if !datadir.join("mysql").is_dir() {
        step("初始化 MySQL 数据目录（首次约 1 分钟）…".into());
        // --initialize-insecure：root 初始无密码，随后立即改密。
        // 用 insecure 而非 --initialize 是因为后者会把随机密码只写进日志，
        // 解析日志取密码在 Windows 上受编码影响不可靠。
        run(
            &layout.mysqld(),
            &[
                "--initialize-insecure",
                &format!("--datadir={}", win_path(datadir)),
                &format!("--basedir={}", win_path(&layout.mysql_home())),
            ],
            None,
            &[],
        )
        .await
        .context("MySQL initialize-insecure 失败")?;
        step("MySQL 数据目录初始化完成".into());
    } else {
        step("MySQL 数据目录已存在，跳过初始化".into());
    }

    // 用临时配置文件启动（端口动态、绑定回环、低内存）
    let cnf = write_mysql_cnf(layout, ports, &secrets.mysql_root_password)?;

    step("启动 MySQL 以建库建用户…".into());
    let mut child = spawn_detached(
        &layout.mysqld(),
        &[&format!("--defaults-file={}", win_path(&cnf))],
        None,
        &[],
    )
    .context("启动 mysqld 失败")?;

    // 等就绪
    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(MYSQL_INIT_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() > deadline {
            let _ = child.kill().await;
            let tail = read_tail(&layout.logs.join("bootstrap-mysql.log"), 20);
            bail!("MySQL 在 {MYSQL_INIT_TIMEOUT_SECS}s 内未就绪。日志尾部：\n{tail}");
        }
        if mysql_ping(ports.mysql).await.is_ok() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    }

    // 建库 + 建应用账号 + 授权 + 设置 root 密码
    // 注意：不用 --skip-name-resolve，否则 root@localhost 无法用 127.0.0.1 连接
    // （阶段 0 实测：该参数会让 TCP 连接报 ERROR 1130 Host not allowed）
    let mysql_client = layout.mysql_home().join("bin").join("mysql.exe");
    let sql = format!(
        "ALTER USER 'root'@'localhost' IDENTIFIED BY '{root}';\n\
         CREATE DATABASE IF NOT EXISTS `{db}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;\n\
         CREATE USER IF NOT EXISTS '{user}'@'localhost' IDENTIFIED BY '{pass}';\n\
         CREATE USER IF NOT EXISTS '{user}'@'127.0.0.1' IDENTIFIED BY '{pass}';\n\
         GRANT ALL PRIVILEGES ON `{db}`.* TO '{user}'@'localhost';\n\
         GRANT ALL PRIVILEGES ON `{db}`.* TO '{user}'@'127.0.0.1';\n\
         FLUSH PRIVILEGES;",
        root = esc(&secrets.mysql_root_password),
        db = secrets.mysql_database,
        user = secrets.mysql_app_user,
        pass = esc(&secrets.mysql_app_password),
    );

    run_stdin(
        &mysql_client,
        &[
            "-u",
            "root",
            "-h",
            "127.0.0.1",
            &format!("--port={}", ports.mysql),
        ],
        &sql,
    )
    .await
    .context("MySQL 建库建用户失败")?;

    eprintln!("[bootstrap] MySQL 就绪，库与账号已创建");

    // 关掉这个临时实例，稍后由编排器按正式参数启动。
    // 时序：上面 ALTER USER 已把 root 密码设为 secrets 里的值，
    // 而 my.ini 的 [client] 段正是该密码，故此处可直接用配置文件认证。
    shutdown_mysql(&layout.mysql_home(), &cnf).await?;
    let _ = child.wait().await;

    Ok(())
}

fn write_mysql_cnf(layout: &Layout, ports: &Ports, root_password: &str) -> Result<std::path::PathBuf> {
    let cnf = layout.config_dir().join("my.ini");
    // 低内存配置：官方 .env.production.example 的 MYSQL_MEMORY_LIMIT_BYTES 是 12GB，
    // 那是服务器预设。桌面版实测 64MB buffer pool 下 mysqld 仅占约 29MB 常驻。
    let content = format!(
        "[mysqld]\r\n\
         basedir={base}\r\n\
         datadir={data}\r\n\
         port={port}\r\n\
         bind-address=127.0.0.1\r\n\
         character-set-server=utf8mb4\r\n\
         collation-server=utf8mb4_unicode_ci\r\n\
         innodb_buffer_pool_size=64M\r\n\
         innodb_log_buffer_size=8M\r\n\
         innodb_buffer_pool_instances=1\r\n\
         key_buffer_size=8M\r\n\
         performance_schema=OFF\r\n\
         max_connections=50\r\n\
         table_open_cache=200\r\n\
         log-error={log}\r\n\
         pid-file={pid}\r\n\
         \r\n\
         [client]\r\n\
         port={port}\r\n\
         user=root\r\n\
         password=\"{pwd}\"\r\n",
        base = win_path(&layout.mysql_home()),
        data = win_path(&layout.mysql_data()),
        port = ports.mysql,
        log = win_path(&layout.logs.join("mysql.log")),
        pid = win_path(&layout.data.join("mysql.pid")),
        pwd = root_password,
    );
    std::fs::write(&cnf, content).with_context(|| format!("写入 {cnf:?} 失败"))?;
    Ok(cnf)
}

/// 关闭 bootstrap 阶段临时启动的 mysqld。
///
/// 用 mysqladmin.exe（不是 mysql.exe + "admin" 参数——那会被当成数据库名）。
/// 密码通过 --defaults-file 传入而非命令行，避免出现在进程列表里。
async fn shutdown_mysql(mysql_home: &Path, cnf: &Path) -> Result<()> {
    let admin = mysql_home.join("bin").join("mysqladmin.exe");
    let status = Command::new(&admin)
        .args([&format!("--defaults-file={}", win_path(cnf)), "shutdown"])
        .creation_flags_no_window()
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    // shutdown 失败不致命：进程会继续跑，后续编排器启动同端口实例时
    // 会因端口被占而失败，并由健康门禁报出，不会静默出错。
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            eprintln!("[bootstrap] mysqladmin shutdown 退出码 {s}（可能已停止）");
            Ok(())
        }
        Err(e) => {
            eprintln!("[bootstrap] mysqladmin 调用失败: {e}");
            Ok(())
        }
    }
}

// ------------------------------------------------------------ PostgreSQL

async fn init_postgres(
    layout: &Layout,
    secrets: &Secrets,
    ports: &Ports,
    step: &crate::ProgressFn,
) -> Result<()> {
    let datadir = &layout.postgres_data();

    if !datadir.join("PG_VERSION").exists() {
        step("初始化 PostgreSQL 数据目录（initdb，约 30 秒）…".into());
        // --no-locale + UTF8：与阶段 0 实测通过的参数一致，避免本地 collation 差异
        run(
            &layout.initdb(),
            &[
                "-D",
                &win_path(datadir),
                "-U",
                &secrets.crawler_db_user,
                "--auth=trust",
                "--encoding=UTF8",
                "--no-locale",
            ],
            // initdb 会读 PG 的 locale 环境变量，显式清空避免干扰。
            // cwd 必须是 None：datadir 此刻尚不存在（initdb 的职责才是创建它），
            // 把不存在的目录当 current_dir 会让 spawn 直接报
            // Windows ERROR_DIRECTORY(267)「目录名称无效」——用户实测踩到过。
            None,
            &[("LC_ALL", "C")],
        )
        .await
        .context("PostgreSQL initdb 失败")?;
        step("PostgreSQL 数据目录初始化完成".into());
    } else {
        step("PostgreSQL 数据目录已存在，跳过初始化".into());
    }

    // 低内存配置（实测 shared_buffers=32MB 下常驻约 71MB）
    let pg_conf = datadir.join("postgresql.conf");
    let extra = format!(
        "\r\n\
         # --- 桌面版低内存配置（由 Tauri 壳写入）---\r\n\
         port = {port}\r\n\
         listen_addresses = '127.0.0.1'\r\n\
         shared_buffers = 32MB\r\n\
         work_mem = 2MB\r\n\
         maintenance_work_mem = 16MB\r\n\
         effective_cache_size = 128MB\r\n\
         max_connections = 30\r\n\
         wal_buffers = 1MB\r\n\
         fsync = on\r\n",
        port = ports.postgres
    );
    // 幂等：先移除上次写入的段落再追加
    if let Ok(existing) = std::fs::read_to_string(&pg_conf) {
        if let Some(idx) = existing.find("# --- 桌面版低内存配置") {
            std::fs::write(&pg_conf, &existing[..idx]).context("清理 postgresql.conf 失败")?;
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&pg_conf)
        .with_context(|| format!("打开 {pg_conf:?} 失败"))?;
    std::io::Write::write_all(&mut f, extra.as_bytes())
        .context("写入 postgresql.conf 失败")?;

    // 密码认证：写 pg_hba.conf，把 trust 换成 scram-sha-256
    // 注意顺序：此时**不能**写入 scram 版 pg_hba.conf。
    // initdb --auth=trust 生成的是 trust 认证，必须先靠它连进去设密码，
    // 再切换到 scram；反过来的话 PG 一启动就要求密码，而密码还不存在，直接死锁。

    step("启动 PostgreSQL 以建库…".into());
    let mut child = spawn_detached(
        &layout.postgres_bin(),
        &["-D", &win_path(datadir)],
        None,
        // PG 在 Windows 上要求 PGDATA 或 -D；同时清空 locale 变量
        &[("LC_ALL", "C")],
    )
    .context("启动 postgres 失败")?;

    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(POSTGRES_INIT_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() > deadline {
            let _ = child.kill().await;
            bail!("PostgreSQL 在 {POSTGRES_INIT_TIMEOUT_SECS}s 内未就绪");
        }
        if postgres_ping(ports.postgres).await.is_ok() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
    }

    // initdb -U <crawler_db_user> 已把该用户建为超级用户，这里只设密码 + 建库。
    // 不能再 CREATE USER 同名账号（会报 role already exists）。
    let psql = layout.postgres_home().join("bin").join("psql.exe");
    let sql = format!(
        "ALTER USER {su} WITH PASSWORD '{sup}';\n\
         CREATE DATABASE {db} OWNER {su} ENCODING 'UTF8';\n\
         GRANT ALL PRIVILEGES ON DATABASE {db} TO {su};",
        su = secrets.crawler_db_user,
        sup = esc(&secrets.crawler_db_password),
        db = secrets.crawler_db,
    );
    // 此刻 pg_hba.conf 仍是 initdb --auth=trust 生成的免密认证，
    // 所以这一步不需要密码即可连入；密码由上面的 ALTER USER 设置。
    step("创建 PostgreSQL 业务库与账号…".into());
    run_pg(
        &psql,
        ports.postgres,
        &secrets.crawler_db_user,
        "postgres",
        &sql,
    )
    .await
    .context("PostgreSQL 建库失败")?;

    // 密码已设置，现在才切到 scram 认证（顺序不能反，见上方注释）
    step("切换 PostgreSQL 为密码认证…".into());
    write_pg_hba(datadir)?;

    step("关闭临时 PostgreSQL 实例…".into());

    // 优雅关闭：pg_ctl stop
    let pg_ctl = layout.postgres_home().join("bin").join("pg_ctl.exe");
    let _ = Command::new(&pg_ctl)
        .args(["-D", &win_path(datadir), "-m", "fast", "stop"])
        .creation_flags_no_window()
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let _ = child.wait().await;

    Ok(())
}

fn write_pg_hba(datadir: &Path) -> Result<()> {
    let hba = datadir.join("pg_hba.conf");
    // 只允许本机 TCP + 密码认证；不开 trust，避免同机其他程序免密连库
    let content = "\
# 桌面版：仅允许本机连接，强制密码认证
# TYPE  DATABASE        USER            ADDRESS                 METHOD
host    all             all             127.0.0.1/32            scram-sha-256
host    all             all             ::1/128                 scram-sha-256
";
    std::fs::write(&hba, content).with_context(|| format!("写入 {hba:?} 失败"))?;
    Ok(())
}

async fn run_pg(
    psql: &Path,
    port: u16,
    user: &str,
    db: &str,
    sql: &str,
) -> Result<()> {
    let child = Command::new(psql)
        .args([
            "-h",
            "127.0.0.1",
            &format!("--port={port}"),
            "-U",
            user,
            "-d",
            db,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .env("PGCLIENTENCODING", "UTF8")
        // trust 模式下不需要密码；若已启用 scram 则由调用方通过 PGPASSWORD 提供
        .creation_flags_no_window()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 psql 失败")?;

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(RUN_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .context("等待 psql 结束超时")?
    .context("等待 psql 结束失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!("psql 失败（{}）\nstdout: {stdout}\nstderr: {stderr}", out.status);
    }
    Ok(())
}

// ------------------------------------------------------------ 通用工具

/// Windows 路径：MySQL/PG 对反斜杠敏感，统一转成反斜杠形式。
fn win_path(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}

/// SQL 字符串转义（单引号翻倍），防注入与语法错。
fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

/// 运行命令并要求成功退出。stderr 收集后一并报错，便于诊断。
///
/// 带 600s 超时：initdb/initialize 正常在 3 分钟内完成；无超时时一旦子进程
/// 挂住（等输入、等锁、等网络），整个启动流程会无限等待，用户只能干等
/// （实测踩过：desktop.log 停在「初始化数据库」一行，5 分钟无任何后续）。
const RUN_TIMEOUT_SECS: u64 = 600;

async fn run(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.creation_flags_no_window();

    let out = tokio::time::timeout(std::time::Duration::from_secs(RUN_TIMEOUT_SECS), cmd.output())
        .await
        .with_context(|| {
            format!("{program:?} 执行超过 {RUN_TIMEOUT_SECS}s 未返回，判定为挂起")
        })?
        .with_context(|| format!("无法执行 {program:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "{} 失败（{}）\n参数: {args:?}\nstdout: {stdout}\nstderr: {stderr}",
            program.display(),
            out.status
        );
    }
    Ok(())
}

/// 运行命令并从 stdin 喂 SQL（MySQL 客户端不接受 -c）。
async fn run_stdin(program: &Path, args: &[&str], stdin_data: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags_no_window()
        .spawn()
        .with_context(|| format!("无法执行 {program:?}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_data.as_bytes())
            .await
            .context("写入 SQL 失败")?;
        // 显式 drop 关闭管道，否则客户端一直等输入而挂住
        drop(stdin);
    }

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(RUN_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .context("等待命令结束超时")?
    .context("等待命令结束失败")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "{} 失败（{}）\nstdout: {stdout}\nstderr: {stderr}",
            program.display(),
            out.status
        );
    }
    Ok(())
}

/// 启动长驻进程（不由本函数等待）。
fn spawn_detached(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Result<tokio::process::Child> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.creation_flags_no_window();
    cmd.spawn()
        .with_context(|| format!("无法启动 {program:?}"))
}

fn read_tail(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(c) => {
            let all: Vec<&str> = c.lines().collect();
            let start = all.len().saturating_sub(lines);
            all[start..].join("\n")
        }
        Err(e) => format!("<无法读取 {path:?}: {e}>"),
    }
}

/// CREATE_NO_WINDOW：避免每个子进程弹一个黑色控制台窗口。
trait CreationFlagsNoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl CreationFlagsNoWindow for Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            // tokio::process::Command 自带 creation_flags，无需 std 的 CommandExt
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}
