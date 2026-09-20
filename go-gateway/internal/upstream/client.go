// Package upstream 封装对 CodeBuddy 上游（chat / billing / auth）的全部 HTTP 调用，
// 以及错误分类（驱动 pool 冷却状态机）。
package upstream

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
)

// ErrKind 错误分类，pool 据此决定冷却时长。
type ErrKind int

const (
	ErrNone        ErrKind = iota // 成功
	ErrHardCredit                 // 余额不足（402 或 body 关键词）→ 长冷却
	ErrSoftRate                   // 429 软限流 → 短冷却
	ErrSessionDead                // 401 + 12153 offline session 失效 → 禁用
	ErrNotFound                   // 404 上游偶发 → 短冷却，不累计错误计数（防雪崩）
	ErrServer                     // 5xx 上游故障
	ErrClient                     // 其他 4xx / 业务错误
	// ErrModelRate 模型级限流：该账号的**这个模型**额度用尽（429 code=6004），
	// 与整个账号被封的 ErrSoftRate 不同 —— 上游明确提示"您也可以切换其他模型继续使用"，
	// 即该账号的其他模型仍然可用。冷却时长取上游给出的重置时间（解析失败回退软冷却）。
	ErrModelRate
	// ErrContextTooLong 请求的上下文超出模型窗口（HTTP 400 code=11115）。
	//
	// 这是**请求侧**错误，与账号无关：同一个请求体发给任何账号都会同样失败。
	// 因此必须与 ErrClient 区分开 —— 否则会落入「换号重试」路径，把整个请求体
	// 对着每个账号重传一遍（2026-09-15 实测：1.12M token 的请求被重传 3 次），
	// 最后还被包装成 503 no_healthy_account，把排查方向引向「账号故障」。
	ErrContextTooLong
)

func (k ErrKind) String() string {
	switch k {
	case ErrHardCredit:
		return "hard_credit"
	case ErrSoftRate:
		return "soft_rate"
	case ErrSessionDead:
		return "session_dead"
	case ErrNotFound:
		return "not_found"
	case ErrServer:
		return "server"
	case ErrClient:
		return "client"
	case ErrModelRate:
		return "model_rate"
	case ErrContextTooLong:
		return "context_too_long"
	default:
		return "none"
	}
}

// Error 带分类的上游错误。
type Error struct {
	Kind   ErrKind
	Status int
	Msg    string
}

func (e *Error) Error() string {
	return fmt.Sprintf("upstream %s (http %d): %s", e.Kind, e.Status, e.Msg)
}

// hardMarkers 余额不足关键词（小写比较 + 中文原文比较双通道）。
// hardMarkers 「余额/额度耗尽」的文案特征（命中即 ErrHardCredit → 长冷却）。
//
// 为什么要收单复数两种写法：上游国际版（workbuddy.ai）实际返回的是
// "Credits exhausted. Please visit the link below to purchase add-on packs
// and get more credits: …"（**复数** Credits），而早期只登记了单数
// "credit exhausted"，于是 strings.Contains 恒不命中 → 被判成 ErrSoftRate
// （软冷却 60 秒）→ 60 秒后重试同一个已耗尽账号，形成无限重试。
// 实测该响应体 15 个关键词全部未命中，故补齐复数形态。
var hardMarkers = []string{
	"insufficient credit", "insufficient credits",
	"no credit", "no credits",
	"credit exhausted", "credits exhausted",
	"credit exhaustion", "credits exhaustion",
	"out of credit", "out of credits",
	"quota exceeded", "quota exhaust",
	"payment required",
	"credit not enough", "credits not enough",
	"not enough credit", "not enough credits",
	"credit used up", "credits used up",
	"积分不足", "额度不足", "余额不足", "积分用完", "额度用尽", "没有积分",
}

var sessionDeadMarkers = []string{"Offline user session not found", "12153"}

// modelRateMarkers 模型级限流的判定依据（429 + 其中之一）。
//
// 主力信号是上游业务码 6004；文案关键词作为兜底 —— 上游改码不改文案时仍能识别，
// 但**必须**同时是 429，避免把其他场景的"频率限制"字样误判成模型限流。
var modelRateMarkers = []string{"超出频率限制", "切换其他模型"}

// modelRateCode 上游「模型级限流」的业务码。
const modelRateCode = 6004

// contextTooLongCode 上游「上下文超长」的业务码。
//
// 实测响应（2026-09-15 现场，prompt 1121509 > 上限 1048576）：
//
//	400 {"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum",
//	     "extError":{"code":"context_length_exceeded","type":"invalid_request_error"},
//	     "displayMsg":{"en":"The request exceeds the model context limit...",
//	                   "zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}
const contextTooLongCode = 11115

// contextTooLongCodeAlt 上游「上下文超长」的另一个业务码（hy3 等模型上实测）。
//
// 实测响应（2026-09-20，issue #27 用户现场，prompt 100001 > 上限 100000）：
//
//	400 {"code":4028,"msg":"prompt is too long: 100001 tokens > 100000 maximum", ...}
//
// 与 11115 是**同一类语义、不同码值**：上游按模型/版本下发不同码。
// 此前只认 11115，这次能判定成功靠的是文案兜底（"prompt is too long"）——
// 一旦上游只改文案不改码就会漏判，因此把 4028 一并登记为正式信号。
const contextTooLongCodeAlt = 4028

// contextTooLongCodes 全部已登记的「上下文超长」业务码。
var contextTooLongCodes = []int{contextTooLongCode, contextTooLongCodeAlt}

