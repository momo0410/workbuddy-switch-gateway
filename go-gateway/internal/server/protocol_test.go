package server

// protocol_test.go 覆盖三种协议入口的转换正确性与鉴权行为。
//
// 这些测试不依赖真实上游：只验证「请求翻译」「响应翻译」「路由注册」，
// 真实转发由 handler_test.go / forward 相关测试覆盖。

import (
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// Anthropic Messages → Chat
// ---------------------------------------------------------------------------

func TestAnthropicToChatBasic(t *testing.T) {
	raw := []byte(`{
		"model":"glm-5.2",
		"max_tokens":1024,
		"system":"you are helpful",
		"messages":[{"role":"user","content":"hi"}],
		"stream":false
	}`)

	body, sessKey, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}

	var out map[string]any
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("unmarshal chat body: %v", err)
	}
	if out["model"] != "glm-5.2" {
		t.Errorf("model = %v", out["model"])
	}
	if out["max_tokens"] != float64(1024) {
		t.Errorf("max_tokens = %v", out["max_tokens"])
	}

	msgs, _ := out["messages"].([]any)
	if len(msgs) != 2 {
		t.Fatalf("messages len = %d, want 2 (system + user)", len(msgs))
	}
	first, _ := msgs[0].(map[string]any)
	if first["role"] != "system" || first["content"] != "you are helpful" {
		t.Errorf("system message = %v", first)
	}
	second, _ := msgs[1].(map[string]any)
	if second["role"] != "user" || second["content"] != "hi" {
		t.Errorf("user message = %v", second)
	}
	if sessKey == "" {
		t.Error("session key should be derived from system + first user text")
	}
}

func TestAnthropicToChatBlockContent(t *testing.T) {
	raw := []byte(`{
		"model":"m",
		"system":[{"type":"text","text":"sys-a"},{"type":"text","text":"sys-b"}],
		"messages":[
			{"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":" world"}]}
		]
	}`)

	body, _, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}
	var out map[string]any
	_ = json.Unmarshal(body, &out)
	msgs, _ := out["messages"].([]any)
	first, _ := msgs[0].(map[string]any)
	if first["content"] != "sys-asys-b" {
		t.Errorf("system blocks = %v", first["content"])
	}
	second, _ := msgs[1].(map[string]any)
	if second["content"] != "hello world" {
		t.Errorf("user blocks = %v", second["content"])
	}
}

// 工具往返：assistant 的 tool_use 必须变成 tool_calls，且 tool_result 变成 role:tool。
func TestAnthropicToChatToolUseRoundTrip(t *testing.T) {
	raw := []byte(`{
		"model":"m",
		"tools":[{"name":"get_weather","description":"w","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
		"tool_choice":{"type":"any"},
		"messages":[
			{"role":"user","content":"weather?"},
			{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"SZ"}}]},
			{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"sunny"}]}]}
		]
	}`)

	body, _, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}
	var out map[string]any
	_ = json.Unmarshal(body, &out)

	if out["tool_choice"] != "required" {
		t.Errorf("tool_choice = %v, want required (from type:any)", out["tool_choice"])
	}
	tools, _ := out["tools"].([]any)
	if len(tools) != 1 {
		t.Fatalf("tools len = %d", len(tools))
	}
	tool, _ := tools[0].(map[string]any)
	fn, _ := tool["function"].(map[string]any)
	if fn["name"] != "get_weather" || fn["parameters"] == nil {
		t.Errorf("tool = %v", tool)
	}

	msgs, _ := out["messages"].([]any)
	var sawToolCall, sawToolResult bool
	for _, mi := range msgs {
		m, _ := mi.(map[string]any)
		if m["role"] == "assistant" {
			if calls, ok := m["tool_calls"].([]any); ok && len(calls) == 1 {
				sawToolCall = true
				call, _ := calls[0].(map[string]any)
				fn, _ := call["function"].(map[string]any)
				if fn["name"] != "get_weather" {
					t.Errorf("tool_call name = %v", fn["name"])
				}
				if fn["arguments"] != `{"city":"SZ"}` {
					t.Errorf("tool_call arguments = %v", fn["arguments"])
				}
			}
		}
		if m["role"] == "tool" {
			sawToolResult = true
			if m["tool_call_id"] != "toolu_1" || m["content"] != "sunny" {
				t.Errorf("tool result = %v", m)
			}
		}
	}
	if !sawToolCall {
		t.Error("assistant tool_use was not converted to tool_calls")
	}
	if !sawToolResult {
		t.Error("tool_result was not converted to role:tool")
	}
}

