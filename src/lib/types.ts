// 与 Rust 后端命令返回结构对齐的类型定义（对照 server.py 各 API 响应）

/** 账号区域键；与后端 `Region::key()` 对齐。 */
export type AccountRegionKey = "cn" | "intl";

export interface AccountMeta {
  id: string;
  /** 服务区域展示名（"国服" / "国际版"），由 domain 后缀推导。 */
  region?: string;
  /** 区域键（"cn" / "intl"），便于样式与筛选。 */
  regionKey?: AccountRegionKey;
  uid: string | null;
  email: string | null;
  nickname: string | null;
  enterpriseName: string | null;
  expiresAt: number | null;
  refreshExpiresAt: number | null;
  refreshedAt: number | null;
  createdAt: number | null;
  needsRelogin: boolean;
  needsReloginReason: string | null;
  /**
   * 用户自定义备注（如「公司号」「备用」）。
   *
   * 为什么需要：授权进来的账号往往只带邮箱/手机号/随机 uid，光看这些认不出
   * 「这是谁的号、干什么用的」。备注只存本地，不参与登录。
   */
  note?: string | null;
  /** 原始域名（如 www.workbuddy.ai / copilot.tencent.com）—— 排查时比区域标签更具体。 */
  domain?: string | null;
  /** 手机号（国服账号的真实身份线索；其 email 常为空）。 */
  phoneNumber?: string | null;
  /** 账号类型（personal / enterprise）—— 影响可用模型与额度口径。 */
  accountType?: string | null;
}

export interface AppStatus {
  running: boolean;
  authFile: string;
  current: {
    uid: string | null;
    nickname: string | null;
    email: string | null;
  } | null;
  appPath: string;
  version: string;
}

export interface OAuthStartResult {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
  /** 本次登录会话所属区域；由后端回显，缺省视为国服。 */
  region?: AccountRegionKey;
}

export interface OAuthPollResult {
  done: boolean;
  result?: AccountMeta;
  error?: string;
}

/** 导出文件中的完整账号记录（含 token，仅导出命令返回；字段与账号库原始记录一致）。 */
export interface AccountRecord {
  id?: string;
  uid?: string | null;
  nickname?: string | null;
  email?: string | null;
  access_token?: string | null;
  refresh_token?: string | null;
  token_type?: string | null;
  domain?: string | null;
  expiresAt?: number | null;
  refreshExpiresAt?: number | null;
  auth_raw?: unknown;
  profile_raw?: unknown;
  createdAt?: number | null;
  [key: string]: unknown;
}

/** 导入文件账号的脱敏预览（不含 token）。 */
export interface ImportPreviewAccount {
  index: number;
  uid: string | null;
  nickname: string | null;
  email: string | null;
  hasToken: boolean;
}

/** 导入结果计数。 */
export interface ImportResult {
  ok: boolean;
  imported: number;
  skipped: number;
  overwritten: number;
}

/** 本机候选账号的来源类型。 */
export type LocalAccountSource = "current" | "snapshot" | "backup";

/** 本机候选账号的凭证可用性。 */
export type LocalAccountFreshness = "refreshable" | "access_only" | "expired";

/** 本机扫描发现的单个候选账号（不含 token）。 */
export interface LocalImportCandidate {
  /** 本次扫描结果中的序号。 */
  index: number;
  /** 来源文件绝对路径；导入时以此为准（跨扫描稳定）。 */
  path: string;
  /** 账号元数据（脱敏）。 */
  meta: AccountMeta;
  source: LocalAccountSource;
  /** 来源展示名：当前登录 / 历史快照 / 切换备份。 */
  sourceLabel: string;
  freshness: LocalAccountFreshness;
  /** 凭证可用性展示名。 */
  freshnessLabel: string;
  /** 同一账号在本机共有多少份文件（>1 表示还有更旧的重复快照）。 */
  duplicateCount: number;
  /** 是否已在账号库中。 */
  alreadyImported: boolean;
  /** 库中已有该账号，但本机这份凭证更新：导入会覆盖刷新。 */
  updatesStored: boolean;
  /** 来源文件最后修改时间（毫秒）。 */
  modifiedAt: number;
}