// contextTooLongMarkers 上下文超长的判定文案（中英双通道兜底）。
//
// 业务码是主信号；文案兜底用于上游改码不改文案的场景。措辞取自上游真实响应，
// 刻意含中英两版 displayMsg —— 上游按 Accept-Language 切换语言，只认一种会漏判。
//
// 注意 msg 有多种写法：实测同一业务码下遇到过 "prompt is too long"（国际版）
// 与 "input length too long"（国服 glm-5.3），两种都要登记。
var contextTooLongMarkers = []string{
	"context_length_exceeded",
	"prompt is too long",
	"input length too long",
	"exceeds the model context limit",
	"对话内容超出模型长度上限",
	"超出模型长度上限",
}

// IsContextTooLong 报告上游响应是否为「请求上下文超出模型窗口」。
//
// 三路判定，任一命中即成立：业务码（见 contextTooLongCodes）、
// extError.code=context_length_exceeded、或真实文案关键词（见 contextTooLongCode 注释里的实测响应）。
//
// 不按 status 门控：上游以 400 为主，但判定依据是业务语义而非状态码，
// 上游若改用 413 也能识别。
func IsContextTooLong(body string) bool {
	var env apiEnvelope
	if json.Unmarshal([]byte(body), &env) == nil {
		for _, code := range contextTooLongCodes {
			if env.Code == code {
				return true
			}
		}
	}
	var ext struct {
		ExtError struct {
			Code string `json:"code"`
		} `json:"extError"`
	}
	if json.Unmarshal([]byte(body), &ext) == nil && strings.EqualFold(ext.ExtError.Code, "context_length_exceeded") {
		return true
	}
	lower := strings.ToLower(body)
	for _, m := range contextTooLongMarkers {
		if strings.Contains(lower, strings.ToLower(m)) {
			return true
		}
	}
	return false
}

// resetTimeRe 从报错文案里提取重置时刻。
//
// 实测文案（2026-09-15 现场）：
//
//	您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。
//
// 捕获组 1 = 时间字面量（日期 + 时间），捕获组 2 = 时区后缀（如 "UTC+8" / "UTC+08:00"）。
// 时区后缀可选，仅为容错：**实测到的上游文案一律带 "UTC+8"**，无后缀分支尚未在真实
// 响应中观察到。保留它是为了上游改格式时不至于整个解析失败。
var resetTimeRe = regexp.MustCompile(`(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})(?:\s*(UTC[+-]\d{1,2}(?::\d{2})?|Z))?`)

// upstreamZone 上游（CodeBuddy 国服）的业务时区，用于解释**无时区后缀**的时间字面量。
//
// 为什么不用 time.Local：上游是国服服务，其自然日/重置时刻都按 CST（UTC+8）计；
// 用容器本地时区解释会让同一份响应在开发机（+08:00）与 UTC 容器上得出相差 8 小时的
// 结果，UTC 下极端情况会把尚未到期的重置点误判成"已过期"而退化成固定软冷却。
// 与 scheduler 的 cstZone 同一口径（中国无夏令时，固定 +8，不依赖 tzdata）。
//
// 注意：这是**防御性**选择 —— 真实的 6004 文案都带 "UTC+8"，故该分支正常不会走到；
// 带后缀时一律以文案里的偏移为准，不受本变量影响。
var upstreamZone = time.FixedZone("CST", 8*60*60)

// ParseResetTime 从上游报错文案里解析「重置时刻」；解析不出返回零值与 false。
//
// 时区处理：
//   - "UTC+8" / "UTC+08:00" → 固定偏移（**实测文案用的就是这种**）
//   - "Z"                   → UTC
//   - 无时区后缀 / 后缀非法  → 按上游业务时区（CST）解释，而非容器本地时区（见 upstreamZone）
//
// 只返回**未来**的时刻：解析出过去的时间说明文案里的重置点已过（如重放旧日志），
// 此时返回 false 交给调用方回退固定冷却，避免写入一个立即失效的冷却。
func ParseResetTime(body string, now time.Time) (time.Time, bool) {
	m := resetTimeRe.FindStringSubmatch(body)
	if m == nil {
		return time.Time{}, false
	}
	literal := strings.Replace(m[1], "T", " ", 1)
	layout := "2006-01-02 15:04:05"

	var loc *time.Location
	switch tz := strings.TrimSpace(m[2]); {
	case tz == "":
		loc = upstreamZone
	case tz == "Z":
		loc = time.UTC
	default:
		loc = parseUTCOffset(tz)
		if loc == nil {
			loc = upstreamZone
		}
	}
	ts, err := time.ParseInLocation(layout, literal, loc)
	if err != nil {
		return time.Time{}, false
	}
	if !ts.After(now) {
		return time.Time{}, false
	}
	return ts, true
}

// parseUTCOffset 解析 "UTC+8" / "UTC-05:30" 形式的固定偏移时区；非法返回 nil。
func parseUTCOffset(s string) *time.Location {
	rest := strings.TrimPrefix(s, "UTC")
	if rest == "" || (rest[0] != '+' && rest[0] != '-') {
		return nil
	}
	sign := 1
	if rest[0] == '-' {
		sign = -1
	}
	rest = rest[1:]
	hours, minutes := 0, 0
	if i := strings.IndexByte(rest, ':'); i >= 0 {
		h, err1 := strconv.Atoi(rest[:i])
		mm, err2 := strconv.Atoi(rest[i+1:])
		if err1 != nil || err2 != nil {
			return nil
		}
		hours, minutes = h, mm
	} else {
		h, err := strconv.Atoi(rest)
		if err != nil {
			return nil
		}
		hours = h
	}
	if hours > 23 || minutes > 59 {
		return nil
	}
	offset := sign * (hours*3600 + minutes*60)
	return time.FixedZone(s, offset)
}

