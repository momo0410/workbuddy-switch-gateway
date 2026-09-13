package server

// responses_stream.go 把上游的 OpenAI Chat SSE 实时翻译成 Responses SSE。
//
// Codex 恒以 stream:true 调用 /v1/responses，且对事件序列有硬性要求：
// 必须先有 response.created，随后是 output_item.added / output_text.delta，
// 结束时以 response.completed 收尾并携带 usage。缺事件会导致 Codex 报
// "stream closed before response.completed"。
//
// 本实现逐帧转换、不缓冲整个响应，因此首字节延迟与原生 chat 透传一致。

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
)

// sseWriter 统一的 Responses SSE 写帧器（带 flush）。
type sseWriter struct {
	w  http.ResponseWriter
	fl http.Flusher
}

func newSSEWriter(w http.ResponseWriter) *sseWriter {
	fl, _ := w.(http.Flusher)
	return &sseWriter{w: w, fl: fl}
}

func (s *sseWriter) write(event string, payload map[string]any) error {
	raw, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	if event != "" {
		if _, err := io.WriteString(s.w, "event: "+event+"\n"); err != nil {
			return err
		}
	}
	if _, err := io.WriteString(s.w, "data: "+string(raw)+"\n\n"); err != nil {
		return err
	}
	if s.fl != nil {
		s.fl.Flush()
	}
	return nil
}

// streamResponses 把 chat SSE 转成 Responses SSE 写回客户端。
func (h *Handler) streamResponses(w http.ResponseWriter, result *chatResult, model string, stat *chatStat) {
	defer func() {
		if result.Stream != nil {
			result.Stream.Close()
		}
		h.release(result.UID)
	}()

	hdr := w.Header()
	hdr.Set("Content-Type", "text/event-stream")
	hdr.Set("Cache-Control", "no-cache")
	hdr.Set("Connection", "keep-alive")
	hdr.Set("X-Accel-Buffering", "no")

	out := newSSEWriter(w)
	ctx := newResponsesStreamState(model)

	if err := out.write("response.created", ctx.createdEvent()); err != nil {
		return
	}

	br := bufio.NewReaderSize(result.Stream, 64*1024)
	for {
		line, err := br.ReadString('\n')
		line = strings.TrimRight(line, "\r\n")
		if payload, ok := strings.CutPrefix(line, "data: "); ok {
			if payload == "[DONE]" {
				break
			}
			var chunk map[string]any
			if json.Unmarshal([]byte(payload), &chunk) == nil {
				if werr := ctx.consume(out, chunk); werr != nil {
					return
				}
			}
		}
		if err != nil {
			if err != io.EOF {
				_ = out.write("response.failed", ctx.failedEvent("upstream stream error: "+err.Error()))
			}
			break
		}
	}

	if !ctx.textStarted {
		if err := ctx.startTextItem(out); err != nil {
			return
		}
	}
	_ = out.write("response.output_text.done", ctx.textDoneEvent())
	// Codex 0.146 仅在 response.output_item.done 时把输出项收进会话状态
	//（不消费 output_item.added / output_text.done），缺它会导致
	// last_agent_message 为空、终端不显示回复。每个输出项都必须补一个 done。
	_ = ctx.finishItems(out)
	if ctx.usage != nil {
		stat.toks = numOf(ctx.usage["completion_tokens"])
	}
	_ = out.write("response.completed", ctx.completedEvent())
}

// responsesStreamState 记录流式转换过程中的累积状态。
type responsesStreamState struct {
	responseID  string
	model       string
	createdAt   int64
	text        strings.Builder
	textStarted bool
	textItemID  string
	sequence    int
	toolCalls   map[int]*responsesToolCall
	toolOrder   []int
	usage       map[string]any
	finished    bool
}

type responsesToolCall struct {
	id        string
	name      string
	arguments strings.Builder
	added     bool
}

func newResponsesStreamState(model string) *responsesStreamState {
	return &responsesStreamState{
		responseID: "resp_wb2api",
		model:      model,
		toolCalls:  map[int]*responsesToolCall{},
	}
}

