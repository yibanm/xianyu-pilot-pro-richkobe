//! 运行时目录布局与路径校验。
//!
//! 硬约束（阶段 0 实测）：MySQL 与 PostgreSQL 的 basedir/datadir 必须位于纯 ASCII 路径。
//! 中文路径下 PG 的 initdb 报 `invalid byte sequence for encoding "UTF8"` 并自动删除数据目录；
//! MySQL 更隐蔽——静默不启动且连 --log-error 指定的日志文件都不生成。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// 桌面版目录布局。
///
/// ```text
/// <root>/
///   runtime/{jre,python,node,mysql,postgres,redis,vcruntime}
///   app/{core-api,automation,crawler,web/user,web/admin}
///   data/{mysql,postgres,redis,uploads,config}
///   logs/
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct Layout {
    pub root: PathBuf,
    pub runtime: PathBuf,
    pub app: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
}

impl Layout {
    pub fn jre(&self) -> PathBuf {
        self.runtime.join("jre")
    }
    pub fn java(&self) -> PathBuf {
        self.jre().join("bin").join("java.exe")
    }
    pub fn python(&self) -> PathBuf {
        self.runtime.join("python").join("python.exe")
    }
    pub fn node(&self) -> PathBuf {
        self.runtime.join("node").join("node.exe")
    }
    pub fn mysql_home(&self) -> PathBuf {
        self.runtime.join("mysql")
    }
    pub fn mysqld(&self) -> PathBuf {
        self.mysql_home().join("bin").join("mysqld.exe")
    }
    pub fn postgres_home(&self) -> PathBuf {
        self.runtime.join("postgres")
    }
    pub fn postgres_bin(&self) -> PathBuf {
        self.postgres_home().join("bin").join("postgres.exe")
    }
    pub fn initdb(&self) -> PathBuf {
        self.postgres_home().join("bin").join("initdb.exe")
    }
    pub fn redis_server(&self) -> PathBuf {
        self.runtime.join("redis").join("redis-server.exe")
    }

    pub fn core_api_jar(&self) -> PathBuf {
        self.app.join("core-api")
    }
    pub fn automation_dir(&self) -> PathBuf {
        self.app.join("automation")
    }
    pub fn crawler_dir(&self) -> PathBuf {
        self.app.join("crawler")
    }
    pub fn user_web_dist(&self) -> PathBuf {
        self.app.join("web").join("user")
    }
    pub fn admin_web_dist(&self) -> PathBuf {
        self.app.join("web").join("admin")
    }

    pub fn mysql_data(&self) -> PathBuf {
        self.data.join("mysql")
    }
    pub fn postgres_data(&self) -> PathBuf {
        self.data.join("postgres")
    }
    pub fn redis_data(&self) -> PathBuf {
        self.data.join("redis")
    }
    pub fn uploads(&self) -> PathBuf {
        self.data.join("uploads")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.data.join("config")
    }
    /// 凭据文件（含数据库密码与三个 JWT/crypto 密钥）。
    /// 权限须收紧，且永不写入日志。
    pub fn secrets_file(&self) -> PathBuf {
        self.config_dir().join("secrets.json")
    }
}