func TestChatToAnthropicText(t *testing.T) {
	chat := map[string]any{
		"id":      "chatcmpl-1",
		"created": float64(1700000000),
		"model":   "glm-5.2",
		"choices": []any{
			map[string]any{
				"index":         float64(0),
				"message":       map[string]any{"role": "assistant", "content": "hello!"},
				"finish_reason": "stop",
			},
		},
		"usage": map[string]any{"prompt_tokens": float64(3), "completion_tokens": float64(2)},
	}

	out := chatToAnthropic(chat, "")
	if out["type"] != "message" || out["role"] != "assistant" {
		t.Errorf("envelope = %v", out)
	}
	if out["stop_reason"] != "end_turn" {
		t.Errorf("stop_reason = %v", out["stop_reason"])
	}
	content, _ := out["content"].([]any)
	if len(content) != 1 {
		t.Fatalf("content len = %d", len(content))
	}
	block, _ := content[0].(map[string]any)
	if block["type"] != "text" || block["text"] != "hello!" {
		t.Errorf("text block = %v", block)
	}
	usage, _ := out["usage"].(map[string]any)
	if usage["input_tokens"] != 3 || usage["output_tokens"] != 2 {
		t.Errorf("usage = %v", usage)
	}
}

func TestChatToAnthropicToolUse(t *testing.T) {
	chat := map[string]any{
		"id":    "chatcmpl-2",
		"model": "m",
		"choices": []any{
			map[string]any{
				"index": float64(0),
				"message": map[string]any{
					"role":    "assistant",
					"content": nil,
					"tool_calls": []any{
						map[string]any{
							"id":   "call_1",
							"type": "function",
							"function": map[string]any{
								"name":      "do_thing",
								"arguments": `{"x":1}`,
							},
						},
					},
				},
				"finish_reason": "tool_calls",
			},
		},
	}

	out := chatToAnthropic(chat, "")
	if out["stop_reason"] != "tool_use" {
		t.Errorf("stop_reason = %v", out["stop_reason"])
	}
	content, _ := out["content"].([]any)
	if len(content) != 1 {
		t.Fatalf("content len = %d", len(content))
	}
	block, _ := content[0].(map[string]any)
	if block["type"] != "tool_use" || block["name"] != "do_thing" {
		t.Errorf("tool_use block = %v", block)
	}
	input, _ := block["input"].(map[string]any)
	if input["x"] != float64(1) {
		t.Errorf("tool_use input = %v", input)
	}
}

// ---------------------------------------------------------------------------
// OpenAI Responses → Chat
// ---------------------------------------------------------------------------

func TestResponsesToChatStringInput(t *testing.T) {
	raw := []byte(`{"model":"gpt-5-codex","input":"hello","instructions":"be brief","stream":true}`)

	body, _, err := responsesToChat(raw)
	if err != nil {
		t.Fatalf("responsesToChat: %v", err)
	}
	var out map[string]any
	_ = json.Unmarshal(body, &out)

	if out["stream"] != true {
		t.Errorf("stream = %v", out["stream"])
	}
	msgs, _ := out["messages"].([]any)
	if len(msgs) != 2 {
		t.Fatalf("messages len = %d, want 2", len(msgs))
	}
	sys, _ := msgs[0].(map[string]any)
	if sys["role"] != "system" || sys["content"] != "be brief" {
		t.Errorf("instructions → system = %v", sys)
	}
	user, _ := msgs[1].(map[string]any)
	if user["role"] != "user" || user["content"] != "hello" {
		t.Errorf("string input → user = %v", user)
	}
}

