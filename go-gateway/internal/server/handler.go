// Package server 暴露 OpenAI 兼容 HTTP 接口，内部驱动 pool 挑号 + upstream 转发。
package server

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

// Config handler 依赖。
type Config struct {
	Pool      *pool.Pool
	Upstream  *upstream.Client
	APIKey    string // 空 = 不鉴权
	MaxRotate int    // 单请求最多换号次数，默认 3
	// Session 会话粘性路由器（可选；nil = 关闭粘性，纯 Pick 轮换）。
	Session *session.Router
	// StickyCount 返回当前粘性会话绑定数（供 /status）；nil 时报告 0。
	StickyCount func() int
	// RedisMode 观测字段（"upstash" / "noop"），供 /status 透出。
	RedisMode    string
	SoftCooldown time.Duration // 429 冷却，默认 60s
	RefreshSkew  time.Duration // token 提前刷新窗口，默认 10m
}

// ServiceName 网关身份标识。经 /healthz 响应体 service 字段与 X-Service 头同时透出：
// 宿主（如 workbuddy-switch 托管网关子进程）探测同端口的旧服务/其他服务时，对方即使
// 返回 2xx 也不带本标识，宿主据此可识别"假成功"。
const ServiceName = "workbuddy2api"

// Handler 主路由。
type Handler struct {
	cfg Config
	mux *http.ServeMux
}

// NewHandler 构建 handler。
func NewHandler(cfg Config) *Handler {
	if cfg.MaxRotate <= 0 {
		cfg.MaxRotate = 3
	}
	if cfg.SoftCooldown <= 0 {
		cfg.SoftCooldown = 60 * time.Second
	}
	if cfg.RefreshSkew <= 0 {
		cfg.RefreshSkew = 10 * time.Minute
	}
	h := &Handler{cfg: cfg, mux: http.NewServeMux()}
	h.mux.HandleFunc("POST /v1/chat/completions", h.withAuth(h.chatCompletions))
	h.mux.HandleFunc("POST /v1/responses", h.withAuth(h.responses))
	h.mux.HandleFunc("POST /responses", h.withAuth(h.responses))
	h.mux.HandleFunc("POST /v1/messages", h.withAuth(h.messages))
	h.mux.HandleFunc("POST /messages", h.withAuth(h.messages))
	h.mux.HandleFunc("GET /v1/models", h.withAuth(h.models))
	h.mux.HandleFunc("GET /status", h.withAuth(h.status))
	h.mux.HandleFunc("GET /healthz", h.healthz)
	return h
}

func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.mux.ServeHTTP(w, r)
}

// withAuth 校验客户端凭据。
//
// 同时接受两种头部形态，覆盖不同客户端的认证习惯：
//   - Authorization: Bearer <key> —— OpenAI SDK、Claude Code 的 ANTHROPIC_AUTH_TOKEN、
//     Claude Desktop 3P（inferenceGatewayAuthScheme=bearer）
//   - x-api-key: <key>            —— Anthropic SDK、Claude Code 的 ANTHROPIC_API_KEY
func (h *Handler) withAuth(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if !h.authorized(r) {
			writeOpenAIError(w, http.StatusUnauthorized, "invalid_api_key", "missing or invalid API key")
			return
		}
		next(w, r)
	}
}

// authorized 判断请求是否携带了正确的网关密钥；未配置密钥时一律放行。
func (h *Handler) authorized(r *http.Request) bool {
	if h.cfg.APIKey == "" {
		return true
	}
	if authz := r.Header.Get("Authorization"); strings.HasPrefix(authz, "Bearer ") {
		return strings.TrimPrefix(authz, "Bearer ") == h.cfg.APIKey
	}
	return r.Header.Get("x-api-key") == h.cfg.APIKey
}

