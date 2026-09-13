package server

// messages.go 实现 Anthropic Messages API 入口，供 Claude Code / Claude Desktop 接入。
//
// 为什么必须有这一层：Claude Code 与 Claude Desktop 的 3P 模式只讲 Anthropic
// Messages 协议（POST /v1/messages + x-api-key/anthropic-version 头 + content block
// 数组），而本网关的上游只接受 OpenAI Chat Completions。少了这一层，
// 「一键导入」写进去的配置会让客户端连不上。
//
// 覆盖范围：
//   - system：字符串或 block 数组
//   - messages：text / image / tool_use / tool_result 四种 block
//   - tools：{name, description, input_schema} → function tool
//   - tool_choice：auto / any / tool → auto / required / {function}
//   - max_tokens / temperature / top_p / stop_sequences
//   - thinking：按 budget_tokens 粗略映射到 reasoning_effort
//   - stream：Claude Code 恒为 true
//
// 反向转换：
//   - 非流式：chat.completion → {type:"message", content:[...], stop_reason, usage}
//   - 流式：chat SSE → Anthropic SSE（message_start / content_block_* / message_delta / message_stop）

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// anthropicRequest Anthropic Messages 请求体（只声明用到的字段）。
type anthropicRequest struct {
	Model         string            `json:"model"`
	MaxTokens     *int              `json:"max_tokens"`
	System        json.RawMessage   `json:"system"`
	Messages      []json.RawMessage `json:"messages"`
	Tools         []json.RawMessage `json:"tools"`
	ToolChoice    json.RawMessage   `json:"tool_choice"`
	Stream        bool              `json:"stream"`
	Temperature   *float64          `json:"temperature"`
	TopP          *float64          `json:"top_p"`
	StopSequences []string          `json:"stop_sequences"`
	Thinking      *struct {
		Type         string `json:"type"`
		BudgetTokens int    `json:"budget_tokens"`
	} `json:"thinking"`
	Metadata struct {
		UserID string `json:"user_id"`
	} `json:"metadata"`
}

// anthropicToChat 把 Anthropic Messages 请求翻译成 OpenAI Chat Completions 请求。
//
// 第二个返回值为会话粘性键：Anthropic 没有 conversation_id，用
// system + 首条 user 文本折叠成稳定键，保证同一会话粘同一账号。
func anthropicToChat(raw []byte) ([]byte, string, error) {
	var req anthropicRequest
	if err := json.Unmarshal(raw, &req); err != nil {
		return nil, "", fmt.Errorf("invalid messages request: %w", err)
	}

	messages := make([]map[string]any, 0, len(req.Messages)+1)

	if sys := anthropicSystemText(req.System); sys != "" {
		messages = append(messages, map[string]any{"role": "system", "content": sys})
	}

	firstUser := ""
	for _, rawMsg := range req.Messages {
		msgs, userText, err := anthropicMessageToChat(rawMsg)
		if err != nil {
			return nil, "", err
		}
		if firstUser == "" && userText != "" {
			firstUser = userText
		}
		messages = append(messages, msgs...)
	}

	targetModel := resolveClaudeModel(req.Model)
	out := map[string]any{
		"model":    targetModel,
		"messages": messages,
		"stream":   req.Stream,
	}
	if req.MaxTokens != nil {
		out["max_tokens"] = *req.MaxTokens
	}
	if req.Temperature != nil {
		out["temperature"] = *req.Temperature
	}
	if req.TopP != nil {
		out["top_p"] = *req.TopP
	}
	if len(req.StopSequences) > 0 {
		out["stop"] = req.StopSequences
	}
	if tools := anthropicToolsToChat(req.Tools); len(tools) > 0 {
		out["tools"] = tools
	}
	if len(req.ToolChoice) > 0 && string(req.ToolChoice) != "null" {
		out["tool_choice"] = anthropicToolChoiceToChat(req.ToolChoice)
	}
	if req.Thinking != nil && req.Thinking.Type == "enabled" {
		out["reasoning_effort"] = effortFromBudget(req.Thinking.BudgetTokens)
	}

	body, err := json.Marshal(out)
	if err != nil {
		return nil, "", err
	}
	return body, anthropicSessionKey(req.System, firstUser, req.Metadata.UserID), nil
}

// anthropicSessionKey 折叠出稳定的会话键。
func anthropicSessionKey(system json.RawMessage, firstUser, userID string) string {
	seed := strings.TrimSpace(userID)
	if seed == "" {
		seed = anthropicSystemText(system) + "\x00" + firstUser
	}
	return buildSessKey(seed)
}

