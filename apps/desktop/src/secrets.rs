//! 密钥生成与持久化。
//!
//! compose 要求 INTERNAL_API_TOKEN / COOKIE_CRYPTO_SECRET / ADMIN_JWT_SECRET 必须显式设置
//! （`${VAR:?must be set}`，长度建议 ≥32 字符）。桌面版在首次运行时随机生成并落盘，
//! 之后每次启动复用——密钥一旦变化，已加密存储的闲鱼 Cookie 将无法解密、JWT 全部失效。

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// 全部机密项。序列化后落盘 data/config/secrets.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secrets {
    /// 服务间调用的内部令牌（compose: INTERNAL_API_TOKEN）
    pub internal_api_token: String,
    /// 闲鱼 Cookie 的加密密钥（compose: COOKIE_CRYPTO_SECRET）
    pub cookie_crypto_secret: String,
    /// 管理端 JWT 签名密钥（compose: ADMIN_JWT_SECRET）
    pub admin_jwt_secret: String,
    /// MySQL root 密码
    pub mysql_root_password: String,
    /// MySQL 应用账号密码（compose: MYSQL_APP_PASSWORD）
    pub mysql_app_password: String,
    /// Redis 密码
    pub redis_password: String,
    /// PostgreSQL 爬虫库密码（compose: CRAWLER_DB_PASSWORD）
    pub crawler_db_password: String,
    /// 数据库账号名（非机密，但一并存以便一致性）
    pub mysql_app_user: String,
    pub crawler_db_user: String,
    pub crawler_db: String,
    pub mysql_database: String,
}

impl Secrets {
    /// 生成全新密钥集。全部使用 32 字节随机数的 hex（64 字符），满足 ≥32 字符建议。
    pub fn generate() -> Self {
        Self {
            internal_api_token: random_hex(32),
            cookie_crypto_secret: random_hex(32),
            admin_jwt_secret: random_hex(32),
            mysql_root_password: random_hex(16),
            mysql_app_password: random_hex(16),
            redis_password: random_hex(16),
            crawler_db_password: random_hex(16),
            mysql_app_user: "xianyu_app".to_string(),
            crawler_db_user: "crawler".to_string(),
            crawler_db: "xianyu_crawler".to_string(),
            mysql_database: "xianyu_assistant_admin".to_string(),
        }
    }

    /// 读取已有密钥；文件不存在则生成并写入。
    ///
    /// 幂等：重复调用不会改变已存在的密钥。
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("读取密钥文件失败 {path:?}"))?;
            let secrets: Secrets = serde_json::from_str(&raw)
                .with_context(|| format!("密钥文件格式损坏 {path:?}"))?;
            secrets.validate()?;
            // 旧版本可能把 ACL 锁成「仅当前用户」，导致管理员也删不掉所在目录树；
            // restrict 幂等（/grant:r 与 /inheritance:r 重复执行无副作用），每次启动顺带自愈。
            #[cfg(windows)]
            restrict_to_current_user(path)?;
            return Ok(secrets);
        }

        let secrets = Secrets::generate();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("无法创建目录 {parent:?}"))?;
        }
        let json = serde_json::to_string_pretty(&secrets)
            .context("序列化密钥失败")?;
        fs::write(path, json).with_context(|| format!("写入密钥文件失败 {path:?}"))?;

        // Windows 下收紧权限：仅当前用户与系统管理身份可读写，其他标准用户不可见
        #[cfg(windows)]
        restrict_to_current_user(path)?;

        Ok(secrets)
    }

    /// 校验字段非空且长度足够。compose 建议 ≥32 字符。
    fn validate(&self) -> Result<()> {
        let required = [
            ("internal_api_token", &self.internal_api_token),
            ("cookie_crypto_secret", &self.cookie_crypto_secret),
            ("admin_jwt_secret", &self.admin_jwt_secret),
            ("mysql_root_password", &self.mysql_root_password),
            ("mysql_app_password", &self.mysql_app_password),
            ("redis_password", &self.redis_password),
            ("crawler_db_password", &self.crawler_db_password),
        ];
        for (name, value) in required {
            if value.is_empty() {
                bail!("密钥 {name} 为空，secrets.json 可能已损坏");
            }
            if value.len() < 32 && !name.contains("password") {
                bail!(
                    "密钥 {name} 长度 {} 不足 32 字符（compose 的建议下限）",
                    value.len()
                );
            }
        }
        Ok(())
    }
}

/// 生成 n 字节随机数的 hex 表示（长度 2n）。
fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Windows ACL：移除继承，只保留当前用户的完全控制。
///
/// 密钥文件泄露等于闲鱼 Cookie 明文泄露，不能依赖目录默认权限。
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;
    let p = path.to_string_lossy().to_string();

    // icacls 是 Windows 自带工具，无需额外依赖。
    // 必须同时保留 Administrators 与 SYSTEM：/inheritance:r 会清掉全部继承 ACE，
    // 若只授当前用户，提权后的管理员对该文件也没有任何 ACE——
    // 实测后果：用户删除整个便携版目录树时被这一个文件卡死，
    // Explorer 报「需要来自 <用户> 的权限」，所有者身份也救不了（删除流程不会先改 ACL）。
    let status = Command::new("icacls")
        .args([
            &p,
            "/inheritance:r", // 移除继承
            "/grant:r",
            &format!("{}:(F)", current_user_name()?),
            "Administrators:(F)",
            "SYSTEM:(F)",
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status()
        .context("调用 icacls 失败")?;

    if !status.success() {
        // 权限收紧失败不致命（文件仍在用户目录下），但必须留痕
        eprintln!("[secrets] icacls 收紧权限失败（退出码 {status}），沿用目录默认权限");
    }
    Ok(())
}

#[cfg(windows)]
fn current_user_name() -> Result<String> {
    std::env::var("USERNAME")
        .map(|u| format!("{u}"))
        .context("无法取得当前用户名（USERNAME 未设置）")
}