func (h *Handler) healthz(w http.ResponseWriter, r *http.Request) {
	total, healthy, _, _, _ := h.cfg.Pool.CountsDetailed()
	// 用 ServableNow 判定：healthy>0 但全占满在途时 chat 会 503，探活必须同口径，
	// 否则负载均衡器会把流量持续打进无法受理的实例。
	status := http.StatusOK
	if !h.cfg.Pool.ServableNow() {
		status = http.StatusServiceUnavailable
	}
	// 恒无鉴权（负载均衡/编排探活只需 2xx/503 语义），身份靠 service 字段 + X-Service 头双保险。
	w.Header().Set("X-Service", ServiceName)
	writeJSON(w, status, map[string]any{
		"healthy": healthy,
		"total":   total,
		"service": ServiceName,
	})
}

func (h *Handler) status(w http.ResponseWriter, r *http.Request) {
	total, healthy, cooling, disabled, inFlightFull := h.cfg.Pool.CountsDetailed()
	sticky := 0
	if h.cfg.StickyCount != nil {
		sticky = h.cfg.StickyCount()
	}
	redisMode := h.cfg.RedisMode
	if redisMode == "" {
		redisMode = "noop"
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"accounts":        h.cfg.Pool.List(),
		"total":           total,
		"healthy":         healthy,
		"cooling":         cooling,
		"disabled":        disabled,
		"in_flight_full":  inFlightFull,
		"sticky_sessions": sticky,
		"redis_mode":      redisMode,
	})
}