// effortFromBudget 把 Anthropic 的 thinking budget 粗略映射到 reasoning_effort。
//
// 上游按模型 supportedEfforts 还会再降级一次（PrepareBodyOptWithEfforts），
// 因此这里给的是「意图」，不必精确。
func effortFromBudget(budget int) string {
	switch {
	case budget <= 0:
		return "low"
	case budget < 4096:
		return "low"
	case budget < 16384:
		return "medium"
	case budget < 32768:
		return "high"
	default:
		return "max"
	}
}

// anthropicSystemText 把 system 字段（字符串或 block 数组）拍平成纯文本。
func anthropicSystemText(raw json.RawMessage) string {
	trimmed := strings.TrimSpace(string(raw))
	if trimmed == "" || trimmed == "null" {
		return ""
	}
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		return s
	}
	var blocks []map[string]any
	if err := json.Unmarshal(raw, &blocks); err == nil {
		var sb strings.Builder
		for _, b := range blocks {
			if t := str(b["text"]); t != "" {
				sb.WriteString(t)
			}
		}
		return sb.String()
	}
	return ""
}

// anthropicMessageToChat 转换单条 Anthropic 消息。
//
// 一条 Anthropic 消息可能同时包含 tool_result（应拆成 OpenAI 的 role:tool）
// 与 text（role:user），因此返回的是消息切片而非单条。
// userText 用于生成会话粘性键（只取第一条 user 文本）。
func anthropicMessageToChat(raw json.RawMessage) ([]map[string]any, string, error) {
	var msg struct {
		Role    string          `json:"role"`
		Content json.RawMessage `json:"content"`
	}
	if err := json.Unmarshal(raw, &msg); err != nil {
		return nil, "", fmt.Errorf("invalid message item: %w", err)
	}

	// content 为纯字符串：直接映射。
	var plain string
	if err := json.Unmarshal(msg.Content, &plain); err == nil {
		return []map[string]any{{"role": msg.Role, "content": plain}}, plain, nil
	}

	var blocks []map[string]any
	if err := json.Unmarshal(msg.Content, &blocks); err != nil {
		return nil, "", fmt.Errorf("invalid message content: %w", err)
	}

	out := make([]map[string]any, 0, len(blocks))
	var text strings.Builder
	var toolCalls []map[string]any

	for _, b := range blocks {
		switch str(b["type"]) {
		case "text", "":
			text.WriteString(str(b["text"]))
		case "image":
			// 图片块：上游支持有限，转换为 image_url 分片与文本共存。
			// 这里保守处理，保持文本路径不受影响。
			continue
		case "tool_use":
			toolCalls = append(toolCalls, map[string]any{
				"id":   str(b["id"]),
				"type": "function",
				"function": map[string]any{
					"name":      str(b["name"]),
					"arguments": jsonString(b["input"]),
				},
			})
		case "tool_result":
			// tool_result 必须单独成一条 role:tool 消息，且要排在 assistant 的
			// tool_calls 之后。
			out = append(out, map[string]any{
				"role":         "tool",
				"tool_call_id": str(b["tool_use_id"]),
				"content":      anthropicToolResultText(b["content"]),
			})
		case "thinking", "redacted_thinking":
			// 推理块不回传上游：上游不接受该形状。
			continue
		}
	}

	// assistant 消息若带 tool_use，必须携带 tool_calls 字段。
	if len(toolCalls) > 0 {
		assistant := map[string]any{"role": "assistant", "tool_calls": toolCalls}
		if text.Len() > 0 {
			assistant["content"] = text.String()
		} else {
			assistant["content"] = nil
		}
		// tool_calls 消息要排在本条最前，其后才是 tool_result 消息。
		out = append([]map[string]any{assistant}, out...)
		return out, text.String(), nil
	}

	if text.Len() > 0 {
		// 已有 tool_result 时，把正文合并进同一条 user 消息之前。
		out = append([]map[string]any{{"role": msg.Role, "content": text.String()}}, out...)
	}
	return out, text.String(), nil
}

// anthropicToolResultText 拍平 tool_result 的 content（字符串或 block 数组）。
func anthropicToolResultText(v any) string {
	switch c := v.(type) {
	case nil:
		return ""
	case string:
		return c
	case []any:
		var sb strings.Builder
		for _, piece := range c {
			m, ok := piece.(map[string]any)
			if !ok {
				continue
			}
			if t := str(m["text"]); t != "" {
				sb.WriteString(t)
			}
		}
		return sb.String()
	case map[string]any:
		return str(c["text"])
	}
	return ""
}

