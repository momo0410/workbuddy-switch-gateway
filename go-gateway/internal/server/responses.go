package server

// responses.go 实现 OpenAI Responses API 入口，供 Codex CLI / ChatGPT 桌面版接入。
//
// 为什么必须有这一层：Codex 0.146 起彻底移除了 `wire_api = "chat"`，只接受
// `wire_api = "responses"`（二进制内明确写着 "`wire_api = \"chat\"` is no longer
// supported"）。因此网关若只提供 /v1/chat/completions，Codex 一侧无论怎么写配置都
// 连不上；必须在网关侧把 Responses 请求翻译成上游认识的 Chat Completions。
//
// 覆盖范围（Codex 实际会发的形状）：
//   - input：字符串，或 item 数组（message / function_call / function_call_output）
//   - instructions：等价于 system 消息
//   - tools：function 工具（含 strict / namespace 形态）
//   - tool_choice / parallel_tool_calls / reasoning.effort / text.verbosity
//   - stream：Codex 恒为 true
//
// 反向转换：
//   - 非流式：chat.completion → response 对象
//   - 流式：chat SSE → Responses SSE（response.created / output_item.added /
//     response.output_text.delta / response.completed …）

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strings"

	"workbuddy2api/internal/upstream"
)

// responsesRequest Responses API 请求体（只声明我们用到的字段）。
type responsesRequest struct {
	Model             string            `json:"model"`
	Input             json.RawMessage   `json:"input"`
	Instructions      string            `json:"instructions"`
	Tools             []json.RawMessage `json:"tools"`
	ToolChoice        json.RawMessage   `json:"tool_choice"`
	ParallelToolCalls *bool             `json:"parallel_tool_calls"`
	Stream            bool              `json:"stream"`
	Reasoning         *struct {
		Effort string `json:"effort"`
	} `json:"reasoning"`
	Text *struct {
		Verbosity string `json:"verbosity"`
	} `json:"text"`
	// 会话粘性：Codex 会带 prompt_cache_key / metadata，可直接当会话键。
	PromptCacheKey string `json:"prompt_cache_key"`
	Metadata       struct {
		ConversationID string `json:"conversation_id"`
	} `json:"metadata"`
	MaxOutputTokens *int `json:"max_output_tokens"`
	Temperature     *float64 `json:"temperature"`
	TopP            *float64 `json:"top_p"`
}

// responsesToChat 把 Responses 请求翻译成 OpenAI Chat Completions 请求。
//
// 返回的字节直接交给 forwardChat；第二个返回值是会话粘性键（可能为空）。
func responsesToChat(raw []byte) ([]byte, string, error) {
	var req responsesRequest
	if err := json.Unmarshal(raw, &req); err != nil {
		return nil, "", fmt.Errorf("invalid responses request: %w", err)
	}

	messages := make([]map[string]any, 0, 8)
	if s := strings.TrimSpace(req.Instructions); s != "" {
		messages = append(messages, map[string]any{"role": "system", "content": s})
	}

	inputMsgs, err := responsesInputToMessages(req.Input)
	if err != nil {
		return nil, "", err
	}
	messages = append(messages, inputMsgs...)

	out := map[string]any{
		"model":    req.Model,
		"messages": messages,
		"stream":   req.Stream,
	}
	if tools := responsesToolsToChat(req.Tools); len(tools) > 0 {
		out["tools"] = tools
	}
	if len(req.ToolChoice) > 0 && string(req.ToolChoice) != "null" {
		out["tool_choice"] = normalizeResponsesToolChoice(req.ToolChoice)
	}
	if req.ParallelToolCalls != nil {
		out["parallel_tool_calls"] = *req.ParallelToolCalls
	}
	if req.MaxOutputTokens != nil {
		out["max_tokens"] = *req.MaxOutputTokens
	}
	if req.Temperature != nil {
		out["temperature"] = *req.Temperature
	}
	if req.TopP != nil {
		out["top_p"] = *req.TopP
	}
	if req.Reasoning != nil && req.Reasoning.Effort != "" {
		out["reasoning_effort"] = req.Reasoning.Effort
	}

	body, err := json.Marshal(out)
	if err != nil {
		return nil, "", err
	}
	return body, responsesSessionKey(&req), nil
}