// IsModelRateLimited 报告 429 响应体是否为「模型级限流」（该账号该模型额度用尽）。
//
// 注意与 ErrSoftRate 的区别：ErrSoftRate 是账号级限流（整个账号被限速），
// 而模型级限流只影响当前请求的那个模型，该账号换模型仍可用 —— 上游文案
// "您也可以切换其他模型继续使用" 明确指出了这一点。
func IsModelRateLimited(body string) bool {
	var env apiEnvelope
	if json.Unmarshal([]byte(body), &env) == nil && env.Code == modelRateCode {
		return true
	}
	// 文案兜底：上游改码不改文案时仍能识别。
	for _, m := range modelRateMarkers {
		if strings.Contains(body, m) {
			return true
		}
	}
	return false
}

// creditExhaustedCode 上游「额度耗尽」的业务码。
//
// 与 modelRateCode(6004) 的关键区别在于**嵌套层级**：6004 在顶层 `code`，
// 而 14018 藏在 `error.data.code`：
//
//	{"error":{"data":{"code":14018,"msg":"Credits exhausted. …"}}}
//
// 因此不能用 apiEnvelope（它只解顶层 code）。实测该响应的 HTTP 状态是 **429**，
// 而 429 分支若不识别它就会落进 ErrSoftRate → 只冷却 60 秒 → 无限重试。
const creditExhaustedCode = 14018

// isCreditExhaustedCode 报告响应体是否为「额度耗尽」业务码（含嵌套层级）。
func isCreditExhaustedCode(body string) bool {
	var env struct {
		Error struct {
			Data struct {
				Code int `json:"code"`
			} `json:"data"`
		} `json:"error"`
	}
	if err := json.Unmarshal([]byte(body), &env); err != nil {
		return false
	}
	return env.Error.Data.Code == creditExhaustedCode
}

// FriendlyMessage 把上游的原始错误体提炼成一句可读的原因，供客户端展示。
//
// 背景：此前直接把整段上游 JSON 拼进 OpenAI 错误体的 message，客户端看到的是
// 「all accounts unavailable (cooling/disabled): upstream soft_rate (http 429):
// {"error":{"data":{"code":14018,"msg":"Credits exhausted. …"}}}」—— 又长又难懂。
//
// 返回空串表示没有更优的表述，调用方应回退到原始文案。
func FriendlyMessage(kind ErrKind, status int, body string) string {
	// 业务码优先，但**只有响应体里真的带 14018 时才把该码写进文案**：
	// ErrHardCredit 也可能来自 HTTP 402 或关键词命中，此时硬写「上游 14018」
	// 会让用户拿着一个与响应不符的码去排查。
	if isCreditExhaustedCode(body) {
		return "账号额度已耗尽（上游 " + strconv.Itoa(creditExhaustedCode) + "）：请为该账号充值，或等待签到 / 免费额度恢复后重试"
	}
	switch {
	case kind == ErrHardCredit:
		return "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试"
	case kind == ErrModelRate:
		return "该账号在此模型上已达频率上限，已按上游给出的重置时间冷却；同一账号的其他模型仍可用"
	case kind == ErrSoftRate:
		return "账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试"
	case kind == ErrSessionDead:
		return "账号登录态已失效，需在「账号管理」页重新登录"
	case kind == ErrNotFound:
		return "上游返回 404（接口或模型不存在），已短暂冷却并切换账号"
	case kind == ErrServer && status > 0:
		return "上游服务异常（HTTP " + strconv.Itoa(status) + "），已切换到其他账号"
	}
	return ""
}

// Classify 按 HTTP 状态码 + body 判定错误类别。
func Classify(status int, body string) ErrKind {
	if status == http.StatusPaymentRequired {
		return ErrHardCredit
	}
	// 额度耗尽的业务码优先判定：它的 HTTP 状态是 429，若不先拦，
	// 会落进下面的 429 分支被判成 ErrSoftRate（仅 60 秒冷却）→ 无限重试。
	if isCreditExhaustedCode(body) {
		return ErrHardCredit
	}
	lower := strings.ToLower(body)
	for _, m := range hardMarkers {
		if strings.Contains(lower, strings.ToLower(m)) || strings.Contains(body, m) {
			return ErrHardCredit
		}
	}
	for _, m := range sessionDeadMarkers {
		if strings.Contains(body, m) {
			return ErrSessionDead
		}
	}
	if status == http.StatusTooManyRequests {
		// 模型级限流优先于账号级软限流：两者的冷却粒度与时长都不同
		// （模型级按 uid+model 冷却到上游给的重置时间）。
		if IsModelRateLimited(body) {
			return ErrModelRate
		}
		return ErrSoftRate
	}
	// 上下文超长必须早于通用 4xx 判定：它是请求侧错误，换号无用，
	// 需要独立 kind 让调用方「立即失败」而不是轮转重传整个请求体。
	// 放在 429 之后是有意的：429 一律按限流归类，保持既有语义不变。
	if IsContextTooLong(body) {
		return ErrContextTooLong
	}
	if status == http.StatusNotFound {
		return ErrNotFound
	}
	if status >= 500 {
		return ErrServer
	}
	if status >= 400 {
		return ErrClient
	}
	// HTTP 200 但业务 code 非 0 且含余额关键词的情况已被上面 hardMarkers 捕获。
	return ErrNone
}