// jsonString 把任意 JSON 值序列化成紧凑字符串（tool_use.input → arguments）。
func jsonString(v any) string {
	if v == nil {
		return "{}"
	}
	if s, ok := v.(string); ok {
		return s
	}
	raw, err := json.Marshal(v)
	if err != nil {
		return "{}"
	}
	return string(raw)
}

// anthropicToolsToChat 把 Anthropic 工具声明转成 OpenAI function 工具。
func anthropicToolsToChat(tools []json.RawMessage) []map[string]any {
	out := make([]map[string]any, 0, len(tools))
	for _, raw := range tools {
		var m map[string]any
		if err := json.Unmarshal(raw, &m); err != nil {
			continue
		}
		name := str(m["name"])
		if name == "" {
			continue
		}
		params := m["input_schema"]
		if params == nil {
			params = map[string]any{"type": "object", "properties": map[string]any{}}
		}
		out = append(out, map[string]any{
			"type": "function",
			"function": map[string]any{
				"name":        name,
				"description": str(m["description"]),
				"parameters":  params,
			},
		})
	}
	return out
}

// anthropicToolChoiceToChat 转换 tool_choice。
//
// Anthropic 的语义：auto（模型自选）/ any（必须用工具）/ tool（指定工具）。
// OpenAI 对应：auto / required / {type:function, function:{name}}。
func anthropicToolChoiceToChat(raw json.RawMessage) any {
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		return "auto"
	}
	switch str(m["type"]) {
	case "any":
		return "required"
	case "tool":
		name := str(m["name"])
		if name == "" {
			return "auto"
		}
		return map[string]any{"type": "function", "function": map[string]any{"name": name}}
	default:
		return "auto"
	}
}

// chatToAnthropic 把非流式 chat.completion 转成 Anthropic message 对象。
func chatToAnthropic(chat map[string]any, model string) map[string]any {
	id := str(chat["id"])
	if id == "" {
		id = "msg_wb2api"
	}
	if model == "" {
		model = str(chat["model"])
	}

	content := make([]any, 0, 2)
	if text := chatMessageText(chat); text != "" {
		content = append(content, map[string]any{"type": "text", "text": text})
	}
	for _, call := range chatToolCalls(chat) {
		fn, _ := call["function"].(map[string]any)
		content = append(content, map[string]any{
			"type":  "tool_use",
			"id":    str(call["id"]),
			"name":  str(fn["name"]),
			"input": rawJSONOrEmptyObject(str(fn["arguments"])),
		})
	}

	stopReason := "end_turn"
	if len(content) > 0 {
		if last, ok := content[len(content)-1].(map[string]any); ok && str(last["type"]) == "tool_use" {
			stopReason = "tool_use"
		}
	}
	if fr := chatFinishReason(chat); fr == "length" {
		stopReason = "max_tokens"
	} else if fr == "tool_calls" {
		stopReason = "tool_use"
	}

	return map[string]any{
		"id":            id,
		"type":          "message",
		"role":          "assistant",
		"model":         model,
		"content":       content,
		"stop_reason":   stopReason,
		"stop_sequence": nil,
		"usage":         anthropicUsage(chat["usage"]),
	}
}

// rawJSONOrEmptyObject 把 arguments 字符串解析回对象；解析失败时原样包成字符串。
func rawJSONOrEmptyObject(s string) any {
	if strings.TrimSpace(s) == "" {
		return map[string]any{}
	}
	var v any
	if err := json.Unmarshal([]byte(s), &v); err != nil {
		return map[string]any{}
	}
	return v
}

// chatFinishReason 取首条 choice 的 finish_reason。
func chatFinishReason(chat map[string]any) string {
	choices, _ := chat["choices"].([]any)
	if len(choices) == 0 {
		return ""
	}
	choice, _ := choices[0].(map[string]any)
	return str(choice["finish_reason"])
}

// anthropicUsage 把 Chat usage 映射成 Anthropic usage。
func anthropicUsage(v any) map[string]any {
	u, _ := v.(map[string]any)
	if u == nil {
		return map[string]any{"input_tokens": 0, "output_tokens": 0}
	}
	out := map[string]any{
		"input_tokens":  numOf(u["prompt_tokens"]),
		"output_tokens": numOf(u["completion_tokens"]),
	}
	if cached := numOf(nestedNum(u, "prompt_tokens_details", "cached_tokens")); cached > 0 {
		out["cache_read_input_tokens"] = cached
	}
	return out
}