func TestResponsesToChatItemInput(t *testing.T) {
	raw := []byte(`{
		"model":"gpt-5-codex",
		"input":[
			{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
			{"type":"function_call","call_id":"call_9","name":"f","arguments":"{\"a\":1}"},
			{"type":"function_call_output","call_id":"call_9","output":"done"}
		],
		"reasoning":{"effort":"high"},
		"tools":[{"type":"function","name":"f","description":"d","parameters":{"type":"object"}}]
	}`)

	body, _, err := responsesToChat(raw)
	if err != nil {
		t.Fatalf("responsesToChat: %v", err)
	}
	var out map[string]any
	_ = json.Unmarshal(body, &out)

	if out["reasoning_effort"] != "high" {
		t.Errorf("reasoning_effort = %v", out["reasoning_effort"])
	}
	msgs, _ := out["messages"].([]any)
	if len(msgs) != 3 {
		t.Fatalf("messages len = %d, want 3", len(msgs))
	}
	if m, _ := msgs[0].(map[string]any); m["role"] != "user" || m["content"] != "hi" {
		t.Errorf("message item = %v", m)
	}
	assistant, _ := msgs[1].(map[string]any)
	if assistant["role"] != "assistant" {
		t.Errorf("function_call → assistant = %v", assistant)
	}
	calls, _ := assistant["tool_calls"].([]any)
	if len(calls) != 1 {
		t.Fatalf("tool_calls len = %d", len(calls))
	}
	toolMsg, _ := msgs[2].(map[string]any)
	if toolMsg["role"] != "tool" || toolMsg["tool_call_id"] != "call_9" || toolMsg["content"] != "done" {
		t.Errorf("function_call_output → tool = %v", toolMsg)
	}
	tools, _ := out["tools"].([]any)
	if len(tools) != 1 {
		t.Fatalf("tools len = %d", len(tools))
	}
}

func TestResponsesSessionKeyPriority(t *testing.T) {
	// metadata.conversation_id 优先于 prompt_cache_key
	raw := []byte(`{"model":"m","input":"x","metadata":{"conversation_id":"conv-1"},"prompt_cache_key":"cache-1"}`)
	_, key, err := responsesToChat(raw)
	if err != nil {
		t.Fatalf("responsesToChat: %v", err)
	}
	if key != "conv-1" {
		t.Errorf("session key = %q, want conv-1", key)
	}

	raw = []byte(`{"model":"m","input":"x","prompt_cache_key":"cache-2"}`)
	_, key, err = responsesToChat(raw)
	if err != nil {
		t.Fatalf("responsesToChat: %v", err)
	}
	if key != "cache-2" {
		t.Errorf("session key = %q, want cache-2", key)
	}
}

func TestChatToResponses(t *testing.T) {
	chat := map[string]any{
		"id":      "chatcmpl-3",
		"created": float64(1700000000),
		"model":   "gpt-5-codex",
		"choices": []any{
			map[string]any{
				"index":         float64(0),
				"message":       map[string]any{"role": "assistant", "content": "done"},
				"finish_reason": "stop",
			},
		},
		"usage": map[string]any{"prompt_tokens": float64(5), "completion_tokens": float64(7), "total_tokens": float64(12)},
	}

	out := chatToResponses(chat, "")
	if out["object"] != "response" || out["status"] != "completed" {
		t.Errorf("envelope = %v", out)
	}
	output, _ := out["output"].([]any)
	if len(output) != 1 {
		t.Fatalf("output len = %d", len(output))
	}
	item, _ := output[0].(map[string]any)
	if item["type"] != "message" || item["role"] != "assistant" {
		t.Errorf("output item = %v", item)
	}
	content, _ := item["content"].([]any)
	block, _ := content[0].(map[string]any)
	if block["type"] != "output_text" || block["text"] != "done" {
		t.Errorf("output_text block = %v", block)
	}
	usage, _ := out["usage"].(map[string]any)
	if usage["input_tokens"] != 5 || usage["output_tokens"] != 7 || usage["total_tokens"] != 12 {
		t.Errorf("usage = %v", usage)
	}
}