// ContextTooLongMessage 把上下文超长的上游响应体提炼成一条**保留原文**的客户端消息。
//
// 为什么必须保留上游原文：下游客户端（如 DeepSeek Harness）靠文案模式识别上下文溢出
// （`prompt is too long` / `context_length_exceeded` / `exceeds the model context limit`），
// 据此触发自动压缩并重试。若只回我们自己的措辞，客户端就认不出这是溢出，
// 只会把它当成普通失败 —— 那正是本次死锁难以自愈的原因之一。
//
// 因此输出形如：`<上游 msg>（<中文 displayMsg>）`，两种语言的特征串都在，
// 中文提示同时给人类看。上游字段缺失时逐级回退，最终回退到原始 body。
func ContextTooLongMessage(body string) string {
	var env struct {
		Msg       string `json:"msg"`
		ExtError  struct {
			Message string `json:"message"`
		} `json:"extError"`
		DisplayMsg struct {
			Zh string `json:"zh"`
			En string `json:"en"`
		} `json:"displayMsg"`
	}
	_ = json.Unmarshal([]byte(body), &env)

	primary := strings.TrimSpace(env.Msg)
	if primary == "" {
		primary = strings.TrimSpace(env.ExtError.Message)
	}
	hint := strings.TrimSpace(env.DisplayMsg.Zh)
	if hint == "" {
		hint = strings.TrimSpace(env.DisplayMsg.En)
	}

	switch {
	case primary == "" && hint == "":
		return truncate(strings.TrimSpace(body), 400)
	case primary == "":
		return hint
	case hint == "" || strings.Contains(primary, hint):
		return primary
	default:
		return primary + "（" + hint + "）"
	}
}

// apiEnvelope 上游统一信封。
type apiEnvelope struct {
	Code int             `json:"code"`
	Msg  string          `json:"msg"`
	Data json.RawMessage `json:"data"`
}

// Client 上游 HTTP 客户端。Base 字段可覆盖便于测试。
type Client struct {
	HTTP *http.Client

	// ChatHTTP 聊天 SSE 专用 client：无总时长上限（Timeout=0），首字节由
	// Transport.ResponseHeaderTimeout 约束，流中空闲由 IdleTimeout 约束。
	// 与 HTTP 共享同一个 *http.Transport 实例，连接池不重复。
	ChatHTTP *http.Client

	// HeaderTimeout 聊天 SSE 首字节前（响应头）超时；<=0 表示未设置（回落 HTTP.Timeout）。
	HeaderTimeout time.Duration
	// IdleTimeout 聊天 SSE 流中空闲超时；<=0 表示禁用空闲监控。
	IdleTimeout time.Duration

	// effortsMu/efforts 缓存各模型 supportedEfforts（FetchModels 刷新），供请求体 effort 降级。
	effortsMu sync.RWMutex
	efforts   map[string][]string

	// SanitizeFingerprints 出站请求体黑名单指纹脱敏开关（默认 true；false 完全还原）。
	SanitizeFingerprints bool

	ChatBaseCN    string
	BillingBaseCN string
	// BaseIntl 国际版基址。与国服不同，国际版所有端点（chat / billing /
	// 签到 / 旅行 / token 刷新）都在同一域名下，因此只需一个 base。
	BaseIntl string

	// proxyURL 当前生效的显式代理（空串 = 未设置，回落环境变量）。
	// 由 SetProxy 维护；国际版（workbuddy.ai）在国内直连不稳定，通常需要它。
	proxyURL string
}

// IsIntl 判断账号是否属于国际版（供签到/旅行的区域范围过滤复用）。
//
// 依据 auth 文件里的 domain 字段：
//   *.workbuddy.cn / *.codebuddy.cn -> 国服
//   *.workbuddy.ai / *.codebuddy.ai -> 国际版
//
// domain 缺失时按国服处理：历史上只存在国服账号，保持向后兼容。
func IsIntl(a *auth.Auth) bool {
	if a == nil {
		return false
	}
	return strings.HasSuffix(strings.ToLower(strings.TrimSpace(a.Domain)), ".ai")
}

// isIntl 包内简写，保持既有调用点不变。
func isIntl(a *auth.Auth) bool { return IsIntl(a) }

// New 生产默认值。配置连接池减少 TLS 握手。
func New() *Client {
	// HTTP 与 ChatHTTP **共享同一个 Transport**：连接池不重复，
	// 两者只差总时长（ChatHTTP.Timeout=0，首字节由 ResponseHeaderTimeout 约束）。
	tr := newTransport(nil)
	return &Client{
		HTTP:                 &http.Client{Timeout: 120 * time.Second, Transport: tr},
		ChatHTTP:             &http.Client{Timeout: 0, Transport: tr},
		SanitizeFingerprints: true,
		ChatBaseCN:           "https://copilot.tencent.com",
			BillingBaseCN:        "https://www.codebuddy.cn",
			BaseIntl:             "https://www.workbuddy.ai",
	}
}

// newTransport 构造共用的 http.Transport。
//
// 关键：显式设置 Proxy 而不是依赖 http.ProxyFromEnvironment 的默认行为 ——
// 后者只读 HTTPS_PROXY 等**环境变量**，而用户在软件「设置 → 更新代理」里填的
// 代理是写在配置文件里的，环境变量通常是空的。于是国内直连
// workbuddy.ai 会超时（实测 wsarecv timeout），而浏览器因为读系统代理却正常。
//
// proxyURL 为 nil 时退回 ProxyFromEnvironment（仍尊重环境变量，行为与之前一致）。
func newTransport(proxyURL *url.URL) *http.Transport {
	tr := &http.Transport{
		MaxIdleConns:        100,
		MaxIdleConnsPerHost: 20,
		IdleConnTimeout:     90 * time.Second,
		// 聊天 SSE 首字节前硬上限（对短 RPC 无实际影响：其总时长 120s 更先到期）。
		ResponseHeaderTimeout: 120 * time.Second,
	}
	if proxyURL != nil {
		tr.Proxy = http.ProxyURL(proxyURL)
	} else {
		tr.Proxy = http.ProxyFromEnvironment
	}
	return tr
}