// 静态 CN 模型表（api-reference §5，动态接口失败时的回退）。
var staticModels = []map[string]any{
	{"id": "glm-5.2", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "glm-5.1", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "glm-5v-turbo", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "kimi-k2.7", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "minimax-m3", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3-preview", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3-preview-agent", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "deepseek-v4-pro", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "deepseek-v4-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
}

// staticModelsIntl 国际版静态模型表。
//
// 国际版的 /console/enterprises/personal/models 在当前版本返回 500
// （openresty 错误页），无法动态拉取，因此这里内置一份。
// 取自国际版客户端本地缓存 acc-product-config-v3.json 的 agents[0].models。
//
// 注意与国服的差异（这也是客户端选模型时最易踩的坑）：
//
//	国服   deepseek-v4-flash   / glm-5.2 / kimi-k2.7 / minimax-m3
//	国际版 deepseek-v4.1-flash / glm-5.3 / kimi-k3   / gpt-5.6-* / gemini-3.5-flash
var staticModelsIntl = []map[string]any{
	{"id": "default-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 200000},
	{"id": "fast-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 176000},
	{"id": "balanced-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 176000},
	{"id": "primary-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 176000},
	{"id": "deep-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 176000},
	{"id": "hy4-preview", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "hy3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "deepseek-v4.1-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-6-astra", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.6-sol", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.6-terra", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.6-luna", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.5", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.4", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gpt-5.3-codex", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "gemini-3.5-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "glm-5.3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "glm-5.2", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "kimi-k3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
	{"id": "kimi-k2.6", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 131072},
}

// staticModelsAll 合并两个区域的模型（按 id 去重，国服优先）。
//
// /v1/models 没有账号上下文，因此返回并集：客户端据此得知全部可用名称。
// 具体某个名称能否用，取决于实际选中的账号属于哪个区域 ——
// 不匹配时上游会返回 code=11102 model service info not found，提示清晰。
var staticModelsAll = func() []map[string]any {
	seen := map[string]bool{}
	out := make([]map[string]any, 0, len(staticModels)+len(staticModelsIntl))
	for _, m := range append(append([]map[string]any{}, staticModels...), staticModelsIntl...) {
		id, _ := m["id"].(string)
		if id == "" || seen[id] {
			continue
		}
		seen[id] = true
		out = append(out, m)
	}
	return out
}()

// dynamicModelsCache 动态模型缓存。
var dynamicModelsCache struct {
	sync.RWMutex
	ids      []upstream.ModelInfo
	fetched  time.Time // 最近一次成功拉取时间
	lastFail time.Time // 最近一次拉取失败时间（负缓存）
}

const (
	dynamicModelsTTL        = time.Hour
	modelsFetchFailCooldown = 5 * time.Minute
)

// models 返回模型列表：优先动态（缓存 1h），失败回退静态表。
func (h *Handler) models(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"object": "list",
		"data":   h.modelList(),
	})
}

// modelList 动态获取模型列表并包装成 OpenAI 格式（含 context_length）。
func (h *Handler) modelList() []map[string]any {
	if infos := h.fetchDynamicModels(); len(infos) > 0 {
		out := make([]map[string]any, 0, len(infos)+len(staticModelsIntl))
		seen := make(map[string]bool, len(infos)+len(staticModelsIntl))
		for _, mi := range infos {
			entry := map[string]any{
				"id":                mi.ID,
				"object":            "model",
				"created":           1753600000,
				"owned_by":          "workbuddy",
				"context_length":    mi.ContextWindow,
				"max_output_tokens": mi.MaxTokens,
			}
			if mi.ContextWindow == 0 {
				entry["context_length"] = 131072 // 兜底
			}
			seen[mi.ID] = true
			out = append(out, entry)
		}
		// 动态列表来自实际取到账号的那个区域（通常是国服）；国际版的模型列表
		// 接口本身不可用（500），只能靠静态表补齐。不补的话，混合账号池下
		// 客户端看不到国际版模型名，也就无法主动选用。
		for _, m := range staticModelsIntl {
			if id, _ := m["id"].(string); id != "" && !seen[id] {
				seen[id] = true
				out = append(out, m)
			}
		}
		return out
	}
	return staticModelsAll
}

// fetchDynamicModels 从池中任一健康账号拉模型列表（含 contextWindow/maxTokens），缓存 1h。
// 拉取失败记录时间戳进入 5min 负缓存，冷却期内直接用静态表，避免反复打上游。
func (h *Handler) fetchDynamicModels() []upstream.ModelInfo {
	dynamicModelsCache.RLock()
	if len(dynamicModelsCache.ids) > 0 && time.Since(dynamicModelsCache.fetched) < dynamicModelsTTL {
		out := dynamicModelsCache.ids
		dynamicModelsCache.RUnlock()
		return out
	}
	// 失败负缓存：冷却期内不再请求上游。
	if !dynamicModelsCache.lastFail.IsZero() && time.Since(dynamicModelsCache.lastFail) < modelsFetchFailCooldown {
		dynamicModelsCache.RUnlock()
		return nil
	}
	dynamicModelsCache.RUnlock()

	acct := h.cfg.Pool.Pick()
	if acct == nil {
		return nil
	}
	infos, err := h.cfg.Upstream.FetchModels(acct)
	if err != nil || len(infos) == 0 {
		// 拉取失败惩罚该账号，避免下次 Pick 又选中同一个反复失败；lastFail 保持全局负缓存。
		h.cfg.Pool.NoteError(acct.UID)
		dynamicModelsCache.Lock()
		dynamicModelsCache.lastFail = time.Now()
		dynamicModelsCache.Unlock()
		return nil
	}
	dynamicModelsCache.Lock()
	dynamicModelsCache.ids = infos
	dynamicModelsCache.fetched = time.Now()
	dynamicModelsCache.lastFail = time.Time{} // 成功则清空负缓存
	dynamicModelsCache.Unlock()
	return infos
}

func (h *Handler) chatCompletions(w http.ResponseWriter, r *http.Request) {
	body, err := readLimitedBody(r)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "read body: "+err.Error())
		return
	}
	var peek struct {
		Stream bool `json:"stream"`
	}
	_ = json.Unmarshal(body, &peek)

	st := newChatStat(time.Now(), body, peek.Stream)
	defer st.done()

	sessKey := ""
	if h.cfg.Session != nil {
		sessKey = session.ExtractKey(body)
	}

	result, status, ferr := h.forwardChat(body, peek.Stream, sessKey)
	if ferr != nil {
		st.status = status
		st.uid = result.UID
		writeOpenAIError(w, status, "no_healthy_account", errText(ferr))
		return
	}
	st.uid = result.UID

	if result.Stream != nil {
		st.status = http.StatusOK
		stats := newChatStatsReaderSince(result.Stream, st.start)
		_ = upstream.Stream(w, stats)
		st.ttfb = stats.TTFB()
		st.toks, _ = stats.Tokens()
		result.Stream.Close()
		h.release(result.UID)
		return
	}

	writeJSON(w, http.StatusOK, result.Response)
	st.status = http.StatusOK
	st.toks = completionTokens(result.Response)
}

// applyErrorPolicy 按错误分类对账号施加冷却/禁用/熔断策略（最终版状态机）。
// kind 是唯一权威分类（来自 upstream.Classify），此处不再按原始 status 二次判断。
// 仅在 chatCompletions 轮转循环内调用：调用方已准备好 lastErr 并打算 continue 换号。
//
// 五条路径，各司其职：
//   - ErrHardCredit → CooldownUntilTomorrow4AM：即时硬冷却到次日 04:00（等签到恢复）。
//   - ErrSoftRate / ErrNotFound → Cooldown(CoolSoft)：即时软冷却（429/404）。
//   - ErrSessionDead → Disable：session 死亡，永久禁用（需人工重登）。
//   - ErrServer → NoteError：喂单一连续失败计数器 fails + 累计错误 errTotal，
//     达到 breakerThreshold 触发熔断（指数退避）。
//   - 其他（default：ErrClient/ErrNone）→ 只换号不罚（防雪崩），不喂熔断。
//
// 恢复出口：CoolSoft/CoolHard 各自到期自动恢复；熔断按其指数退避截止到期；
// 成功（NoteSuccess）清 fails/熔断；签到解冻（ReenableIfCredits→reviveCoolingLocked）只清冷却，不动熔断。
func (h *Handler) applyErrorPolicy(uid string, kind upstream.ErrKind) {
	switch kind {
	case upstream.ErrHardCredit:
		// 402 + 余额关键词即积分耗尽：同步冷却到次日 04:00（签到任务 09/21 点恢复），
		// 不需要异步核查（冗余）。立即换号。
		h.cfg.Pool.CooldownUntilTomorrow4AM(uid, "余额不足")
	case upstream.ErrSoftRate:
		h.cfg.Pool.Cooldown(uid, pool.CoolSoft, h.cfg.SoftCooldown, "429 rate limit")
	case upstream.ErrSessionDead:
		h.cfg.Pool.Disable(uid, "12153 session dead")
	case upstream.ErrNotFound:
		// 404 短冷却（软冷却），防雪崩。
		h.cfg.Pool.Cooldown(uid, pool.CoolSoft, h.cfg.SoftCooldown, "upstream 404")
	case upstream.ErrServer:
		// 5xx 上游故障：Classify 已把 ≥500 判为 ErrServer，在此喂熔断计数（不再手写 status>=500）。
		h.cfg.Pool.NoteError(uid)
	default:
		// 其余（ErrClient/ErrNone）：只换号不罚（防雪崩），不喂熔断。
	}
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const maxRequestBody = 8 << 20

func readLimitedBody(r *http.Request) ([]byte, error) {
	body, err := io.ReadAll(io.LimitReader(r.Body, maxRequestBody))
	if err != nil {
		return nil, err
	}
	if len(body) == 0 {
		return nil, errors.New("empty request body")
	}
	return body, nil
}

var nowFunc = time.Now

func jsonUnmarshal(s string, v any) error {
	return json.Unmarshal([]byte(s), v)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	raw, _ := json.Marshal(v)
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(raw)
}

func writeOpenAIError(w http.ResponseWriter, status int, code, msg string) {
	writeJSON(w, status, map[string]any{
		"error": map[string]any{
			"message": msg,
			"type":    "api_error",
			"code":    code,
		},
	})
}