// ---------------------------------------------------------------------------
// 路由与鉴权
// ---------------------------------------------------------------------------

func TestProtocolRoutesRegistered(t *testing.T) {
	// 无账号池 → 三个入口都应走到业务层并返回 4xx/5xx，而不是 404（未注册）。
	h := NewHandler(Config{Pool: testPoolWith()})

	for _, route := range []string{"/v1/chat/completions", "/v1/responses", "/v1/messages"} {
		req := httptest.NewRequest("POST", route, strings.NewReader(`{"model":"m","input":"x","max_tokens":16}`))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if rec.Code == 404 {
			t.Errorf("%s is not registered (got 404, body=%s)", route, rec.Body)
		}
	}
}

func TestProtocolRoutesRequireAuth(t *testing.T) {
	h := NewHandler(Config{
		Pool:   testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		APIKey: "secret",
	})

	for _, route := range []string{"/v1/chat/completions", "/v1/responses", "/v1/messages"} {
		req := httptest.NewRequest("POST", route, strings.NewReader(`{"model":"m"}`))
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if rec.Code != 401 {
			t.Errorf("%s without key: status = %d, want 401", route, rec.Code)
		}
	}
}

// ---------------------------------------------------------------------------
// 流式事件序列
// ---------------------------------------------------------------------------

func TestResponsesStreamEventSequence(t *testing.T) {
	state := newResponsesStreamState("gpt-5-codex")
	w := httptest.NewRecorder()
	out := newSSEWriter(w)

	if err := out.write("response.created", state.createdEvent()); err != nil {
		t.Fatalf("write created: %v", err)
	}
	chunks := []string{
		`{"id":"c1","created":1700000000,"model":"gpt-5-codex","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{"content":"he"},"finish_reason":null}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{"content":"llo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}`,
	}
	for _, raw := range chunks {
		var chunk map[string]any
		if err := json.Unmarshal([]byte(raw), &chunk); err != nil {
			t.Fatalf("bad fixture: %v", err)
		}
		if err := state.consume(out, chunk); err != nil {
			t.Fatalf("consume: %v", err)
		}
	}
	_ = out.write("response.completed", state.completedEvent())

	body := w.Body.String()
	for _, want := range []string{
		"response.created",
		"response.output_item.added",
		"response.output_text.delta",
		"response.completed",
	} {
		if !strings.Contains(body, want) {
			t.Errorf("stream missing %q", want)
		}
	}
	if !strings.Contains(body, `"text":"hello"`) {
		t.Errorf("completed event should carry accumulated text, got:\n%s", body)
	}
}

func TestAnthropicStreamEventSequence(t *testing.T) {
	state := newAnthropicStreamState("glm-5.2")
	w := httptest.NewRecorder()
	out := newSSEWriter(w)

	_ = out.write("message_start", map[string]any{
		"type":    "message_start",
		"message": state.messageObject("", "in_progress"),
	})
	chunks := []string{
		`{"id":"c2","model":"glm-5.2","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}`,
		`{"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}`,
	}
	for _, raw := range chunks {
		var chunk map[string]any
		if err := json.Unmarshal([]byte(raw), &chunk); err != nil {
			t.Fatalf("bad fixture: %v", err)
		}
		if err := state.consume(out, chunk); err != nil {
			t.Fatalf("consume: %v", err)
		}
	}
	state.closeOpenBlocks(out)
	_ = out.write("message_delta", map[string]any{
		"type":  "message_delta",
		"delta": map[string]any{"stop_reason": state.stopReason(), "stop_sequence": nil},
	})
	_ = out.write("message_stop", map[string]any{"type": "message_stop"})

	body := w.Body.String()
	for _, want := range []string{
		"message_start",
		"content_block_start",
		"content_block_delta",
		"content_block_stop",
		"message_delta",
		"message_stop",
	} {
		if !strings.Contains(body, want) {
			t.Errorf("stream missing %q", want)
		}
	}
	if !strings.Contains(body, `"stop_reason":"end_turn"`) {
		t.Errorf("stop_reason should map from finish_reason stop, got:\n%s", body)
	}
}
