import fs from 'node:fs';

/**
 * 系统浏览器 channel 解析。
 *
 * 背景：仓库原先在 Windows/Linux 上硬编码 `channel: 'chrome'`，要求机器装有
 * Google Chrome。未装 Chrome 的 Windows 机器（例如只有系统自带 Edge）会直接
 * 抛 `browserType.launch: Chromium distribution 'chrome' is not found at ...`，
 * 导致店铺爬取、商机搜索、扫码登录全部不可用。
 *
 * Windows 便携打包不内置 Playwright Chromium（约 160MB），因此必须能回退到
 * 系统已装的浏览器。优先级：环境变量显式指定 > Chrome > Edge > 内置 Chromium。
 *
 * 反检测说明：sliderSolver.ts 的注释指出「真实 Chrome 而非 Chromium」能降低被
 * Baxia 识别为自动化的概率，故 Chrome 优先于 Edge；两者都是真实发行版浏览器，
 * 均优于内置 Chromium。
 */
export type BrowserChannel = 'chrome' | 'msedge';

/** launch/launchPersistentContext 可直接展开的选项；无可用系统浏览器时为空对象（回退内置 Chromium）。 */
export type SystemBrowserLaunchOptions = { channel?: BrowserChannel };

const CHANNEL_ENV = 'SLIDER_BROWSER_CHANNEL';

// Windows 上 Chrome 与 Edge 的常见安装位置。Edge 随系统预装，故必有其一可用。
const CANDIDATES: Array<{ channel: BrowserChannel; paths: string[] }> = [
  {
    channel: 'chrome',
    paths: [
      'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
      'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
      `${process.env.LOCALAPPDATA ?? ''}\\Google\\Chrome\\Application\\chrome.exe`,
    ],
  },
  {
    channel: 'msedge',
    paths: [
      'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
      'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
    ],
  },
];

let cached: SystemBrowserLaunchOptions | null = null;

function exists(path: string): boolean {
  if (!path || path.startsWith('\\')) return false; // LOCALAPPDATA 为空时会拼出 '\\Google\...'
  try {
    return fs.existsSync(path);
  } catch {
    return false;
  }
}

/**
 * 解析当前平台应使用的浏览器启动选项。
 *
 * 结果按进程缓存：浏览器安装状态在进程生命周期内不会变化，避免每次 launch
 * 都做文件系统探测。
 */
export function resolveSystemBrowserLaunch(): SystemBrowserLaunchOptions {
  if (cached) return cached;

  const forced = process.env[CHANNEL_ENV]?.trim();
  if (forced === 'chrome' || forced === 'msedge') {
    cached = { channel: forced };
    console.log(`[BrowserChannel] 使用 ${CHANNEL_ENV} 指定的 channel=${forced}`);
    return cached;
  }
  if (forced) {
    // 非法值不静默忽略：回退自动探测，但留下线索便于排查配置错误。
    console.warn(`[BrowserChannel] ${CHANNEL_ENV}='${forced}' 非法（应为 chrome 或 msedge），改为自动探测`);
  }

  if (process.platform === 'win32') {
    for (const candidate of CANDIDATES) {
      const hit = candidate.paths.find(exists);
      if (hit) {
        cached = { channel: candidate.channel };
        console.log(`[BrowserChannel] 探测到系统浏览器 channel=${candidate.channel}: ${hit}`);
        return cached;
      }
    }
    // 两者都没有：回退内置 Chromium（需要已执行 playwright install）。
    console.warn('[BrowserChannel] 未探测到系统 Chrome/Edge，回退 Playwright 内置 Chromium');
    cached = {};
    return cached;
  }

  if (process.platform === 'darwin') {
    // 保持原行为：macOS 不指定 channel，使用内置 Chromium。
    cached = {};
    return cached;
  }

  // Linux：Docker 镜像已安装 google-chrome-stable（见 crawler-service/Dockerfile），
  // 保持原有 channel:'chrome' 行为不变。
  cached = { channel: 'chrome' };
  return cached;
}

/** 仅供测试重置缓存。 */
export function resetSystemBrowserLaunchCache(): void {
  cached = null;
}
