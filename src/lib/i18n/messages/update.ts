// update 命名空间:每个键 = [英文, 简体中文]。术语。
// 覆盖：设置·About 页 + updateService（错误文案 / 系统通知）。
export default {
  // About 页
  sectionUpdates: ['Updates', '更新'],
  version: ['Version', '版本'],
  versionHint: ['Current app version', '当前应用版本'],
  openReleasesHint: ['Open the releases page (manual download)', '打开 Releases 页面（手动下载）'],
  checkForUpdates: ['Check for updates', '检查更新'],
  checkForUpdatesHint: ['Compare with the latest release', '与最新发布版本比较'],
  install: ['Install', '安装'],
  downloadInstall: ['Download & install', '下载并安装'],
  installDownloadedHint: ['Run the installer for the downloaded, signature-verified update', '运行已下载并通过签名校验的更新安装器'],
  downloadInstallHint: ['Download, verify the signature and run the installer', '下载、校验签名并运行安装器'],
  checkAtStartup: ['Check for updates at startup', '启动时检查更新'],
  checkAtStartupHint: [
    'At every launch, check for a new version and download it in the background, then show a notification. Nothing is installed until you click Install.',
    '每次启动时检查新版本并在后台下载，完成后发送通知。在你点击「安装」之前不会安装任何内容。',
  ],

  // 状态行
  installerLaunched: [
    'Installer launched — the app closes while the update is applied. Reopen it afterwards.',
    '安装器已启动——更新期间应用会关闭，完成后请重新打开。',
  ],
  installFailed: ['Install failed: {error}', '安装失败：{error}'],
  checking: ['Checking for updates…', '正在检查更新…'],
  downloading: ['Downloading update…', '正在下载更新…'],
  downloadingPct: ['Downloading update… {pct}%', '正在下载更新… {pct}%'],
  downloadComplete: ['Download complete — starting the installer…', '下载完成——正在启动安装器…'],
  readyToInstall: ['Version {version} has been downloaded and is ready to install.', '版本 {version} 已下载，可以安装。'],
  readyToInstallCurrent: [
    'Version {version} has been downloaded and is ready to install — current v{current}.',
    '版本 {version} 已下载，可以安装——当前 v{current}。',
  ],
  servedFrom: ['Updates are served from the public repository releases page.', '更新来自公开仓库的 Releases 页面。'],
  unavailable: ['Update check is only available in the desktop app.', '仅桌面应用可检查更新。'],
  upToDate: ["You're up to date — current v{current}.", '已是最新版本——当前 v{current}。'],
  available: ['Version {version} is available — current v{current}.', '有新版本 {version}——当前 v{current}。'],
  availableDev: [
    'Version {version} is available — current v{current}. Dev build: install is disabled here, download it from the releases page.',
    '有新版本 {version}——当前 v{current}。开发构建无法在此安装，请从 Releases 页面下载。',
  ],

  // updateService 错误文案
  errNoRelease: ['No published release found.', '未找到已发布的版本。'],
  errNetwork: ['Network error while checking for updates.', '检查更新时网络出错。'],
  errSignature: [
    'The downloaded package failed signature verification. Download the installer from the releases page instead.',
    '下载的安装包未通过签名校验，请改从 Releases 页面下载安装器。',
  ],
  errNoReady: ['No downloaded update to install.', '没有可安装的已下载更新。'],
  errDevInstall: ['Dev build: install is disabled.', '开发构建：已禁用安装。'],

  // 系统通知（新版已预下载）
  notifyTitle: ['TokenCalendar {version} is ready', 'TokenCalendar {version} 已就绪'],
  notifyBody: [
    'The update has been downloaded. Open Settings → About and click Install to update.',
    '更新已下载。打开「设置 → 关于」，点击「安装」即可更新。',
  ],
} as const satisfies Record<string, readonly [string, string]>