// SetProxy 设置出站代理（空串 = 不使用显式代理，回落环境变量）。
//
// 与 New() 一致：HTTP 与 ChatHTTP 共享**同一个** Transport（连接池不重复），
// 两者只差总时长。既有连接不会被打断，由旧 Transport 自行回收；
// 新请求立即走新代理。启动时调用一次即可。
func (c *Client) SetProxy(raw string) error {
	raw = strings.TrimSpace(raw)
	var proxyURL *url.URL
	if raw != "" {
		// 容忍用户只填 host:port（如 127.0.0.1:7890）：补 http:// 前缀。
		if !strings.Contains(raw, "://") {
			raw = "http://" + raw
		}
		u, err := url.Parse(raw)
		if err != nil {
			return fmt.Errorf("代理地址无效: %w", err)
		}
		// 注意 url.Parse 对 "http://:8080" 不报错（Host 为 ":8080"、Hostname() 为空），
		// 必须用 Hostname() 判空，否则会把一个连不上主机的地址当成合法配置。
		if u.Hostname() == "" {
			return fmt.Errorf("代理地址缺少主机名: %s", raw)
		}
		proxyURL = u
	}
	tr := newTransport(proxyURL)
	// 保留调用方已设的调优值（SetProxy 常在 New 之后、调优之前调用，
	// 但测试/其它调用顺序不确定，故这里从旧 Transport 继承可继承的字段）。
	if old, ok := c.ChatHTTP.Transport.(*http.Transport); ok && old != nil {
		tr.ResponseHeaderTimeout = old.ResponseHeaderTimeout
	}
	c.HTTP = &http.Client{Timeout: 120 * time.Second, Transport: tr}
	c.ChatHTTP = &http.Client{Timeout: 0, Transport: tr}
	c.proxyURL = raw
	return nil
}

// ProxyURL 返回当前生效的显式代理（空串 = 未设置）。
func (c *Client) ProxyURL() string {
	return c.proxyURL
}

// chatHTTP 返回聊天专用 client；未设置（如测试只注入 HTTP）时回落 HTTP。
func (c *Client) chatHTTP() *http.Client {
	if c.ChatHTTP != nil {
		return c.ChatHTTP
	}
	return c.HTTP
}

// chatBase 返回该账号的 chat 基址（按区域路由）。
func (c *Client) chatBase(a *auth.Auth) string {
	if isIntl(a) {
		if c.BaseIntl != "" {
			return c.BaseIntl
		}
		return "https://www.workbuddy.ai"
	}
	return c.ChatBaseCN
}

// prepareBody 组装出站请求体（脱敏开关由 Client.SanitizeFingerprints 控制）。
//
// intl 为该账号是否国际版（workbuddy.ai）：国际版要求 messages 首条必须是
// system（实测首条 user → HTTP 400 code=11128），需要在此补一条。
func (c *Client) prepareBody(body []byte, intl bool) []byte {
	return PrepareBodyForRegion(body, c.SanitizeFingerprints, c.effortsSnapshot(), intl)
}

// effortsSnapshot 返回 effort 能力缓存副本；nil 表示未知（透传不降级）。
func (c *Client) effortsSnapshot() map[string][]string {
	c.effortsMu.RLock()
	defer c.effortsMu.RUnlock()
	if len(c.efforts) == 0 {
		return nil
	}
	cp := make(map[string][]string, len(c.efforts))
	for k, v := range c.efforts {
		cp[k] = v
	}
	return cp
}

// billingBase 返回该账号的 billing 基址（按区域路由）。
//
// 国服 billing 与 chat 分属不同域名；国际版两者同域。
func (c *Client) billingBase(a *auth.Auth) string {
	if isIntl(a) {
		if c.BaseIntl != "" {
			return c.BaseIntl
		}
		return "https://www.workbuddy.ai"
	}
	return c.BillingBaseCN
}

// doJSON 发请求并解信封；HTTP 非 2xx 或业务 code != 0 时返回带 body 片段的 *Error。
func (c *Client) doJSON(req *http.Request) (json.RawMessage, error) {
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		kind := Classify(resp.StatusCode, string(raw))
		return nil, &Error{Kind: kind, Status: resp.StatusCode, Msg: truncate(string(raw), 200)}
	}
	var env apiEnvelope
	if err := json.Unmarshal(raw, &env); err != nil {
		return nil, fmt.Errorf("parse failed: %w (body: %s)", err, truncate(string(raw), 120))
	}
	if env.Code != 0 {
		kind := Classify(resp.StatusCode, env.Msg)
		if kind == ErrNone {
			kind = ErrClient
		}
		return nil, &Error{Kind: kind, Status: resp.StatusCode, Msg: fmt.Sprintf("code=%d msg=%s", env.Code, truncate(env.Msg, 160))}
	}
	return env.Data, nil
}

// RefreshToken 刷新 access token；成功时更新 a 的字段（缺省值保留旧值），
// 调用方负责 SaveAtomic。全程持 a 锁，防止并发 SaveAtomic 读半更新 token。
func (c *Client) RefreshToken(a *auth.Auth) error {
	a.Lock()
	defer a.Unlock()
	if strings.TrimSpace(a.RefreshToken) == "" {
		return fmt.Errorf("no refreshToken")
	}
	url := c.chatBase(a) + "/v2/plugin/auth/token/refresh"
	req, err := http.NewRequest(http.MethodPost, url, nil)
	if err != nil {
		return err
	}
	RefreshHeaders(req, a)
	data, err := c.doJSON(req)
	if err != nil {
		return err
	}
	var tok struct {
		AccessToken  string `json:"accessToken"`
		RefreshToken string `json:"refreshToken"`
		ExpiresIn    int64  `json:"expiresIn"`
		Domain       string `json:"domain"`
	}
	if err := json.Unmarshal(data, &tok); err != nil || tok.AccessToken == "" {
		return fmt.Errorf("refresh_failed: no accessToken in response — re-login required")
	}
	a.AccessToken = tok.AccessToken
	if tok.RefreshToken != "" {
		a.RefreshToken = tok.RefreshToken
	}
	if tok.Domain != "" {
		a.Domain = tok.Domain
	}
	// preserveExpiry：响应缺 expiresIn 时保留旧过期时间，避免刷新风暴。
	if tok.ExpiresIn > 0 {
		a.ExpiresAt = time.Now().Add(time.Duration(tok.ExpiresIn) * time.Second).Unix()
	}
	return nil
}