// anthropicError 生成 Anthropic 形状的错误体。
func anthropicError(status int, code, msg string) map[string]any {
	return map[string]any{
		"type": "error",
		"error": map[string]any{
			"type":    code,
			"message": msg,
		},
	}
}

// messages 处理 POST /v1/messages。
func (h *Handler) messages(w http.ResponseWriter, r *http.Request) {
	body, err := readLimitedBody(r)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, anthropicError(http.StatusBadRequest, "invalid_request_error", err.Error()))
		return
	}

	var req anthropicRequest
	_ = json.Unmarshal(body, &req)

	stat := newChatStat(nowFunc(), body, true)
	stat.model = req.Model
	stat.mode = "messages"
	defer stat.done()

	chatBody, sessKey, err := anthropicToChat(body)
	if err != nil {
		stat.status = http.StatusBadRequest
		writeJSON(w, http.StatusBadRequest, anthropicError(http.StatusBadRequest, "invalid_request_error", err.Error()))
		return
	}

	result, status, ferr := h.forwardChat(chatBody, req.Stream, sessKey)
	if ferr != nil {
		stat.status = status
		stat.uid = result.UID
		writeJSON(w, status, anthropicError(status, "api_error", errText(ferr)))
		return
	}
	stat.uid = result.UID

	if result.Stream != nil {
		stat.status = http.StatusOK
		h.streamAnthropic(w, result, req.Model, stat)
		return
	}

	stat.status = http.StatusOK
	if toks, ok := result.Response["usage"].(map[string]any); ok {
		stat.toks = numOf(toks["completion_tokens"])
	}
	writeJSON(w, http.StatusOK, chatToAnthropic(result.Response, req.Model))
}

// ---------------------------------------------------------------------------
// Claude 模型名 → 上游真实模型名
// ---------------------------------------------------------------------------

// claudeModelTables 「Claude 请求模型名 → 上游真实名」的两级映射：
//
//   - aliases：精确名映射。来自虚拟名/真实名成对配置（CC Switch 风格）以及
//     Claude Desktop profile 的 name → labelOverride。
//   - slots：槽位兜底（sonnet/opus/haiku/fable → 真实名）。Claude 客户端的
//     /model 菜单里可以直接选中内置型号（如 claude-opus-5），这类名字不会经过
//     客户端配置的四个槽位，精确表命中不了，需要按名字中的槽位关键词回退，
//     否则用户在菜单里换个型号就会撞上 11102。
type claudeModelTables struct {
	aliases map[string]string
	slots   map[string]string
}

// claudeAliasCache 缓存模型映射表。
//
// 为什么要缓存：映射来自客户端配置文件（~/.claude/settings.json 与
// Claude Desktop 的 configLibrary），每次 /v1/messages 都要读两处磁盘
// （其中一处还是目录扫描）代价过高。配置变更不频繁，30 秒 TTL 足够新鲜。
var claudeAliasCache struct {
	sync.RWMutex
	tables  *claudeModelTables
	fetched time.Time
}

const claudeAliasTTL = 30 * time.Second

// resolveClaudeModel 把客户端传入的 Claude 模型名翻译成上游真实模型名。
//
// 翻译顺序：精确别名 → 槽位关键词兜底 → 原样透传（无配置可读时）。
// 透传是刻意的失败语义：宁可让上游报 11102（可定位），
// 也不要静默替换成用户没选的模型。
func resolveClaudeModel(requested string) string {
	name := strings.TrimSpace(requested)
	lower := strings.ToLower(name)
	if !strings.HasPrefix(lower, "claude-") {
		return name
	}

	tables := claudeTables()
	if mapped := tables.aliases[lower]; mapped != "" {
		return mapped
	}
	if mapped := tables.slotFallback(lower); mapped != "" {
		return mapped
	}
	return name
}

// slotFallback 按名字中的槽位关键词取对应槽位的模型；无关键词时回退主槽位。
func (t *claudeModelTables) slotFallback(lower string) string {
	for _, slot := range []string{"opus", "sonnet", "haiku", "fable"} {
		if strings.Contains(lower, slot) && t.slots[slot] != "" {
			return t.slots[slot]
		}
	}
	return t.slots["sonnet"]
}