/** GET /api/import-local/scan 响应。 */
export interface LocalScanResult {
  ok: boolean;
  candidates: LocalImportCandidate[];
  total: number;
  /** 识别出的认证文件总数（含被去重掉的旧快照）。 */
  filesScanned: number;
  /** 可导入（凭证未完全过期）的候选数。 */
  usable: number;
  /** 认证文件目录。 */
  authDir: string;
  /** 本工具备份目录。 */
  backupDir: string;
}

/** POST /api/import-local/selected 响应。 */
export interface LocalImportResult {
  ok: boolean;
  imported: number;
  /** 新增账号数。 */
  added: number;
  /** 覆盖刷新既有账号数。 */
  updated: number;
  /** 逐个账号的结果明细。 */
  outcomes: Array<{
    name: string;
    region: string;
    source: LocalAccountSource;
    file: string;
    freshness: LocalAccountFreshness;
    updated: boolean;
  }>;
}

export interface Session {
  id: string;
  title: string;
  cwd: string;
  updatedAt: number;
  hasHistory: boolean;
  /** WorkBuddy playground（侧栏「任务」）；缺省视为空间会话。 */
  isPlayground?: boolean;
}

export interface CopyResult {
  id: string;
  newId: string;
  jsonlCopied: boolean;
  mappingWritten: boolean;
  backup: string;
}

export interface SwitchResult {
  ok: boolean;
  account: string;
  backup: string | null;
  sessionCopy?: {
    sourceUid: string;
    targetUid: string;
    copied: CopyResult[];
    errors?: { id: string; error: string }[];
  };
}

export interface CheckinConfig {
  enabled: boolean;
  /** Legacy persisted fields; accepted by the backend but ignored by scheduling. */
  start_hour?: number;
  end_hour?: number;
  keepalive_days: number;
  lazy_refresh_hours: number;
  /**
   * 历史字段：旧版本曾用 `"cn" | "all"` 控制覆盖区域。
   * 自动签到 / 自动旅行现已硬绑定为「仅国服」，后端忽略此字段，
   * 前端不再读取或写入，仅作为兼容旧配置文件保留类型定义。
   */
  region_scope?: "cn" | "all";
}

export interface CheckinLog {
  ts: number;
  accountId: string | null;
  email: string;
  result: string;
  error?: string;
}

export interface CheckinResult {
  result: string;
  error?: string;
}

export interface TravelConfig {
  enabled: boolean;
  /**
   * 历史字段：旧版本曾用 `"cn" | "all"` 控制覆盖区域。
   * 自动旅行现已硬绑定为「仅国服」，后端忽略此字段，
   * 前端不再读取或写入，仅作为兼容旧配置文件保留类型定义。
   */
  region_scope?: "cn" | "all";
}

export type TravelStatusLabel = "untraveled" | "no-buddy" | "traveling" | "finished" | "adopted" | "adopt-threshold";

export interface TravelStatus {
  label: TravelStatusLabel;
  rewardCredit: number | null;
  locationName?: string | null;
  arriveAt?: number | null;
  /** 后端给出的具体说明（如「领养需先积累对话轮次」），供卡片直接展示原因。 */
  message?: string | null;
  /** 跳过/结果原因，用于区分细分状态（adopt-threshold / no-buddy / daily-limit 等）。 */
  skip?: string | null;
}

export interface AutoRotateConfig {
  enabled: boolean;
  check_interval_minutes: number;
  cooldown_minutes: number;
  min_gap_hours: number;
  min_urgency_hours: number;
  active_guard_minutes: number;
  min_remaining_credits: number;
}