// ChatStream 发 chat 请求并返回原始 SSE body 流（调用方负责 Close）。
// 非 2xx 时 rc 为 nil、body 为上游响应体（供调用方 Classify(status, string(body))）、err 为 nil；
// 只有传输层失败才返回 err。
func (c *Client) ChatStream(a *auth.Auth, body []byte) (rc io.ReadCloser, status int, respBody []byte, err error) {
	url := c.chatBase(a) + "/v2/chat/completions"
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(c.prepareBody(body, isIntl(a))))
	if err != nil {
		return nil, 0, nil, err
	}
	ChatHeaders(req, a)
	ctx, cancel := context.WithCancel(context.Background())
	req = req.WithContext(ctx)
	resp, err := c.chatHTTP().Do(req)
	if err != nil {
		cancel()
		log.Printf("chat_stream uid=%s: transport error: %v", a.UID, err)
		return nil, 0, nil, err
	}
	if resp.StatusCode >= 400 {
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		cancel()
		kind := Classify(resp.StatusCode, string(raw))
		log.Printf("chat_stream uid=%s: upstream %d %s body=%s",
			a.UID, resp.StatusCode, kind, truncate(string(raw), 200))
		return nil, resp.StatusCode, raw, nil
	}
	// 成功分支：cancel 所有权交给 monitorBody（其 Close 会 cancel）；
	// IdleTimeout<=0 时 monitorBody 原样返回底流、无人调 cancel——可接受：
	// ctx 无 deadline 无 goroutine，连接由 resp.Body.Close 正常清理。
	return monitorBody(resp.Body, c.IdleTimeout, cancel), resp.StatusCode, nil, nil
}

// ModelInfo 动态模型信息（含 maxInputTokens/maxOutputTokens）。
type ModelInfo struct {
	ID            string
	Name          string
	ContextWindow int64    // = maxInputTokens
	MaxTokens     int64    // = maxOutputTokens
	Efforts       []string // reasoning.supportedEfforts（空=未知/固定档）
	// DefaultEffort 上游给的默认思考档 = reasoning.effort（空=未声明）。
	//
	// 与 Efforts 分开：Efforts 是「允许哪些档」，DefaultEffort 是「不指定时用哪档」。
	// 上游同时给了两者，但默认档未必在 supportedEfforts 里（上游数据未保证），
	// 因此不要用它去推断 Efforts，也不要用 Efforts[0] 去冒充它。
	DefaultEffort string
	// SupportsImages 是否接受图片输入。
	//
	// nil **不等于** false：上游 /v3/config 只给对话模型写 supportsImages，
	// 补全/图片生成等条目整条缺失该字段。缺失时是「未声明」，不是「不支持」——
	// 谎报成纯文本会让客户端把本可用的图片能力关掉，所以这里保留三态。
	SupportsImages *bool
}

// modelsConfigUA 拉模型配置用的 User-Agent。
//
// **必须**用 WorkBuddy 前缀（实测 2026-09-15）：
//
//	WorkBuddy/... → 200，返回 WorkBuddy 产品的模型清单
//	CLI/...       → 200，但返回的是 **CodeBuddy** 产品的清单（另一套模型）
//	其他任意 UA    → 400
//
// 两个清单差异很大且各自都"看起来合理"，很容易误判成上游数据错误：
//
//	国际版 WorkBuddy 20 个 / CodeBuddy 17 个
//	  仅 WorkBuddy 有：gpt-6-astra、deepseek-v4.1-flash、hy4-preview-f、kimi-k2.8-preview
//	  仅 CodeBuddy 有：gpt-5.3-codex、minimax-m3
//
// 其中 gpt-6-astra / deepseek-v4.1-flash / kimi-k2.8-preview 实测在国际版**均可正常调用**，
// 说明 WorkBuddy 前缀拿到的才是本产品真实可用集。
//
// 另：UA 只影响本接口的返回内容，不影响 /v2/chat/completions（实测两者 chat 结果一致），
// 因此这里单独覆盖，不动全局 clientUA。
const modelsConfigUA = "WorkBuddy/5.5.2 WorkBuddy/5.5.2 CLI/2.137.1"

// modelsConfigPath 模型配置接口路径。
//
// 为什么不用 /console/enterprises/personal/models：
// 该接口在国际版恒返回 500（openresty 错误页，实测 5/5 账号，
// 与认证方式/请求头无关），导致国际版永远只能靠硬编码静态表。
// /v3/config 返回同一份模型数据且两个区域都可用（实测国际版 21、国服 52）。
const modelsConfigPath = "/v3/config"

// effectiveSupportsImages 合并「模型是否支持图片」与「账号级多模态是否被禁用」。
//
// 三态语义（返回值可能是 nil = 未声明）：
//
//	supportsImages 缺失 + 未禁用 → nil（未声明，客户端按自己的默认处理）
//	supportsImages=true        → true
//	supportsImages=false       → false
//	disabledMultimodal=true    → false（无条件，账号级开关优先）
//
// disabledMultimodal 优先是刻意的：上游用它表达「该账号不能发图片」，
// 与模型自身能力无关，此时宣称支持会让客户端发出必然失败的请求。
func effectiveSupportsImages(supportsImages *bool, disabledMultimodal bool) *bool {
	if disabledMultimodal {
		f := false
		return &f
	}
	return supportsImages
}

