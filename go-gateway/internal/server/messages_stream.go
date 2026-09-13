package server

// messages_stream.go 把上游的 OpenAI Chat SSE 实时翻译成 Anthropic Messages SSE。
//
// Claude Code / Claude Desktop 对事件序列有硬要求，缺事件会直接报错：
//   message_start → content_block_start → content_block_delta* →
//   content_block_stop → message_delta → message_stop
//
// 工具调用映射为 tool_use 内容块，参数以 input_json_delta 分片下发。

import (
	"bufio"
	"io"
	"net/http"
	"sort"
	"strings"
)

// streamAnthropic 把 chat SSE 转成 Anthropic SSE 写回客户端。
func (h *Handler) streamAnthropic(w http.ResponseWriter, result *chatResult, model string, stat *chatStat) {
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
	ctx := newAnthropicStreamState(model)

	// message_start：必须在任何内容块之前发送，Claude 据此建立消息。
	if err := out.write("message_start", map[string]any{
		"type":    "message_start",
		"message": ctx.messageObject("", "in_progress"),
	}); err != nil {
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
			if err := jsonUnmarshal(payload, &chunk); err == nil {
				if werr := ctx.consume(out, chunk); werr != nil {
					return
				}
			}
		}
		if err != nil {
			break
		}
	}

	ctx.closeOpenBlocks(out)
	if ctx.usage != nil {
		stat.toks = numOf(ctx.usage["completion_tokens"])
	}
	_ = out.write("message_delta", map[string]any{
		"type": "message_delta",
		"delta": map[string]any{
			"stop_reason":   ctx.stopReason(),
			"stop_sequence": nil,
		},
		"usage": map[string]any{
			"output_tokens": numOf(ctx.usage["completion_tokens"]),
		},
	})
	_ = out.write("message_stop", map[string]any{"type": "message_stop"})
}

// anthropicStreamState 记录流式转换的累积状态。
type anthropicStreamState struct {
	messageID string
	model     string
	usage     map[string]any

	textOpen  bool
	textIndex int
	text      strings.Builder

	toolCalls map[int]*anthropicToolState
	toolOrder []int

	finishReason string
}

type anthropicToolState struct {
	id        string
	name      string
	arguments strings.Builder
	index     int
	open      bool
}

func newAnthropicStreamState(model string) *anthropicStreamState {
	return &anthropicStreamState{
		messageID: "msg_wb2api",
		model:     model,
		toolCalls: map[int]*anthropicToolState{},
		textIndex: 0,
	}
}

func (s *anthropicStreamState) messageObject(stopReason, status string) map[string]any {
	msg := map[string]any{
		"id":            s.messageID,
		"type":          "message",
		"role":          "assistant",
		"model":         s.model,
		"content":       []any{},
		"stop_reason":   nil,
		"stop_sequence": nil,
		"usage":         anthropicUsage(s.usage),
	}
	if stopReason != "" {
		msg["stop_reason"] = stopReason
	}
	_ = status
	return msg
}

// consume 处理单个 chat SSE chunk。
func (s *anthropicStreamState) consume(out *sseWriter, chunk map[string]any) error {
	if id := str(chunk["id"]); id != "" && s.messageID == "msg_wb2api" {
		s.messageID = id
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
		if fr := str(choice["finish_reason"]); fr != "" {
			s.finishReason = fr
		}
		delta, _ := choice["delta"].(map[string]any)
		if delta == nil {
			continue
		}
		if text := str(delta["content"]); text != "" {
			if err := s.openText(out); err != nil {
				return err
			}
			s.text.WriteString(text)
			if err := out.write("content_block_delta", map[string]any{
				"type":  "content_block_delta",
				"index": s.textIndex,
				"delta": map[string]any{"type": "text_delta", "text": text},
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
	return nil
}

func (s *anthropicStreamState) openText(out *sseWriter) error {
	if s.textOpen {
		return nil
	}
	s.textOpen = true
	return out.write("content_block_start", map[string]any{
		"type":  "content_block_start",
		"index": s.textIndex,
		"content_block": map[string]any{
			"type": "text",
			"text": "",
		},
	})
}

func (s *anthropicStreamState) consumeToolCall(out *sseWriter, call map[string]any) error {
	idx := numOf(call["index"])
	tc, ok := s.toolCalls[idx]
	if !ok {
		tc = &anthropicToolState{index: s.nextBlockIndex()}
		s.toolCalls[idx] = tc
		s.toolOrder = append(s.toolOrder, idx)
	}
	if id := str(call["id"]); id != "" {
		tc.id = id
	}
	fn, _ := call["function"].(map[string]any)
	if fn != nil {
		if name := str(fn["name"]); name != "" {
			tc.name = name
		}
	}

	// 拿到 name 后才能开块（Anthropic 的 content_block_start 必须带 name）。
	if !tc.open && tc.name != "" {
		tc.open = true
		if err := out.write("content_block_start", map[string]any{
			"type":  "content_block_start",
			"index": tc.index,
			"content_block": map[string]any{
				"type":  "tool_use",
				"id":    tc.id,
				"name":  tc.name,
				"input": map[string]any{},
			},
		}); err != nil {
			return err
		}
	}
	if fn != nil {
		if args := str(fn["arguments"]); args != "" {
			tc.arguments.WriteString(args)
			if err := out.write("content_block_delta", map[string]any{
				"type":  "content_block_delta",
				"index": tc.index,
				"delta": map[string]any{
					"type":         "input_json_delta",
					"partial_json": args,
				},
			}); err != nil {
				return err
			}
		}
	}
	return nil
}

func (s *anthropicStreamState) nextBlockIndex() int {
	if s.textOpen {
		return 1 + len(s.toolOrder)
	}
	return len(s.toolOrder)
}

// closeOpenBlocks 按 Anthropic 规范逐个关闭已开启的内容块。
func (s *anthropicStreamState) closeOpenBlocks(out *sseWriter) {
	if s.textOpen {
		_ = out.write("content_block_stop", map[string]any{
			"type":  "content_block_stop",
			"index": s.textIndex,
		})
	}
	order := append([]int(nil), s.toolOrder...)
	sort.Ints(order)
	for _, idx := range order {
		tc := s.toolCalls[idx]
		if tc == nil || !tc.open {
			continue
		}
		_ = out.write("content_block_stop", map[string]any{
			"type":  "content_block_stop",
			"index": tc.index,
		})
	}
}

// stopReason 把 chat 的 finish_reason 映射成 Anthropic 的 stop_reason。
func (s *anthropicStreamState) stopReason() string {
	switch s.finishReason {
	case "length":
		return "max_tokens"
	case "tool_calls":
		return "tool_use"
	case "stop":
		return "end_turn"
	case "":
		if len(s.toolOrder) > 0 {
			return "tool_use"
		}
		return "end_turn"
	default:
		return "end_turn"
	}
}

var _ = io.EOF