export interface RotateLog {
  ts: number;
  action: string;
  reason?: string | null;
  from?: { id: string; name?: string | null } | null;
  to?: { id: string; name?: string | null } | null;
}

export interface RotateStatus {
  config: AutoRotateConfig;
  cliConfigured: boolean;
  activeAccountId: string | null;
  activeAccountName: string | null;
  lastCheckAt: number | null;
  lastSwitchAt: number | null;
}

export interface CreditResource {
  packageCode: string | null;
  packageName: string | null;
  total: number;
  remaining: number;
  used: number;
  status: number | null;
  expireAt: number | null;
  expired: boolean;
  expiringSoon: boolean;
}

export interface CreditExpiry {
  ok: boolean;
  accountId?: string | null;
  accountName?: string;
  updatedAt?: number;
  totalCapacity?: number;
  totalRemaining?: number;
  expiringSoonRemaining?: number;
  expiredRemaining?: number;
  soonestExpireAt?: number | null;
  expiringSoon?: boolean;
  expired?: boolean;
  resources?: CreditResource[];
  error?: string;
}

export interface CreditStatsSummary {
  currentRemaining: number;
  currentCapacity: number;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  todayCheckedInAccounts: number;
  todaySuccess: number;
  todayAlready: number;
  todayFailed: number;
}

export interface CreditStatsDailyPoint {
  date: string;
  usage: number;
  /** 官方用量按模型聚合（全量，不受请求明细条数限制）；本地观察口径下为空 */
  models?: { model: string; requestCount: number; credit: number }[];
}

