package server

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// Issue #5 回归：超限请求体必须显式报错，不能静默截断后透传给上游
//
// 现场表现（长对话）：
//
//	400 {"code":"no_healthy_account","message":"all accounts unavailable ...:
//	upstream client (http 400): {\"code\":\"11101\",\"msg\":\"Unmarshal chat params
//	failed with error: unexpected EOF\"}"}
//
// 根因：readLimitedBody 用 io.LimitReader(8MB) 读取，读满即返回。截断后的字节不是
// 合法 JSON，prepareBody 解析失败后原样透传，上游解码时报 unexpected EOF。
// 客户端只看到「请求参数有误」，完全无法定位到是网关截断。
// ---------------------------------------------------------------------------

// oversizedChatBody 构造一个超过 maxRequestBody 的合法 JSON 请求体（模拟长对话历史）。
func oversizedChatBody() string {
	pad := strings.Repeat("x", maxRequestBody)
	return fmt.Sprintf(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"%s"}]}`, pad)
}

// TestOversizedBodyRejectedNotTruncated 核心回归：超限请求体必须被拒绝（413），
// 且**不得**把截断后的坏字节发往上游。
func TestOversizedBodyRejectedNotTruncated(t *testing.T) {
	body := oversizedChatBody()
	if len(body) <= maxRequestBody {
		t.Fatalf("precondition: body=%d 必须超过上限 %d", len(body), maxRequestBody)
	}

	up := newFakeUpstream(t, func(string) (int, string, bool) {
		t.Error("超限请求体不应被转发到上游（截断体透传正是 Issue #5 的根因）")
		return 200, sseOK, true
	})
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body)))

	if rec.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("code=%d want 413；body=%s", rec.Code, truncateForLog(rec.Body.String()))
	}
	var e struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &e); err != nil {
		t.Fatalf("error envelope 解析失败: %v body=%s", err, rec.Body)
	}
	if e.Error.Code != "payload_too_large" {
		t.Errorf("code=%q want payload_too_large", e.Error.Code)
	}
	if !strings.Contains(e.Error.Message, "limit") {
		t.Errorf("message 应说明超限原因，实际=%q", e.Error.Message)
	}
}

// TestReadLimitedBodyRejectsOversize 单位级：readLimitedBody 对超限体返回
// errBodyTooLarge 哨兵，对正常体原样返回。
func TestReadLimitedBodyRejectsOversize(t *testing.T) {
	// 正好在限内：应成功。
	ok := strings.Repeat("a", 1024)
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(ok))
	got, err := readLimitedBody(req)
	if err != nil {
		t.Fatalf("限内请求体应可读: %v", err)
	}
	if len(got) != len(ok) {
		t.Errorf("读回长度=%d want %d", len(got), len(ok))
	}

	// 超限 1 字节：必须报 errBodyTooLarge（旧实现静默截断）。
	over := strings.NewReader(strings.Repeat("a", maxRequestBody+1))
	req2 := httptest.NewRequest("POST", "/v1/chat/completions", over)
	if _, err := readLimitedBody(req2); err != errBodyTooLarge {
		t.Fatalf("超限请求体应返回 errBodyTooLarge，实际=%v", err)
	}

	// 空体仍维持原有语义。
	req3 := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(""))
	if _, err := readLimitedBody(req3); err == nil || err == errBodyTooLarge {
		t.Fatalf("空体应报 invalid_request 类错误，实际=%v", err)
	}
}

// TestOversizedBodyRejectedOnAllProtocols 三种协议入口都要给出 413
// 且使用各自词汇表的错误码（Anthropic 用 request_too_large）。
func TestOversizedBodyRejectedOnAllProtocols(t *testing.T) {
	body := oversizedChatBody()
	cases := []struct {
		name     string
		path     string
		wantCode string
	}{
		{"chat/completions", "/v1/chat/completions", "payload_too_large"},
		{"responses", "/v1/responses", "payload_too_large"},
		{"messages", "/v1/messages", "request_too_large"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			h := NewHandler(Config{
				Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
				Upstream: upstream.New(),
			})
			rec := httptest.NewRecorder()
			h.ServeHTTP(rec, httptest.NewRequest("POST", c.path, strings.NewReader(body)))
			if rec.Code != http.StatusRequestEntityTooLarge {
				t.Fatalf("code=%d want 413；body=%s", rec.Code, truncateForLog(rec.Body.String()))
			}
			if !strings.Contains(rec.Body.String(), c.wantCode) {
				t.Errorf("body 应含错误码 %q，实际=%s", c.wantCode, truncateForLog(rec.Body.String()))
			}
		})
	}
}

// truncateForLog 截断过长的响应体用于失败信息（避免刷屏）。
func truncateForLog(s string) string {
	if len(s) > 300 {
		return s[:300] + "..."
	}
	return s
}