// FetchModels 调上游模型配置接口，返回该账号所在区域的可用模型。
//
// 数据来源是 data.agents[name=="cli"].models（**不是** data.models）：
//
//	data.models        = 产品全部模型池，含图片/视频生成、lite 辅助模型等
//	agents[cli].models = CLI agent 真正可用的子集（正是客户端选模型时看到的）
//
// 实测（国际版）：models 21 个，其中 cli.models 20 个；被排除的是
// gemini-3.0-pro-image（图片生成）、hunyuan-video-art（视频生成）、default-model-lite（内部 lite）等。
// 若直接返回 data.models，客户端会看到一批根本不能用于对话的模型。
//
// 元数据（contextWindow/maxOutputTokens/efforts）从 data.models 按 id 关联补齐。
func (c *Client) FetchModels(a *auth.Auth) ([]ModelInfo, error) {
	url := c.chatBase(a) + modelsConfigPath
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+a.AccessToken)
	req.Header.Set("Accept", "application/json")
	origin := originRefererFor(a)
	req.Header.Set("Origin", origin)
	req.Header.Set("Referer", origin+"/")
	req.Header.Set("User-Agent", modelsConfigUA)
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("models api status %d: %s", resp.StatusCode, truncate(string(raw), 120))
	}
	var env struct {
		Code int `json:"code"`
		Data struct {
			Models []struct {
				ID              string `json:"id"`
				Name            string `json:"name"`
				MaxInputTokens  int64  `json:"maxInputTokens"`
				MaxOutputTokens int64  `json:"maxOutputTokens"`
				Disabled        bool   `json:"disabled"`
				// 指针：区分「显式 false」与「字段缺失」（见 ModelInfo.SupportsImages）。
				SupportsImages *bool `json:"supportsImages"`
				// disabledMultimodal：账号级多模态开关。实测当前恒为 false/缺失，
				// 但一旦为 true，即便 supportsImages=true 也不能收图片。
				DisabledMultimodal bool `json:"disabledMultimodal"`
				Reasoning          struct {
					Effort           string   `json:"effort"`
					SupportedEfforts []string `json:"supportedEfforts"`
				} `json:"reasoning"`
			} `json:"models"`
			Agents []struct {
				Name   string   `json:"name"`
				Models []string `json:"models"`
			} `json:"agents"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &env); err != nil {
		return nil, fmt.Errorf("models parse: %w", err)
	}
	if env.Code != 0 {
		return nil, fmt.Errorf("models api code=%d", env.Code)
	}

	// 先按 id 建索引，便于给 cli 清单补元数据。
	// disabled 单独记一份：agents[cli].models 是「这个 agent 允许用哪些模型」的白名单，
	// 而 disabled 是模型级的停用开关，被停用的模型可以仍留在白名单里。
	// 两者都不看会把已停用的模型下发给客户端（选中即报错）。
	meta := make(map[string]ModelInfo, len(env.Data.Models))
	disabled := make(map[string]bool, len(env.Data.Models))
	for _, m := range env.Data.Models {
		if m.ID == "" {
			continue
		}
		if _, dup := meta[m.ID]; dup {
			continue
		}
		meta[m.ID] = ModelInfo{
			ID:            m.ID,
			Name:          m.Name,
			ContextWindow: m.MaxInputTokens,
			MaxTokens:     m.MaxOutputTokens,
			Efforts:       m.Reasoning.SupportedEfforts,
			DefaultEffort: m.Reasoning.Effort,
			// 账号级多模态开关为 true 时强制降级为 false：上游语义是
			// 「即便模型本身支持，该账号也不许用图片」，此时不能宣称支持。
			SupportsImages: effectiveSupportsImages(m.SupportsImages, m.DisabledMultimodal),
		}
		if m.Disabled {
			disabled[m.ID] = true
		}
	}

	// 取 cli agent 的可用清单（这是客户端真正能选的模型）。
	var cliModels []string
	for _, ag := range env.Data.Agents {
		if ag.Name == "cli" {
			cliModels = ag.Models
			break
		}
	}

	out := make([]ModelInfo, 0, len(cliModels))
	seen := make(map[string]bool, len(cliModels))
	for _, id := range cliModels {
		if id == "" || seen[id] {
			continue
		}
		if mi, ok := meta[id]; ok {
			// 上游显式标了 disabled 的不下发（与旧实现一致）。
			// 池里查不到该 id 时无从判断，按「宁可多」返回。
			if disabled[id] {
				continue
			}
			seen[id] = true
			out = append(out, mi)
			continue
		}
		// cli 清单里有、models 池里没有：仍要返回（它确实可用），只是元数据未知。
		seen[id] = true
		out = append(out, ModelInfo{ID: id})
	}

	// 兜底：上游没给 cli agent 时退回全量池（宁可多不可少，保持旧行为）。
	if len(out) == 0 {
		for _, m := range env.Data.Models {
			if m.Disabled || m.ID == "" || seen[m.ID] {
				continue
			}
			seen[m.ID] = true
			out = append(out, meta[m.ID])
		}
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("models api returned empty list")
	}
	// 刷新 effort 能力缓存（供请求体降级；无 supportedEfforts 的模型不入缓存）。
	cache := make(map[string][]string, len(out))
	for _, mi := range out {
		if len(mi.Efforts) > 0 {
			cache[mi.ID] = mi.Efforts
		}
	}
	c.effortsMu.Lock()
	c.efforts = cache
	c.effortsMu.Unlock()
	return out, nil
}

// resourcePackage billing get-user-resource 返回的单个套餐。
type resourcePackage struct {
	PackageName         string `json:"PackageName"`
	CapacitySize        int64  `json:"CapacitySize"`
	CapacityRemain      int64  `json:"CapacityRemain"`
	CapacityUsed        int64  `json:"CapacityUsed"`
	CycleCapacitySize   int64  `json:"CycleCapacitySize"`
	CycleCapacityRemain int64  `json:"CycleCapacityRemain"`
	CycleCapacityUsed   int64  `json:"CycleCapacityUsed"`

	// 到期字段（实测国服与国际版均返回）：
	//
	//	DeductionEndTime 抵扣截止（epoch 毫秒）—— 额度真正失效的时刻，优先采用
	//	ExpiredTime / CycleEndTime 兼容回退（实测可能是 "2006-01-02 15:04:05" 字符串）
	//
	// 声明为 any 是因为同一字段在不同区域/套餐上出现过数字与字符串两种形态。
	DeductionEndTime any `json:"DeductionEndTime"`
	ExpiredTime      any `json:"ExpiredTime"`
	CycleEndTime     any `json:"CycleEndTime"`
}

// remain 该套餐可花费积分（与原聚合口径逐字一致：Cycle* 优先，负值钳 0）。
func (p resourcePackage) remain() int64 {
	var r int64
	switch {
	case p.CycleCapacitySize > 0:
		r = p.CycleCapacityRemain
	case p.CycleCapacityRemain > 0 || p.CycleCapacityUsed > 0:
		r = p.CycleCapacityRemain
	default:
		r = p.CapacityRemain
	}
	if r < 0 {
		r = 0
	}
	return r
}

// expiryUnix 该套餐的到期时刻（Unix 秒）；0 = 未知。
func (p resourcePackage) expiryUnix() int64 {
	for _, v := range []any{p.DeductionEndTime, p.ExpiredTime, p.CycleEndTime} {
		if at := parseExpiryUnix(v); at > 0 {
			return at
		}
	}
	return 0
}

// normalizeEpoch 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
func normalizeEpoch(n int64) int64 {
	if n <= 0 {
		return 0
	}
	if n > 1_000_000_000_000 {
		return n / 1000
	}
	return n
}

// parseExpiryUnix 解析上游到期字段，兼容 epoch 秒/毫秒与常见日期字符串。
func parseExpiryUnix(v any) int64 {
	switch t := v.(type) {
	case float64:
		return normalizeEpoch(int64(t))
	case json.Number:
		n, err := t.Int64()
		if err != nil {
			return 0
		}
		return normalizeEpoch(n)
	case string:
		s := strings.TrimSpace(t)
		if s == "" {
			return 0
		}
		if n, err := strconv.ParseInt(s, 10, 64); err == nil {
			return normalizeEpoch(n)
		}
		for _, layout := range []string{
			"2006-01-02 15:04:05",
			"2006-01-02T15:04:05Z07:00",
			"2006-01-02 15:04:05.999",
		} {
			if ts, err := time.ParseInLocation(layout, s, time.Local); err == nil {
				return ts.Unix()
			}
		}
		// 仅日期：按当日 23:59:59 计（额度一般用到当天结束）。
		if ts, err := time.ParseInLocation("2006-01-02", s, time.Local); err == nil {
			return ts.Add(24*time.Hour - time.Second).Unix()
		}
	}
	return 0
}

// CreditInfo 账号积分余额与到期信息（billing get-user-resource 的归一化结果）。
type CreditInfo struct {
	// Remain 所有套餐可花费积分之和（负值钳 0）。
	Remain int64
	// SoonestExpireAt 仍有剩余积分的套餐中最早的到期时刻（Unix 秒）；0 = 未知。
	//
	// 供账号池做「按到期紧迫度分层」选号：先烧快过期的额度，避免积分作废。
	SoonestExpireAt int64
}

// UserResource 查询账号当前可花费积分余额（所有套餐 CycleCapacity 聚合，负值钳 0）。
func (c *Client) UserResource(a *auth.Auth) (remain int64, err error) {
	info, err := c.UserResourceDetail(a)
	return info.Remain, err
}

// UserResourceDetail 查询积分余额与「最近到期」时刻（一次请求同时取回，不额外打上游）。
func (c *Client) UserResourceDetail(a *auth.Auth) (CreditInfo, error) {
	url := c.billingBase(a) + "/v2/billing/meter/get-user-resource"
	now := time.Now()
	body := map[string]any{
		"PageNumber":               1,
		"PageSize":                 100,
		"ProductCode":              "p_tcaca",
		"Status":                   []int{0, 3},
		"PackageEndTimeRangeBegin": now.Format("2006-01-02 15:04:05"),
		"PackageEndTimeRangeEnd":   now.Add(365 * 101 * 24 * time.Hour).Format("2006-01-02 15:04:05"),
	}
	raw, _ := json.Marshal(body)
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(raw))
	if err != nil {
		return CreditInfo{}, err
	}
	BillingHeaders(req, a)
	data, err := c.doJSON(req)
	if err != nil {
		return CreditInfo{}, err
	}
	var resp struct {
		Response struct {
			Data struct {
				Accounts []resourcePackage `json:"Accounts"`
			} `json:"Data"`
		} `json:"Response"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return CreditInfo{}, fmt.Errorf("resource parse: %w", err)
	}
	var info CreditInfo
	for _, acct := range resp.Response.Data.Accounts {
		r := acct.remain()
		info.Remain += r
		// 只有「还有剩余」的套餐才代表真实到期压力；已用尽的套餐到期日再早也无意义。
		if r <= 0 {
			continue
		}
		if at := acct.expiryUnix(); at > 0 && (info.SoonestExpireAt == 0 || at < info.SoonestExpireAt) {
			info.SoonestExpireAt = at
		}
	}
	return info, nil
}

// DailyCheckin 执行每日签到。已签到（业务 code 非 0）也返回错误，调用方按 msg 区分。
func (c *Client) DailyCheckin(a *auth.Auth) error {
	url := c.billingBase(a) + "/v2/billing/meter/daily-checkin"
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader([]byte("{}")))
	if err != nil {
		return err
	}
	BillingHeaders(req, a)
	_, err = c.doJSON(req)
	return err
}

func truncate(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) > n {
		return s[:n]
	}
	return s
}