func (s *responsesStreamState) seq() int {
	n := s.sequence
	s.sequence++
	return n
}

func (s *responsesStreamState) createdEvent() map[string]any {
	return map[string]any{
		"type":            "response.created",
		"sequence_number": s.seq(),
		"response":        s.snapshot("in_progress", nil),
	}
}

func (s *responsesStreamState) snapshot(status string, output []any) map[string]any {
	if output == nil {
		output = []any{}
	}
	return map[string]any{
		"id":         s.responseID,
		"object":     "response",
		"created_at": s.createdAt,
		"status":     status,
		"model":      s.model,
		"output":     output,
	}
}

func (s *responsesStreamState) startTextItem(out *sseWriter) error {
	if s.textStarted {
		return nil
	}
	s.textStarted = true
	s.textItemID = "msg_" + s.responseID
	return out.write("response.output_item.added", map[string]any{
		"type":            "response.output_item.added",
		"sequence_number": s.seq(),
		"output_index":    0,
		"item": map[string]any{
			"id":      s.textItemID,
			"type":    "message",
			"role":    "assistant",
			"status":  "in_progress",
			"content": []any{},
		},
	})
}

// consume 处理单个 chat SSE chunk。
func (s *responsesStreamState) consume(out *sseWriter, chunk map[string]any) error {
	if id := str(chunk["id"]); id != "" && s.responseID == "resp_wb2api" {
		s.responseID = id
	}
	if s.createdAt == 0 {
		if created, ok := chunk["created"].(float64); ok {
			s.createdAt = int64(created)
		}
	}
	if m := str(chunk["model"]); m != "" && s.model == "" {
		s.model = m
	}
	if u, ok := chunk["usage"].(map[string]any); ok && u != nil {
		s.usage = u
	}

	choices, _ := chunk["choices"].([]any)
	for _, ci := range choices {
		choice, _ := ci.(map[string]any)
		if choice == nil {
			continue
		}
		delta, _ := choice["delta"].(map[string]any)
		if delta != nil {
			if text := str(delta["content"]); text != "" {
				if err := s.startTextItem(out); err != nil {
					return err
				}
				s.text.WriteString(text)
				if err := out.write("response.output_text.delta", map[string]any{
					"type":            "response.output_text.delta",
					"sequence_number": s.seq(),
					"item_id":         s.textItemID,
					"output_index":    0,
					"content_index":   0,
					"delta":           text,
				}); err != nil {
					return err
				}
			}
			if tcs, ok := delta["tool_calls"].([]any); ok {
				for _, tci := range tcs {
					call, _ := tci.(map[string]any)
					if call == nil {
						continue
					}
					if err := s.consumeToolCall(out, call); err != nil {
						return err
					}
				}
			}
		}
		if fr := str(choice["finish_reason"]); fr != "" {
			s.finished = true
		}
	}
	return nil
}

func (s *responsesStreamState) consumeToolCall(out *sseWriter, call map[string]any) error {
	idx := numOf(call["index"])
	tc, ok := s.toolCalls[idx]
	if !ok {
		tc = &responsesToolCall{}
		s.toolCalls[idx] = tc
		s.toolOrder = append(s.toolOrder, idx)
	}
	if id := str(call["id"]); id != "" {
		tc.id = id
	}
	if fn, ok := call["function"].(map[string]any); ok {
		if name := str(fn["name"]); name != "" {
			tc.name = name
		}
		if args := str(fn["arguments"]); args != "" {
			tc.arguments.WriteString(args)
		}
	}
	// 首个带 name/id 的分片补 output_item.added；后续分片只发 arguments.delta。
	if !tc.added && (tc.name != "" || tc.id != "") {
		tc.added = true
		outputIndex := 1 + indexOfInt(s.toolOrder, idx)
		if err := out.write("response.output_item.added", map[string]any{
			"type":            "response.output_item.added",
			"sequence_number": s.seq(),
			"output_index":    outputIndex,
			"item": map[string]any{
				"type":      "function_call",
				"id":        tc.id,
				"call_id":   tc.id,
				"name":      tc.name,
				"arguments": "",
				"status":    "in_progress",
			},
		}); err != nil {
			return err
		}
	}
	if fn, ok := call["function"].(map[string]any); ok {
		if args := str(fn["arguments"]); args != "" {
			outputIndex := 1 + indexOfInt(s.toolOrder, idx)
			if err := out.write("response.function_call_arguments.delta", map[string]any{
				"type":            "response.function_call_arguments.delta",
				"sequence_number": s.seq(),
				"item_id":         tc.id,
				"output_index":    outputIndex,
				"delta":           args,
			}); err != nil {
				return err
			}
		}
	}
	return nil
}