/// 从 exe 所在位置推导布局。
///
/// 三种来源，按优先级探测：
/// 1. **便携版**：runtime/app 与 exe 同级（CI 打包时这样摆放），data/logs 也放同级，
///    做到「解压即用、数据随程序走」；
/// 2. **安装版（NSIS）**：Tauri 把 bundle.resources 放到 `<exe目录>/resources/` 下，
///    故 runtime/app 在 `<exe>/resources/`；data/logs 必须放到可写的
///    `%LOCALAPPDATA%\XianyuPilotPro\`——安装目录（Program Files）通常只读，
///    且升级会覆盖，把数据库放那里会丢数据；
/// 3. **开发期**：`XYPILOT_ROOT` 指向已准备好的根目录。
pub fn detect_layout() -> Result<Layout> {
    let exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let exe_dir = exe.parent().context("exe 无父目录")?.to_path_buf();

    // 开发期显式指定优先
    if let Ok(env_root) = std::env::var("XYPILOT_ROOT") {
        let root = PathBuf::from(env_root);
        if !root.is_dir() {
            bail!("XYPILOT_ROOT 指向的目录不存在: {root:?}");
        }
        if !root.join("runtime").is_dir() || !root.join("app").is_dir() {
            bail!("XYPILOT_ROOT={root:?} 下缺少 runtime/ 或 app/");
        }
        return Ok(Layout {
            runtime: root.join("runtime"),
            app: root.join("app"),
            data: root.join("data"),
            logs: root.join("logs"),
            root,
        });
    }

    // 便携版判据：runtime 直接在 exe 同级
    let portable = exe_dir.join("runtime").is_dir() && exe_dir.join("app").is_dir();
    // 安装版判据：runtime 在 exe/resources 下（Tauri resources 落地位置）
    let resources_dir = exe_dir.join("resources");
    let installed = resources_dir.join("runtime").is_dir() && resources_dir.join("app").is_dir();

    let bundle_root = if portable {
        exe_dir.clone()
    } else if installed {
        resources_dir.clone()
    } else {
        bail!(
            "未找到运行时布局：{:?} 及其 resources/ 子目录下都没有 runtime/ 与 app/。\n\
             安装版请通过安装包正确安装；开发期请设 XYPILOT_ROOT 指向已准备好的根目录。",
            exe_dir
        );
    };

    // data/logs：便携版随程序走；安装版放可写的 %LOCALAPPDATA%
    let writable_root = if portable {
        exe_dir.clone()
    } else {
        local_app_data()?.join("XianyuPilotPro")
    };

    Ok(Layout {
        runtime: bundle_root.join("runtime"),
        app: bundle_root.join("app"),
        data: writable_root.join("data"),
        logs: writable_root.join("logs"),
        root: bundle_root,
    })
}

/// `%LOCALAPPDATA%`，取不到时回退到用户主目录下的 AppData\Local。
///
/// 不用 exe 目录存数据：安装版通常装在 Program Files（只读），且升级会覆盖。
fn local_app_data() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        let path = PathBuf::from(p);
        if path.is_dir() {
            return Ok(path);
        }
    }
    let home = std::env::var("USERPROFILE").context("无法定位用户目录（LOCALAPPDATA 与 USERPROFILE 均缺失）")?;
    Ok(PathBuf::from(home).join("AppData").join("Local"))
}

/// 校验路径为纯 ASCII，否则 MySQL/PostgreSQL 会以难以诊断的方式失败。
///
/// 安装器应优先装到 `%LOCALAPPDATA%\Programs\...`（天然 ASCII）；便携版解压到
/// 中文路径时，本函数在启动阶段就给出明确错误，而不是等到 initdb 崩溃。
pub fn assert_ascii_path(path: &Path, what: &str) -> Result<()> {
    let s = path.to_string_lossy();
    if !s.is_ascii() {
        let bad: String = s.chars().filter(|c| !c.is_ascii()).collect();
        bail!(
            "{what} 的路径必须为纯 ASCII，当前包含非 ASCII 字符：{bad}\n\
             路径：{s}\n\n\
             原因：MySQL 与 PostgreSQL 在非 ASCII 路径下会失败——\n\
             PostgreSQL 的 initdb 报 \"invalid byte sequence for encoding UTF8\" 并自动删除数据目录；\n\
             MySQL 静默不启动且不生成任何日志。\n\n\
             解决：把程序安装/解压到纯英文路径（例如 %LOCALAPPDATA%\\Programs\\XianyuPilotPro）。"
        );
    }
    Ok(())
}

/// 创建全部可写目录。
pub fn ensure_dirs(layout: &Layout) -> Result<()> {
    for dir in [
        &layout.data,
        &layout.logs,
        &layout.config_dir(),
        &layout.uploads(),
        &layout.redis_data(),
    ] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("无法创建目录 {dir:?}"))?;
    }
    Ok(())
}

/// 判断是否首次运行（数据库尚未初始化）。
pub fn is_first_run(layout: &Layout) -> bool {
    // MySQL 的系统库目录存在即视为已初始化
    !layout.mysql_data().join("mysql").is_dir()
}
