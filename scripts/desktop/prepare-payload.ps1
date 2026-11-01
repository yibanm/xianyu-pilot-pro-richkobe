<#
.SYNOPSIS
  组装 Tauri 打包所需的 payload（runtime/ 与 app/ 两棵树）。

.DESCRIPTION
  下载并裁剪全部运行时与依赖，产出 apps/desktop/payload/ 供 tauri.conf.json
  的 bundle.resources 打包进产物。

  裁剪配方与体积数字均经本地实测验证（见项目 经验.md）：
    MySQL      268MB zip → 解压 1206MB → 裁剪 111MB
    PostgreSQL 308MB zip → 解压  869MB → 裁剪 124MB（删 pgAdmin 711MB）
    Redis       12MB zip → 必需文件仅 2MB
    JRE17       41MB zip → 解压 126MB
    Python      10MB zip → embeddable + 预装 site-packages
    Node        89MB 单文件 node.exe

.NOTES
  必须在 Windows 上运行（裁剪逻辑与目标平台一致）。
  幂等：重复执行会复用已下载的 zip 与已裁剪的目录。
#>

[CmdletBinding()]
param(
    # 仓库根目录（含 apps/）
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    # 下载缓存目录（CI 里可指向 actions/cache 的路径）
    [string]$CacheDir = (Join-Path $env:TEMP 'xypilot-dl'),
    # 输出目录：必须是 apps/desktop/payload（tauri.conf.json 的 bundle.resources 指向它）
    [string]$OutDir = (Join-Path $PSScriptRoot '..\..\apps\desktop\payload'),
    # 跳过前端/后端构建（仅拉运行时，用于本地调试）
    [switch]$RuntimeOnly
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest 的进度条在 CI 里极慢

# ---- 组件版本：与仓库 pin / compose 一致，禁止随手升级 ----
# Node 24.18.0：.node-version + dev-start.ps1 的 fail-closed 校验值
# Python 3.12.10：3.12.11+ 不再发布 embeddable 包（实测 404）
# MySQL 8.4.13 / PostgreSQL 16.10 / Redis 5.0.14.1 / JRE 17.0.20.1+1
$Components = [ordered]@{
    mysql = @{
        Url  = 'https://cdn.mysql.com/Downloads/MySQL-8.4/mysql-8.4.13-winx64.zip'
        File = 'mysql-8.4.13-winx64.zip'
    }
    postgres = @{
        Url  = 'https://get.enterprisedb.com/postgresql/postgresql-16.10-1-windows-x64-binaries.zip'
        File = 'postgresql-16.10-windows-x64.zip'
    }
    redis = @{
        Url  = 'https://github.com/tporadowski/redis/releases/download/v5.0.14.1/Redis-x64-5.0.14.1.zip'
        File = 'redis-5.0.14.1.zip'
    }
    jre = @{
        Url  = 'https://github.com/adoptium/temurin17-binaries/releases/download/jdk-17.0.20.1%2B1/OpenJDK17U-jre_x64_windows_hotspot_17.0.20.1_1.zip'
        File = 'jre-17.0.20.1.zip'
    }
    python = @{
        Url  = 'https://www.python.org/ftp/python/3.12.10/python-3.12.10-embed-amd64.zip'
        File = 'python-3.12.10-embed.zip'
    }
    node = @{
        Url  = 'https://nodejs.org/dist/v24.18.0/node-v24.18.0-win-x64.zip'
        File = 'node-v24.18.0-win-x64.zip'
    }
}

function Write-Step($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "  [OK] $msg" -ForegroundColor Green }
function Write-Warn2($msg) { Write-Host "  [!] $msg" -ForegroundColor Yellow }

function Get-SizeMB($path) {
    if (Test-Path $path) {
        $item = Get-Item $path -Force
        if ($item.PSIsContainer) {
            return [math]::Round(((Get-ChildItem $path -Recurse -Force -ErrorAction SilentlyContinue |
                Measure-Object -Property Length -Sum).Sum / 1MB), 1)
        }
        return [math]::Round($item.Length / 1MB, 1)
    }
    return 0
}

function Fetch-Component($name) {
    $spec = $Components[$name]
    $dest = Join-Path $CacheDir $spec.File
    if (Test-Path $dest) {
        Write-Ok "$name 已缓存: $($spec.File) ($(Get-SizeMB $dest) MB)"
        return $dest
    }
    Write-Host "  -> 下载 $name …" -ForegroundColor DarkGray
    # 重试：CDN 偶发截断（本地实测过 MySQL/Garnet 各截断一次）
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Invoke-WebRequest -Uri $spec.Url -OutFile $dest -UseBasicParsing -TimeoutSec 600
            # 校验 zip 完整性，防截断文件混进产物
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $null = [System.IO.Compression.ZipFile]::OpenRead($dest).Dispose()
            Write-Ok "$name 下载完成 ($(Get-SizeMB $dest) MB)"
            return $dest
        } catch {
            Write-Warn2 "$name 第 $attempt 次下载失败: $($_.Exception.Message)"
            Remove-Item $dest -Force -ErrorAction SilentlyContinue
            if ($attempt -eq 3) { throw "$name 下载失败（已重试 3 次）" }
            Start-Sleep -Seconds (5 * $attempt)
        }
    }
}