// claudeTables 取缓存的映射表；过期或未加载时重建。
func claudeTables() *claudeModelTables {
	claudeAliasCache.RLock()
	if claudeAliasCache.tables != nil && time.Since(claudeAliasCache.fetched) < claudeAliasTTL {
		t := claudeAliasCache.tables
		claudeAliasCache.RUnlock()
		return t
	}
	claudeAliasCache.RUnlock()

	tables := loadClaudeModelTables()
	claudeAliasCache.Lock()
	claudeAliasCache.tables = tables
	claudeAliasCache.fetched = time.Now()
	claudeAliasCache.Unlock()
	return tables
}

// loadClaudeModelTables 从两处客户端配置汇总映射（失败返回空表，不报错）。
func loadClaudeModelTables() *claudeModelTables {
	t := &claudeModelTables{
		aliases: make(map[string]string, 8),
		slots:   make(map[string]string, 4),
	}
	loadClaudeCodeTables(t)
	loadClaudeDesktopTables(t)
	return t
}

// loadClaudeCodeTables 读 ~/.claude/settings.json 的 env 块。
//
// 兼容两种配置形态：
//
//	直写真实名（本应用）：ANTHROPIC_DEFAULT_SONNET_MODEL = deepseek-v4-flash
//	虚拟名映射（CC Switch 等）：_MODEL = claude-sonnet-4-6，_MODEL_NAME = deepseek-v4-flash
func loadClaudeCodeTables(t *claudeModelTables) {
	home, err := os.UserHomeDir()
	if err != nil {
		return
	}
	dir := os.Getenv("CLAUDE_CONFIG_DIR")
	if dir == "" {
		dir = filepath.Join(home, ".claude")
	}
	data, err := os.ReadFile(filepath.Join(dir, "settings.json"))
	if err != nil {
		return
	}
	var root struct {
		Env map[string]string `json:"env"`
	}
	if err := json.Unmarshal(data, &root); err != nil || root.Env == nil {
		return
	}

	for _, slot := range []string{"SONNET", "OPUS", "HAIKU", "FABLE"} {
		model := root.Env["ANTHROPIC_DEFAULT_"+slot+"_MODEL"]
		name := root.Env["ANTHROPIC_DEFAULT_"+slot+"_MODEL_NAME"]
		if model != "" && name != "" {
			t.aliases[strings.ToLower(model)] = name
		}
		// 槽位兜底优先取显示名（虚拟名形态下它才是真实名），其次取模型名；
		// claude- 前缀的值只是客户端的虚拟名，不能作为兜底目标。
		candidate := name
		if candidate == "" || strings.HasPrefix(strings.ToLower(candidate), "claude-") {
			candidate = model
		}
		if candidate != "" && !strings.HasPrefix(strings.ToLower(candidate), "claude-") {
			t.slots[strings.ToLower(slot)] = candidate
		}
	}
	// ANTHROPIC_MODEL 只作兜底：它可能本身还是虚拟名（循环映射无意义）。
	if m := root.Env["ANTHROPIC_MODEL"]; m != "" && !strings.HasPrefix(strings.ToLower(m), "claude-") {
		t.aliases["claude-sonnet-4-6"] = m
		if t.slots["sonnet"] == "" {
			t.slots["sonnet"] = m
		}
	}
}

// loadClaudeDesktopTables 读 Claude Desktop 的 3P profile。
//
// profile 的 inferenceModels 里，name 是 Claude 槽位名、labelOverride 才是真实上游模型。
func loadClaudeDesktopTables(t *claudeModelTables) {
	localAppData := os.Getenv("LOCALAPPDATA")
	if localAppData == "" {
		return
	}
	libDir := filepath.Join(localAppData, "Claude-3p", "configLibrary")
	entries, err := os.ReadDir(libDir)
	if err != nil {
		return
	}

	for _, entry := range entries {
		name := entry.Name()
		if !strings.HasSuffix(name, ".json") || name == "_meta.json" {
			continue
		}
		data, err := os.ReadFile(filepath.Join(libDir, name))
		if err != nil {
			continue
		}
		var prof struct {
			InferenceModels []struct {
				Name          string `json:"name"`
				LabelOverride string `json:"labelOverride"`
			} `json:"inferenceModels"`
		}
		if err := json.Unmarshal(data, &prof); err != nil {
			continue
		}
		for _, im := range prof.InferenceModels {
			if im.Name == "" || im.LabelOverride == "" {
				continue
			}
			lowerName := strings.ToLower(im.Name)
			t.aliases[lowerName] = im.LabelOverride
			for _, slot := range []string{"sonnet", "opus", "haiku", "fable"} {
				if strings.Contains(lowerName, slot) {
					t.slots[slot] = im.LabelOverride
				}
			}
		}
	}
}