func (s *responsesStreamState) textDoneEvent() map[string]any {
	return map[string]any{
		"type":            "response.output_text.done",
		"sequence_number": s.seq(),
		"item_id":         s.textItemID,
		"output_index":    0,
		"content_index":   0,
		"text":            s.text.String(),
	}
}

// completedEvent 收尾事件：携带完整 output 与 usage，Codex 以此判定请求成功。
func (s *responsesStreamState) completedEvent() map[string]any {
	return map[string]any{
		"type":            "response.completed",
		"sequence_number": s.seq(),
		"response":        s.snapshot("completed", s.outputItems()),
	}
}

func (s *responsesStreamState) failedEvent(msg string) map[string]any {
	resp := s.snapshot("failed", []any{})
	resp["error"] = map[string]any{"code": "upstream_error", "message": msg}
	return map[string]any{
		"type":            "response.failed",
		"sequence_number": s.seq(),
		"response":        resp,
	}
}

// finishItems 为每个输出项补发 response.output_item.done（文本项 + 工具调用项）。
func (s *responsesStreamState) finishItems(out *sseWriter) error {
	if s.textStarted {
		if err := out.write("response.output_item.done", map[string]any{
			"type":            "response.output_item.done",
			"sequence_number": s.seq(),
			"output_index":    0,
			"item":            s.textMessageItem(),
		}); err != nil {
			return err
		}
	}
	order := append([]int(nil), s.toolOrder...)
	sort.Ints(order)
	for _, idx := range order {
		tc := s.toolCalls[idx]
		if tc == nil {
			continue
		}
		if err := out.write("response.output_item.done", map[string]any{
			"type":            "response.output_item.done",
			"sequence_number": s.seq(),
			"output_index":    1 + indexOfInt(s.toolOrder, idx),
			"item":            s.functionCallItem(tc),
		}); err != nil {
			return err
		}
	}
	return nil
}

// textMessageItem 组装完整的 assistant message 输出项。
func (s *responsesStreamState) textMessageItem() map[string]any {
	return map[string]any{
		"id":     s.textItemID,
		"type":   "message",
		"role":   "assistant",
		"status": "completed",
		"content": []any{
			map[string]any{
				"type":        "output_text",
				"text":        s.text.String(),
				"annotations": []any{},
			},
		},
	}
}

// functionCallItem 组装完整的 function_call 输出项。
func (s *responsesStreamState) functionCallItem(tc *responsesToolCall) map[string]any {
	return map[string]any{
		"type":      "function_call",
		"id":        tc.id,
		"call_id":   tc.id,
		"name":      tc.name,
		"arguments": tc.arguments.String(),
		"status":    "completed",
	}
}

func (s *responsesStreamState) outputItems() []any {
	items := make([]any, 0, 1+len(s.toolOrder))
	if s.textStarted {
		items = append(items, s.textMessageItem())
	}
	order := append([]int(nil), s.toolOrder...)
	sort.Ints(order)
	for _, idx := range order {
		tc := s.toolCalls[idx]
		if tc == nil {
			continue
		}
		items = append(items, s.functionCallItem(tc))
	}
	return items
}

func indexOfInt(list []int, v int) int {
	for i, item := range list {
		if item == v {
			return i
		}
	}
	return len(list)
}

var _ = fmt.Sprintf