export interface CreditStatsAccount {
  accountId: string;
  accountName: string;
  isCurrent: boolean;
  currentRemaining: number | null;
  totalCapacity: number | null;
  lastSnapshotAt: number | null;
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
  checkedInToday: boolean | null;
  checkinStatusToday: string | null;
  lastCheckinAt: number | null;
  lastCheckinResult: string | null;
  /** 按账号的逐日观察消耗（缺省兼容旧后端）；官方可用时趋势图优先使用官方 daily */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditStatsUsageEvent {
  kind: "usage";
  ts: number;
  date: string;
  accountId: string;
  accountName: string;
  amount: number;
}

export interface CreditStatsCheckinEvent {
  kind: "checkin";
  ts: number;
  date: string;
  accountId: string | null;
  accountName: string;
  result: string;
  error?: string | null;
}

export type CreditStatsEvent = CreditStatsUsageEvent | CreditStatsCheckinEvent;

export type CreditOfficialUsageStatus = "complete" | "partial" | "unavailable";

export interface CreditOfficialUsageSummary {
  usageToday: number;
  usage7Days: number;
  usageThisMonth: number;
}

export interface CreditOfficialUsageModel {
  model: string;
  requestCount: number;
  credit: number;
}

export interface CreditOfficialUsageAccount {
  accountId: string;
  accountName: string;
  ok: boolean;
  requestCount: number;
  detailTruncated: boolean;
  usageToday: number | null;
  usage7Days: number | null;
  usageThisMonth: number | null;
  error?: string | null;
  reportedTotal?: number | null;
  fetchedCount?: number;
  /** 缺省兼容旧后端响应。 */
  models?: CreditOfficialUsageModel[];
  /** 按账号的逐日官方消耗（全量聚合，不受 requests 明细上限影响；缺省兼容旧后端） */
  daily?: CreditStatsDailyPoint[];
}

export interface CreditOfficialUsageRequest {
  accountId: string;
  accountName: string;
  requestId: string;
  credit: number;
  model: string;
  client: string;
  requestTime: string;
}

export interface CreditOfficialUsageError {
  accountId: string;
  accountName: string;
  error: string;
}

export interface CreditOfficialUsage {
  status: CreditOfficialUsageStatus;
  rangeStart: string;
  rangeEnd: string;
  /** 官方用量最近一次采集时间；缓存命中时保持采集当时的时间。 */
  collectedAt?: number;
  summary: CreditOfficialUsageSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditOfficialUsageAccount[];
  requests: CreditOfficialUsageRequest[];
  /** 官方全部有效请求按模型汇总；不受 requests 明细上限影响。 */
  models?: CreditOfficialUsageModel[];
  detailLimitPerAccount: number;
  errors: CreditOfficialUsageError[];
}

export interface CreditStatistics {
  generatedAt: number;
  retentionDays: number;
  coverageStartAt: number | null;
  summary: CreditStatsSummary;
  daily: CreditStatsDailyPoint[];
  accounts: CreditStatsAccount[];
  events: CreditStatsEvent[];
  /** 官方接口不可用时仍使用上述本地观察字段；缺省兼容旧后端。 */
  officialUsage?: CreditOfficialUsage;
}

export interface TokenStatsTotals { total: number; input: number; output: number; cacheRead: number; cacheWrite: number; uncachedInput: number; records: number; cacheHitRate: number | null; }
export interface TokenStatsGroup extends TokenStatsTotals { key: string; title?: string | null; project?: string; sessionId?: string; }
export interface TokenStatsSource { source: "workbuddy" | "workbuddy-ai" | "codebuddy-cli" | "codebuddy-ide"; summary: TokenStatsTotals; models: TokenStatsGroup[]; projects: TokenStatsGroup[]; sessions: TokenStatsGroup[]; daily: TokenStatsGroup[]; /** Optional model-specific daily series for trend filtering. */ dailyByModel?: Record<string, TokenStatsGroup[]>; hours: TokenStatsGroup[]; filesScanned: number; parseErrors: number; coverageStartAt?: number | null; coverageEndAt?: number | null; }
export interface TokenStatistics { generatedAt: number; rangeDays?: number | null; sources: TokenStatsSource[]; }

export interface CodeBuddyCliStatus {
  configured: boolean;
  authMode?: "settings-env";
  environmentOverride?: boolean;
  settingsPresent: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  helperCurrent?: boolean;
  migrationRequired?: boolean;
  syncPending?: boolean;
  activeIndex: number | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  accountCount: number;
  statePath: string;
}

export interface CodeBuddyCliSwitchResult {
  ok: boolean;
  configured: boolean;
  synced: boolean;
  verified?: boolean;
  authMode?: "settings-env";
  activeIndex?: number;
  activeAccountId?: string;
  source?: string;
  skipped?: boolean;
  message?: string;
  error?: string;
}

export interface CodeBuddyCliInstallResult {
  ok: boolean;
  configured: boolean;
  helperPresent: boolean;
  helperSupportsAccountIds: boolean;
  verified?: boolean;
  authMode?: "settings-env";
  message?: string;
  error?: string;
}

export interface GithubConfig {
  owner?: string;
  repo?: string;
  proxy?: string;
}

export interface UpdateInfo {
  ok: boolean;
  current?: string;
  latest?: string;
  latestTag?: string;
  hasUpdate?: boolean;
  releaseName?: string;
  releaseUrl?: string;
  publishedAt?: string;
  error?: string;
  message?: string;
}

/** CodeBuddy CN IDE（桌面客户端）状态；与 CodeBuddy CLI 独立。 */
export interface CodeBuddyCnIdeStatus {
  installed: boolean;
  running: boolean;
  dataDir: string | null;
  dbPath: string | null;
  dbExists: boolean;
  appPath: string | null;
  activeAccountId: string | null;
  activeAccountName: string | null;
  detectedFrom?: string;
  statePath?: string;
}

export interface CodeBuddyCnIdeSwitchResult {
  ok: boolean;
  account: string;
  accountId: string;
  dbPath?: string;
  restarted?: boolean;
  message?: string;
}


// ---------------------------------------------------------------------------
// 网关（workbuddy2api）集成
// ---------------------------------------------------------------------------

/** 网关配置（持久化在 ~/.wb-switch/gateway/gateway_config.json）。 */
/* 网关工作模式：
 * balance —— 负载均衡（默认）：账号池加权随机选号，自动避开冷却/熔断账号
 * pinned  —— 指定账号：只使用 pinned_uid 对应的那一个账号
 * rotation —— 单一模型 + 积分轮转：只用一个账号烧到不可用，再换按到期日
 *             排序的下一个（仍优先烧最快过期的额度）                      */
export type GatewayMode = "balance" | "pinned" | "rotation";

export interface GatewayConfig {
  /** 是否已启用（启动过即为 true）。 */
  enabled: boolean;
  /** 网关工作模式。 */
  mode?: GatewayMode;
  /** 指定账号模式下锁定的账号 uid。 */
  pinned_uid?: string | null;
  /**
   * 「单一模型 + 积分轮转」锁定的模型名（仅 rotation 模式生效）。
   *
   * 非空时网关**只放行该模型**，其余模型返回 400 model_not_allowed ——
   * 轮转的语义是「把这个账号的指定模型额度烧干净再换号」，模型是策略的一部分。
   */
  allowed_model?: string | null;
  /** 服务端口（权威字段，前端口选择器直接编辑它）。 */
  port: number;
  /** 监听地址，由 port 派生，如 ":7863"。 */
  listen: string;
  /** OpenAI 兼容接口的鉴权密钥；空 = 不鉴权。 */
  api_key: string;
  /** 随 App 启动而自动拉起。 */
  auto_start: boolean;
  last_status?: string | null;
  last_error?: string | null;
}

/** 单个「账号+模型」的冷却记录（来自网关 /status 的 model_cooling）。 */
export interface GatewayModelCooling {
  /** 被限流的模型名。 */
  model: string;
  /** 冷却截止时刻（ISO 8601）。 */
  until?: string;
  /** 距到期的剩余秒数（后端已算好，避免前后端时钟偏差）。 */
  remaining_sec?: number;
  /** 面向用户的说明文案（含模型名与重置时间）。 */
  reason?: string;
  /** true = 到期时间取自上游报错文案；false = 解析失败，回退固定软冷却。 */
  reset_at_parsed?: boolean;
}

/** 网关账号池中的单个账号运行态（来自网关 /status）。 */
export interface GatewayPoolAccount {
  uid: string;
  nickname?: string;
  credits?: number;
  cooling?: boolean;
  cool_kind?: string;
  cool_remaining_sec?: number;
  disabled?: boolean;
  reason?: string;
  success_count?: number;
  err_total?: number;
  in_flight?: number;
  /** 「最近到期积分」的到期时刻（Unix 秒）；缺省 = 未知。 */
  soonest_expire_at?: number;
  /** 到期日（YYYY-MM-DD），即选号分层档位键；同一天的账号同级。 */
  expire_day?: string;
  /**
   * 是否正因「到期档位更晚」而排队等待（当前轮不到它）。
   *
   * 由网关按与选号**完全相同**的档位口径算出。语义是「现在轮不到」，
   * **不是故障** —— 前面档位被消耗或冷却后会自动进入路由。
   */
  queued?: boolean;
  /**
   * 该账号当前因「模型级限流」而冷却的模型（按到期时间升序）。
   *
   * 与 `cooling` 的区别（界面据此区分两种冷却）：
   * - `cooling` = 账号级：余额（积分）欠费或账号被限速，整号不可用
   * - `model_cooling` 非空 = 模型级：仅这些模型不可用，换模型仍可用
   * 两者可同时存在。
   */
  model_cooling?: GatewayModelCooling[];
}

/** 网关 /status 响应。 */
export interface GatewayPool {
  accounts?: GatewayPoolAccount[];
  total?: number;
  healthy?: number;
  cooling?: number;
  disabled?: number;
  in_flight_full?: number;
  sticky_sessions?: number;
  redis_mode?: string;
}

/** 网关综合状态。 */
export interface GatewayStatus {
  running: boolean;
  reachable: boolean;
  base: string;
  openaiBase: string;
  port: number;
  exePath: string | null;
  exeFound: boolean;
  /** 网关来源：embedded=内嵌在单个 exe 内 / env=环境变量指定 / external=外部文件。 */
  exeSource?: "embedded" | "env" | "external";
  /** 配置端口当前是否空闲（网关运行时该端口被自己占用，属正常）。 */
  portAvailable?: boolean;
  /** 当前工作模式。 */
  mode?: GatewayMode;
  /** 指定账号模式锁定的 uid。 */
  pinnedUid?: string | null;
  /** 可选账号列表（供「指定账号」下拉使用）。 */
  accounts?: Array<{
    uid: string;
    nickname?: string;
    /**
     * 用户自定义备注（与 `AccountMeta.note` 同源，只在本地）。
     *
     * 界面优先用它标识账号：上游昵称对国服账号常为空，uid 只是随机串，
     * 备注才是用户认得出「这是谁的号」的线索。
     */
    note?: string;
    expiresAt?: number;
    needsRelogin?: boolean;
  }>;
  /**
   * 因「需重新登录」而被排除出网关账号池的账号。
   *
   * 这些账号的 refresh token 已被服务端拒绝，继续留在池里只会每次请求白跑一轮，
   * 因此同步时不会写入网关凭证目录；重新登录成功后会自动恢复。
   */
  excludedAccounts?: Array<{
    uid: string;
    nickname?: string;
    reason?: string | null;
  }>;
  authDir: string;
  /** 网关日志文件绝对路径（`~/.wb-switch/gateway/gateway.log`）。 */
  logFile?: string;
  /** 日志文件当前字节数（0 或缺失 = 还没有日志）。 */
  logBytes?: number;
  accountsInLibrary: number;
  config: GatewayConfig;
  health: { reachable?: boolean; healthy?: boolean; detail?: unknown } | null;
  pool: GatewayPool | null;
}

/** GET /api/gateway/config 响应。 */
export interface GatewayConfigResult {
  config: GatewayConfig;
  exeFound: boolean;
  exePath: string | null;
  authDir: string;
}

/** POST /api/gateway/{start,restart} 响应。 */
export interface GatewayStartResult {
  started?: boolean;
  base?: string;
  port?: number;
  accounts?: number;
  health?: unknown;
}

/** POST /api/gateway/sync 响应。 */
export interface GatewaySyncResult {
  ok: boolean;
  accounts?: number;
  changed?: string[];
  updatedFromGateway?: string[];
  reloaded?: boolean;
  error?: string;
}

/** POST /api/gateway/mode 响应（切换模式并立即生效）。 */
export interface GatewayModeSwitchResult {
  ok: boolean;
  mode?: GatewayMode;
  pinnedUid?: string | null;
  /** 重导出后的账号数。 */
  accounts?: number;
  changed?: string[];
  /** 是否因模式变更重启了网关（未运行时为 false）。 */
  reloaded?: boolean;
  config?: GatewayConfig;
  error?: string;
}

/** POST /api/gateway/port-check 响应。 */
export interface GatewayPortCheck {
  port: number;
  available: boolean;
  /** 是否为 1024 以下的特权端口。 */
  reserved: boolean;
  /** 该端口当前是否被本网关自身占用。 */
  inUseByGateway: boolean;
  /** 端口被占用时给出的可用建议端口。 */
  suggest: number | null;
}

/** 网关 Token 用量中的一组计量（口径与本地 Token 统计页一致）。 */
export interface GatewayUsageTotals {
  /** input + output + cacheWrite（不含 cacheRead，避免重复计数）。 */
  total: number;
  /** 输入 token，已包含缓存读取。 */
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  uncachedInput: number;
  /** 计入统计的成功请求数。 */
  records: number;
  /** 缓存命中率 = cacheRead / input；无输入时为 null。 */
  cacheHitRate: number | null;
}

/** 带分组键的用量（模型名 / 账号 uid / 日期）。 */
export interface GatewayUsageGroup extends GatewayUsageTotals {
  key: string;
}

/** 网关 /usage 响应（enabled=false 表示该网关未启用统计）。 */
export interface GatewayUsageSnapshot {
  enabled: boolean;
  generatedAt: number;
  /** 统计范围（近 N 天）；null = 全部历史。 */
  rangeDays?: number | null;
  summary?: GatewayUsageTotals;
  models?: GatewayUsageGroup[];
  accounts?: GatewayUsageGroup[];
  /** 按日期升序的日聚合。 */
  daily?: GatewayUsageGroup[];
  dailyByModel?: Record<string, GatewayUsageGroup[]>;
}

/** get_gateway_usage 的统一响应：网关不可达时 usage 为 null 且带 error。 */
export interface GatewayUsageResult {
  running: boolean;
  reachable: boolean;
  usage: GatewayUsageSnapshot | null;
  error: string | null;
}

/**
 * 网关日志读取结果（GET /api/gateway/log / read_gateway_log）。
 *
 * `truncated=true` 有两种成因，界面提示统一为「仅显示末尾部分」即可：
 * ① 文件超过行数上限；② 只回读了文件末尾一段字节（长文件常态）。
 */
export interface GatewayLogResult {
  ok: boolean;
  error?: string;
  /** 日志文件绝对路径（用户可自行去取完整文件） */
  path: string;
  /** 文件是否存在：不存在时 lines 为空，界面提示「还没有日志」 */
  exists?: boolean;
  /** 文件总字节数 */
  totalBytes?: number;
  /** 末尾若干行，最新一行在最后 */
  lines?: string[];
  /** true 表示只给了尾部，完整内容需看文件 */
  truncated?: boolean;
}

// ---------------------------------------------------------------------------
// 智能体客户端一键导入（agent_import）
// ---------------------------------------------------------------------------

/** 单个 AI 客户端的探测状态。 */
export interface AgentClientTarget {
  /** 客户端标识：dsh / claude-code / claude-desktop / codex */
  id: string;
  /** 显示名称 */
  label: string;
  /** 是否检测到已安装 */
  installed: boolean;
  /** 是否已接入本网关 */
  configured: boolean;
  /** 主要配置文件的绝对路径 */
  configPath: string;
  /** 补充说明信息 */
  note: string;
  /** 探测到的版本号 */
  version?: string | null;
}

/** GET /api/gateway/agents 探测响应。 */
export interface AgentDetectionResult {
  /** 本机网关根地址，如 http://127.0.0.1:7863 */
  base: string;
  /** 网关配置中是否已设置 API Key */
  hasApiKey: boolean;
  /** 探测到的客户端列表 */
  targets: AgentClientTarget[];
}

/** 网关模型项。 */
export interface GatewayModelItem {
  id: string;
  name?: string;
  context_length?: number;
  max_output_tokens?: number;
  owned_by?: string;
}

/** POST /api/gateway/agents/import 接入响应。 */
export interface AgentImportResult {
  ok: boolean;
  target: string;
  backupDir: string;
  files: string[];
  models?: string[];
  model?: string;
}

/** 批量接入/一键更新响应。 */
export interface AgentBatchImportResult {
  ok: boolean;
  count: number;
  outcomes: Array<{
    target: string;
    backupDir: string;
    files: string[];
    models?: string[];
  }>;
  models: string[];
}

/** POST /api/gateway/agents/restore 恢复响应。 */
export interface AgentRestoreResult {
  ok: boolean;
  restored: number;
  backupId: string;
}

/** 备份记录项。 */
export interface AgentBackupItem {
  id: string;
  createdAt: number;
  path: string;
}