function Expand-Zip($zip, $destDir) {
    if (Test-Path $destDir) { return }
    New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    Expand-Archive -Path $zip -DestinationPath $destDir -Force
}

# ============================================================ 主流程

New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

$runtimeDir = Join-Path $OutDir 'runtime'
$appDir = Join-Path $OutDir 'app'
New-Item -ItemType Directory -Path $runtimeDir -Force | Out-Null
New-Item -ItemType Directory -Path $appDir -Force | Out-Null

# ---------------------------------------------------------- MySQL
Write-Step 'MySQL 8.4.13（裁剪）'
$zip = Fetch-Component 'mysql'
$tmp = Join-Path $CacheDir 'mysql-x'
Expand-Zip $zip $tmp
$src = Get-ChildItem $tmp -Directory | Where-Object Name -like 'mysql-*' | Select-Object -First 1
$dst = Join-Path $runtimeDir 'mysql'
if (-not (Test-Path $dst)) {
    Copy-Item $src.FullName $dst -Recurse -Force
    # 调试符号与静态库：mysqld.pdb 单个 577MB、mysqlclient.lib 76MB
    Get-ChildItem $dst -Recurse -Include '*.pdb', '*.lib' -Force | Remove-Item -Force
    # debug 版二进制
    Get-ChildItem $dst -Recurse -Include '*-debug.dll', '*-debug.exe' -Force | Remove-Item -Force
    # mecab 日语全文检索词典 122MB（中文项目不需要）
    Remove-Item (Join-Path $dst 'lib\mecab') -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $dst 'lib\plugin\debug') -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $dst 'include') -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item (Join-Path $dst 'docs') -Recurse -Force -ErrorAction SilentlyContinue
    # 客户端工具只留 4 个：mysqld/mysql/mysqladmin/mysqldump
    # （mysqladmin 用于 bootstrap 的优雅关闭，必须保留）
    $keepExe = @('mysqld.exe', 'mysql.exe', 'mysqladmin.exe', 'mysqldump.exe')
    Get-ChildItem (Join-Path $dst 'bin') -Filter '*.exe' |
        Where-Object { $keepExe -notcontains $_.Name } | Remove-Item -Force
    # 认证/复制插件：本项目单机单实例，用不到
    $pluginDir = Join-Path $dst 'lib\plugin'
    if (Test-Path $pluginDir) {
        $dropPlugins = @(
            'authentication_oci_client.dll', 'authentication_kerberos_client.dll',
            'authentication_ldap_sasl_client.dll', 'authentication_webauthn_client.dll',
            'group_replication.dll', 'mysql_clone.dll', 'rewriter.dll',
            'validate_password.dll', 'component_*.dll', 'ha_*.dll', 'semisync_*.dll'
        )
        foreach ($p in $dropPlugins) {
            Get-ChildItem $pluginDir -Filter $p -ErrorAction SilentlyContinue | Remove-Item -Force
        }
    }
    # lib/private/icudt77l 是 ICU 数据（regexp/collation 依赖），不可删
}
Write-Ok "MySQL: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- PostgreSQL
Write-Step 'PostgreSQL 16.10（裁剪）'
$zip = Fetch-Component 'postgres'
$tmp = Join-Path $CacheDir 'pg-x'
Expand-Zip $zip $tmp
$src = Join-Path $tmp 'pgsql'
$dst = Join-Path $runtimeDir 'postgres'
if (-not (Test-Path $dst)) {
    New-Item -ItemType Directory -Path $dst -Force | Out-Null
    # 只复制运行时必需的三个目录：pgAdmin 4 单独就占 711MB
    foreach ($d in 'bin', 'lib', 'share') {
        Copy-Item (Join-Path $src $d) $dst -Recurse -Force
    }
}
Write-Ok "PostgreSQL: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- Redis
Write-Step 'Redis 5.0.14.1（精简）'
$zip = Fetch-Component 'redis'
$tmp = Join-Path $CacheDir 'redis-x'
Expand-Zip $zip $tmp
$dst = Join-Path $runtimeDir 'redis'
if (-not (Test-Path $dst)) {
    New-Item -ItemType Directory -Path $dst -Force | Out-Null
    # 全量 47MB（含 pdb/文档）→ 只留运行必需的 exe
    foreach ($f in 'redis-server.exe', 'redis-cli.exe', 'EventLog.dll') {
        $p = Join-Path $tmp $f
        if (Test-Path $p) { Copy-Item $p $dst -Force }
    }
}
Write-Ok "Redis: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- JRE
Write-Step 'JRE 17（Temurin）'
$zip = Fetch-Component 'jre'
$tmp = Join-Path $CacheDir 'jre-x'
Expand-Zip $zip $tmp
$src = Get-ChildItem $tmp -Directory | Select-Object -First 1
$dst = Join-Path $runtimeDir 'jre'
if (-not (Test-Path $dst)) { Copy-Item $src.FullName $dst -Recurse -Force }
Write-Ok "JRE: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- Node
Write-Step 'Node.js 24.18.0（只要 node.exe）'
$zip = Fetch-Component 'node'
$tmp = Join-Path $CacheDir 'node-x'
Expand-Zip $zip $tmp
$src = Get-ChildItem $tmp -Directory | Where-Object Name -like 'node-*' | Select-Object -First 1
$dst = Join-Path $runtimeDir 'node'
if (-not (Test-Path $dst)) {
    New-Item -ItemType Directory -Path $dst -Force | Out-Null
    # crawler-service 只用 node 跑编译后的 dist/*.js，不需要 npm/npx/corepack
    Copy-Item (Join-Path $src.FullName 'node.exe') $dst -Force
}
Write-Ok "Node: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- Python
Write-Step 'Python 3.12.10 embeddable + 依赖'
$zip = Fetch-Component 'python'
$tmp = Join-Path $CacheDir 'py-x'
Expand-Zip $zip $tmp
$dst = Join-Path $runtimeDir 'python'
if (-not (Test-Path $dst)) {
    Copy-Item $tmp $dst -Recurse -Force
    # 关键：取消 ._pth 里 import site 的注释，否则 site-packages 不进 sys.path
    $pth = Get-ChildItem $dst -Filter 'python*._pth' | Select-Object -First 1
    if ($pth) {
        (Get-Content $pth.FullName -Raw) -replace '#\s*import site', 'import site' |
            Set-Content $pth.FullName -NoNewline -Encoding ASCII
        Write-Ok "已启用 $($pth.Name) 的 import site"
    } else {
        throw "未找到 python*._pth，无法启用 site-packages"
    }
}
# 装依赖：用 CI 上的 python 3.12 以 --target 方式装入便携目录
# （--only-binary + 平台标签：全部依赖都有 cp312-win_amd64 预编译 wheel，无需编译）
$spDir = Join-Path $dst 'Lib\site-packages'
$reqFile = Join-Path $RepoRoot 'apps\automation-service\requirements.txt'
Write-Host "  -> 安装 automation-service 依赖到 site-packages …" -ForegroundColor DarkGray
New-Item -ItemType Directory -Path $spDir -Force | Out-Null
& python -m pip install --quiet --disable-pip-version-check `
    --only-binary=:all: --platform win_amd64 --python-version 312 `
    --implementation cp --abi cp312 `
    --target $spDir -r $reqFile
if ($LASTEXITCODE -ne 0) { throw "pip install 失败（退出码 $LASTEXITCODE）" }
# patchright：商机搜索的反检测兜底（Docker 里由 Dockerfile 装，桌面版在此装）
& python -m pip install --quiet --disable-pip-version-check `
    --only-binary=:all: --platform win_amd64 --python-version 312 `
    --implementation cp --abi cp312 `
    --target $spDir patchright