// responsesSessionKey 依次尝试 metadata.conversation_id、prompt_cache_key、
// 首条用户消息文本，任一可用即作为粘性键。
func responsesSessionKey(req *responsesRequest) string {
	if id := strings.TrimSpace(req.Metadata.ConversationID); id != "" {
		return id
	}
	if k := strings.TrimSpace(req.PromptCacheKey); k != "" {
		return k
	}
	return ""
}

// responsesInputToMessages 把 Responses 的 input 翻译成 Chat 的 messages。
//
// input 有两种形态：
//   - 纯字符串：直接当 user 消息
//   - item 数组：逐条按 type 分派
func responsesInputToMessages(input json.RawMessage) ([]map[string]any, error) {
	trimmed := strings.TrimSpace(string(input))
	if trimmed == "" || trimmed == "null" {
		return nil, nil
	}

	if strings.HasPrefix(trimmed, "\"") {
		var text string
		if err := json.Unmarshal(input, &text); err != nil {
			return nil, fmt.Errorf("invalid responses input string: %w", err)
		}
		return []map[string]any{{"role": "user", "content": text}}, nil
	}

	var items []map[string]any
	if err := json.Unmarshal(input, &items); err != nil {
		return nil, fmt.Errorf("invalid responses input items: %w", err)
	}

	out := make([]map[string]any, 0, len(items))
	for _, item := range items {
		switch str(item["type"]) {
		case "message":
			if msg, ok := responsesMessageToChat(item); ok {
				out = append(out, msg)
			}
		case "function_call":
			out = append(out, responsesFunctionCallToChat(item))
		case "function_call_output":
			out = append(out, responsesFunctionOutputToChat(item))
		case "reasoning":
			// 推理条目不回转给上游：上游不接受该形状，且内容已体现在后续 assistant 消息里。
			continue
		case "":
			// 缺 type 时按 message 兜底（部分客户端省略）。
			if msg, ok := responsesMessageToChat(item); ok {
				out = append(out, msg)
			}
		}
	}
	return out, nil
}

func responsesMessageToChat(item map[string]any) (map[string]any, bool) {
	role := str(item["role"])
	if role == "" {
		role = "user"
	}
	content, ok := responsesContentToChat(item["content"])
	if !ok {
		return nil, false
	}
	return map[string]any{"role": role, "content": content}, true
}

// responsesContentToChat 把 Responses 的 content 数组拍平成 Chat 的字符串或分片数组。
//
// Responses 的内容分片形如 {"type":"input_text","text":"..."}；
// 图片分片上游支持有限，这里保守降级成文本占位，避免整个请求被 400。
func responsesContentToChat(v any) (any, bool) {
	switch c := v.(type) {
	case nil:
		return "", true
	case string:
		return c, true
	case []any:
		var sb strings.Builder
		parts := make([]map[string]any, 0, len(c))
		textOnly := true
		for _, piece := range c {
			m, ok := piece.(map[string]any)
			if !ok {
				continue
			}
			switch str(m["type"]) {
			case "input_text", "output_text", "text", "":
				t := str(m["text"])
				sb.WriteString(t)
				parts = append(parts, map[string]any{"type": "text", "text": t})
			case "input_image":
				textOnly = false
				parts = append(parts, map[string]any{
					"type":      "image_url",
					"image_url": map[string]any{"url": str(m["image_url"])},
				})
			default:
				continue
			}
		}
		if textOnly {
			return sb.String(), true
		}
		return parts, true
	case map[string]any:
		return responsesContentToChat([]any{c})
	}
	return "", false
}

func responsesFunctionCallToChat(item map[string]any) map[string]any {
	return map[string]any{
		"role": "assistant",
		"tool_calls": []map[string]any{{
			"id":   str(item["call_id"]),
			"type": "function",
			"function": map[string]any{
				"name":      str(item["name"]),
				"arguments": str(item["arguments"]),
			},
		}},
	}
}

func responsesFunctionOutputToChat(item map[string]any) map[string]any {
	output := item["output"]
	if output == nil {
		output = ""
	}
	text, _ := responsesContentToChat(output)
	return map[string]any{
		"role":         "tool",
		"tool_call_id": str(item["call_id"]),
		"content":      text,
	}
}

// responsesToolsToChat 转换工具声明。
//
// Responses 的工具形如：
//   - {"type":"function","name":...,"description":...,"parameters":{...}}
//   - {"type":"function","function":{...}}（部分兼容实现）
//   - {"type":"namespace","name":...}（Codex 私有扩展，降级为无参函数）
func responsesToolsToChat(tools []json.RawMessage) []map[string]any {
	out := make([]map[string]any, 0, len(tools))
	for _, raw := range tools {
		var m map[string]any
		if err := json.Unmarshal(raw, &m); err != nil {
			continue
		}
		if nested, ok := m["function"].(map[string]any); ok {
			out = append(out, map[string]any{"type": "function", "function": nested})
			continue
		}
		name := str(m["name"])
		if name == "" {
			continue
		}
		params := m["parameters"]
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

// normalizeResponsesToolChoice 把 tool_choice 归一成上游接受的 string 或标准对象。
func normalizeResponsesToolChoice(raw json.RawMessage) any {
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		return s
	}
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err == nil {
		if t := str(m["type"]); t == "function" {
			name := str(m["name"])
			if name == "" {
				if fn, ok := m["function"].(map[string]any); ok {
					name = str(fn["name"])
				}
			}
			if name != "" {
				return map[string]any{"type": "function", "function": map[string]any{"name": name}}
			}
		}
		if t := str(m["type"]); t == "auto" || t == "none" || t == "required" {
			return t
		}
	}
	return "auto"
}

func str(v any) string {
	s, _ := v.(string)
	return s
}

// chatToResponses 把非流式的 OpenAI chat.completion 转成 Responses 响应对象。
func chatToResponses(chat map[string]any, model string) map[string]any {
	id := str(chat["id"])
	if id == "" {
		id = "resp_wb2api"
	}
	if model == "" {
		model = str(chat["model"])
	}

	var output []any
	text := chatMessageText(chat)
	status := "completed"

	if text != "" {
		output = append(output, map[string]any{
			"id":     "msg_" + id,
			"type":   "message",
			"role":   "assistant",
			"status": "completed",
			"content": []any{
				map[string]any{"type": "output_text", "text": text, "annotations": []any{}},
			},
		})
	}
	for _, call := range chatToolCalls(chat) {
		fn, _ := call["function"].(map[string]any)
		output = append(output, map[string]any{
			"type":      "function_call",
			"id":        str(call["id"]),
			"call_id":   str(call["id"]),
			"name":      str(fn["name"]),
			"arguments": str(fn["arguments"]),
			"status":    "completed",
		})
	}

	return map[string]any{
		"id":         id,
		"object":     "response",
		"created_at": chat["created"],
		"status":     status,
		"model":      model,
		"output":     output,
		"usage":      responsesUsage(chat["usage"]),
	}
}

// chatMessageText 取出首条 choice 的正文文本。
func chatMessageText(chat map[string]any) string {
	choices, _ := chat["choices"].([]any)
	if len(choices) == 0 {
		return ""
	}
	choice, _ := choices[0].(map[string]any)
	msg, _ := choice["message"].(map[string]any)
	if msg == nil {
		if delta, ok := choice["delta"].(map[string]any); ok {
			return str(delta["content"])
		}
		return ""
	}
	return str(msg["content"])
}

// chatToolCalls 取出首条 choice 的 tool_calls 列表。
func chatToolCalls(chat map[string]any) []map[string]any {
	choices, _ := chat["choices"].([]any)
	if len(choices) == 0 {
		return nil
	}
	choice, _ := choices[0].(map[string]any)
	msg, _ := choice["message"].(map[string]any)
	if msg == nil {
		return nil
	}
	raw, _ := msg["tool_calls"].([]any)
	out := make([]map[string]any, 0, len(raw))
	for _, item := range raw {
		if m, ok := item.(map[string]any); ok {
			out = append(out, m)
		}
	}
	return out
}

// responsesUsage 把 Chat 的 usage 映射成 Responses 的 usage 形状。
func responsesUsage(v any) map[string]any {
	u, _ := v.(map[string]any)
	if u == nil {
		return map[string]any{"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
	}
	input := numOf(u["prompt_tokens"])
	output := numOf(u["completion_tokens"])
	out := map[string]any{
		"input_tokens":  input,
		"output_tokens": output,
		"total_tokens":  numOf(u["total_tokens"]),
	}
	if cached := numOf(nestedNum(u, "prompt_tokens_details", "cached_tokens")); cached > 0 {
		out["input_tokens_details"] = map[string]any{"cached_tokens": cached}
	}
	if reasoning := numOf(nestedNum(u, "completion_tokens_details", "reasoning_tokens")); reasoning > 0 {
		out["output_tokens_details"] = map[string]any{"reasoning_tokens": reasoning}
	}
	return out
}

func nestedNum(m map[string]any, outer, inner string) any {
	nested, _ := m[outer].(map[string]any)
	if nested == nil {
		return nil
	}
	return nested[inner]
}

func numOf(v any) int {
	switch n := v.(type) {
	case float64:
		return int(n)
	case int:
		return n
	case int64:
		return int(n)
	}
	return 0
}

// responsesError 生成 Responses 形状的错误体。
func responsesError(status int, code, msg string) map[string]any {
	return map[string]any{
		"error": map[string]any{
			"message": msg,
			"type":    "api_error",
			"code":    code,
		},
	}
}

// responses 处理 POST /v1/responses。
func (h *Handler) responses(w http.ResponseWriter, r *http.Request) {
	body, err := readLimitedBody(r)
	if err != nil {
		writeJSON(w, http.StatusBadRequest, responsesError(http.StatusBadRequest, "invalid_request", err.Error()))
		return
	}

	stat := newChatStat(nowFunc(), body, true)
	defer stat.done()

	chatBody, sessKey, err := responsesToChat(body)
	if err != nil {
		stat.status = http.StatusBadRequest
		writeJSON(w, http.StatusBadRequest, responsesError(http.StatusBadRequest, "invalid_request", err.Error()))
		return
	}

	var req responsesRequest
	_ = json.Unmarshal(body, &req)
	stat.model = req.Model
	stat.mode = "responses"

	result, status, ferr := h.forwardChat(chatBody, req.Stream, sessKey)
	if ferr != nil {
		stat.status = status
		stat.uid = result.UID
		writeJSON(w, status, responsesError(status, "upstream_error", errText(ferr)))
		return
	}
	stat.uid = result.UID

	if result.Stream != nil {
		stat.status = http.StatusOK
		h.streamResponses(w, result, req.Model, stat)
		return
	}

	stat.status = http.StatusOK
	if toks, ok := result.Response["usage"].(map[string]any); ok {
		if n, ok2 := toks["completion_tokens"].(float64); ok2 {
			stat.toks = int(n)
		}
	}
	writeJSON(w, http.StatusOK, chatToResponses(result.Response, req.Model))
}

func errText(err error) string {
	if err == nil {
		return "unknown error"
	}
	return err.Error()
}

var _ = upstream.ErrNone