if ($LASTEXITCODE -ne 0) { Write-Warn2 "patchright 安装失败（非致命，商机搜索兜底不可用）" }
Write-Ok "Python: $(Get-SizeMB $dst) MB"

# ---------------------------------------------------------- VC++ 运行库
Write-Step 'VC++ 运行库（三个数据库 exe 都依赖）'
$dst = Join-Path $runtimeDir 'vcruntime'
if (-not (Test-Path $dst)) {
    New-Item -ItemType Directory -Path $dst -Force | Out-Null
    $sys = Join-Path $env:SystemRoot 'System32'
    # 直接取 runner 上的运行库（GitHub windows runner 已装 VC++ 2015-2022）
    foreach ($dll in 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll',
                     'concrt140.dll', 'ucrtbase.dll', 'api-ms-win-crt-*.dll') {
        Get-ChildItem $sys -Filter $dll -ErrorAction SilentlyContinue |
            Copy-Item -Destination $dst -Force -ErrorAction SilentlyContinue
    }
    $count = (Get-ChildItem $dst -Filter '*.dll').Count
    if ($count -lt 3) {
        Write-Warn2 "只复制到 $count 个 DLL，VC 运行库可能不全（runner 未装？）"
    } else {
        Write-Ok "VC 运行库: $count 个 DLL"
    }
}

if ($RuntimeOnly) {
    Write-Step '仅运行时模式，跳过应用产物'
    return
}

# ============================================================ 应用产物
# 以下由各构建步骤产出后拷入；脚本只负责搬运，构建本身在 workflow 里做
Write-Step '收集应用产物'

$coreApiJar = Get-ChildItem (Join-Path $RepoRoot 'apps\core-api\target') -Filter '*.jar' -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -notlike '*sources*' -and $_.Name -notlike '*original*' } |
    Select-Object -First 1
if ($coreApiJar) {
    $d = Join-Path $appDir 'core-api'
    New-Item -ItemType Directory -Path $d -Force | Out-Null
    Copy-Item $coreApiJar.FullName $d -Force
    Write-Ok "core-api: $($coreApiJar.Name) ($([math]::Round($coreApiJar.Length/1MB,1)) MB)"
} else {
    throw '未找到 core-api jar，请先执行 mvnw package'
}

# automation-service：Python 源码（依赖已装进 runtime/python）
$autoSrc = Join-Path $RepoRoot 'apps\automation-service'
$autoDst = Join-Path $appDir 'automation'
if (Test-Path $autoDst) { Remove-Item $autoDst -Recurse -Force }
New-Item -ItemType Directory -Path $autoDst -Force | Out-Null
foreach ($item in 'app', 'run-fast.py', 'run-worker.py', 'requirements.txt') {
    $p = Join-Path $autoSrc $item
    if (Test-Path $p) { Copy-Item $p $autoDst -Recurse -Force }
}
Write-Ok "automation: $(Get-SizeMB $autoDst) MB"

# crawler-service：编译产物 + 生产依赖（不含 devDependencies）
$crawlerSrc = Join-Path $RepoRoot 'apps\crawler-service'
$crawlerDst = Join-Path $appDir 'crawler'
if (Test-Path $crawlerDst) { Remove-Item $crawlerDst -Recurse -Force }
New-Item -ItemType Directory -Path $crawlerDst -Force | Out-Null
foreach ($item in 'dist', 'node_modules', 'package.json', 'migrations', 'cloakbrowser_solve.py') {
    $p = Join-Path $crawlerSrc $item
    if (Test-Path $p) { Copy-Item $p $crawlerDst -Recurse -Force }
}
Write-Ok "crawler: $(Get-SizeMB $crawlerDst) MB"

# 前端：Vite dist（由网关托管，不需 nginx）
foreach ($web in @(@{n='user';p='apps\user-web'}, @{n='admin';p='apps\admin-web'})) {
    $srcDist = Join-Path $RepoRoot (Join-Path $web.p 'dist')
    $dstWeb = Join-Path $appDir "web\$($web.n)"
    if (Test-Path $srcDist) {
        if (Test-Path $dstWeb) { Remove-Item $dstWeb -Recurse -Force }
        New-Item -ItemType Directory -Path $dstWeb -Force | Out-Null
        Copy-Item (Join-Path $srcDist '*') $dstWeb -Recurse -Force
        Write-Ok "$($web.n)-web: $(Get-SizeMB $dstWeb) MB"
    } else {
        throw "未找到 $($web.p)\dist，请先执行 npm run build"
    }
}

# ============================================================ 汇总
Write-Step '体积汇总'
$total = 0
foreach ($d in (Get-ChildItem $runtimeDir -Directory) + (Get-ChildItem $appDir -Directory)) {
    $mb = Get-SizeMB $d.FullName
    $total += $mb
    Write-Host ("  {0,-22} {1,8} MB" -f $d.Name, $mb)
}
$webDir = Join-Path $appDir 'web'
if (Test-Path $webDir) {
    foreach ($d in Get-ChildItem $webDir -Directory) {
        $mb = Get-SizeMB $d.FullName
        $total += $mb
        Write-Host ("  {0,-22} {1,8} MB" -f "web/$($d.Name)", $mb)
    }
}
Write-Host ("  {0,-22} {1,8} MB" -f '合计（解压后）', [math]::Round($total,1)) -ForegroundColor Green
Write-Host "`n  payload 目录: $OutDir"
