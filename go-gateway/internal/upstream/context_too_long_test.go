package upstream

import (
	"net/http"
	"testing"
)

// ---------------------------------------------------------------------------
// 上下文超长分类回归（2026-09-15 现场）
//
// 现场表现：上游 400 code=11115（prompt 1121509 > 上限 1048576）被网关归入
// 通用 ErrClient → 落入「换号重试」→ 整个请求体对着每个账号重传（实测 3 次）
// → 最终包装成 503 no_healthy_account，把「请求太大」伪装成「账号全挂」。
//
// 修复：新增 ErrContextTooLong，让调用方立即失败而非轮转。
// ---------------------------------------------------------------------------

// realContextTooLongBody 现场真实响应体（逐字复制，勿改）。
const realContextTooLongBody = `{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum",` +
	`"requestId":"8a6bc76c-690c-457b-8702-0065ba4cb1e5",` +
	`"extError":{"code":"context_length_exceeded","message":"prompt is too long: 1119655 tokens > 1048576 maximum",` +
	`"param":"","type":"invalid_request_error","StatusCode":400,"Request":null,"Response":null},` +
	`"displayMsg":{"en":"The request exceeds the model context limit. Please shorten the conversation or remove attachments.",` +
	`"zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}`

// realContextTooLongBodyCN 国服实测响应体（逐字复制，勿改）。
//
// 与上面国际版样本的**关键差异**：msg 是 "input length too long" 而非
// "prompt is too long"，且 extError.code 是 "400001" 而非 "context_length_exceeded"
// —— 即两个兜底信号都不成立，只剩业务码 11115。
//
// 该样本来自 2026-09-15 端到端实测（glm-5.3，4.58MB 请求体）。
// 它证明：文案兜底必须覆盖两种措辞，否则一旦上游改码就会漏判。
const realContextTooLongBodyCN = `{"code":11115,"msg":"input length too long",` +
	`"requestId":"fe049491-ba6f-43f6-8f7e-306db4d974f4",` +
	`"extError":{"code":"400001","message":"input length too long","param":"",` +
	`"type":"invalid_request_error","StatusCode":400,"Request":null,"Response":null}}`

// TestClassifyRealContextTooLong 现场样本必须判为 ErrContextTooLong，而非 ErrClient。
func TestClassifyRealContextTooLong(t *testing.T) {
	if got := Classify(http.StatusBadRequest, realContextTooLongBody); got != ErrContextTooLong {
		t.Fatalf("真实样本 → %v want ErrContextTooLong", got)
	}
}

// realContextTooLongBodyHy3 hy3 实测响应体（逐字复制自 #27 用户现场截图，勿改）。
//
// 关键差异：业务码是 **4028** 而非 11115，且不含 extError.code —— 与 11115 样本
// 相比，另外两路信号都不成立。此前能判对纯粹靠 msg 里的 "prompt is too long"
// 文案兜底；一旦上游换成别的措辞就会漏判，故本样本专门盯住 4028 这条路。
//
// 另一个值得记住的数字：只超了 1 个 token（100001 > 100000）。
// 这说明窗口边界是硬限制，不存在「超一点点会放行」的余地。
const realContextTooLongBodyHy3 = `{"code":4028,"msg":"prompt is too long: 100001 tokens > 100000 maximum"}`

// TestClassifyRealContextTooLongHy3 hy3 现场的 4028 必须独立判为 ErrContextTooLong。
//
// 去掉 msg 后仍然要成立 —— 这条才是「业务码已正式登记」的真正证明。
func TestClassifyRealContextTooLongHy3(t *testing.T) {
	if got := Classify(http.StatusBadRequest, realContextTooLongBodyHy3); got != ErrContextTooLong {
		t.Fatalf("hy3 现场样本 → %v want ErrContextTooLong", got)
	}
	// 剥掉文案，只剩业务码：这是上游改文案时唯一还站得住的信号。
	bare := `{"code":4028,"msg":"upstream reworded this"}`
	if got := Classify(http.StatusBadRequest, bare); got != ErrContextTooLong {
		t.Errorf("仅凭 4028 业务码 → %v want ErrContextTooLong（4028 未被登记？）", got)
	}
}

// TestClassifyRealContextTooLongCN 国服样本同样要判为 ErrContextTooLong。
func TestClassifyRealContextTooLongCN(t *testing.T) {
	if got := Classify(http.StatusBadRequest, realContextTooLongBodyCN); got != ErrContextTooLong {
		t.Fatalf("国服真实样本 → %v want ErrContextTooLong", got)
	}
}

// TestContextTooLongMarkerCoversBothWording 两种真实措辞都要被文案兜底覆盖。
//
// 去掉业务码后仍应识别 —— 这模拟「上游保留文案但改了码」的场景。
func TestContextTooLongMarkerCoversBothWording(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{"国际版措辞", `{"code":9,"msg":"prompt is too long: 999 tokens > 10 maximum"}`},
		{"国服措辞", `{"code":9,"msg":"input length too long"}`},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := Classify(http.StatusBadRequest, c.body); got != ErrContextTooLong {
				t.Errorf("%s（无业务码）→ %v want ErrContextTooLong", c.name, got)
			}
		})
	}
}

// TestClassifyContextTooLongVariants 三路判定各自独立成立：
// 业务码、extError.code、以及中英文案兜底（上游改码不改文案时仍要识别）。
func TestClassifyContextTooLongVariants(t *testing.T) {
	cases := []struct {
		name string
		body string
	}{
		{"仅业务码 11115", `{"code":11115,"msg":"boom"}`},
		{"仅业务码 4028", `{"code":4028,"msg":"boom"}`},
		{"仅 extError.code", `{"code":0,"extError":{"code":"context_length_exceeded"}}`},
		{"仅英文文案", `{"code":9,"msg":"prompt is too long: 999 tokens > 10 maximum"}`},
		{"仅英文 displayMsg", `{"displayMsg":{"en":"The request exceeds the model context limit."}}`},
		{"仅中文文案", `{"code":9,"msg":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}`},
		{"仅中文短句", `{"msg":"超出模型长度上限"}`},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := Classify(http.StatusBadRequest, c.body); got != ErrContextTooLong {
				t.Errorf("%s → %v want ErrContextTooLong", c.name, got)
			}
		})
	}
}

// TestClassifyDoesNotOverreach 防过度归类：相邻错误不得被误判成上下文超长。
//
// 这些是最容易混淆的邻居 —— 尤其 429 模型限流的文案里也有「模型」字样。
func TestClassifyDoesNotOverreach(t *testing.T) {
	cases := []struct {
		name string
		code int
		body string
		want ErrKind
	}{
		{"429 模型级限流", http.StatusTooManyRequests,
			`{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`, ErrModelRate},
		{"429 账号级限流", http.StatusTooManyRequests, `{"code":1001,"msg":"too many requests"}`, ErrSoftRate},
		{"402 余额不足", http.StatusPaymentRequired, `{"code":1,"msg":"余额不足"}`, ErrHardCredit},
		{"401 session 失效", http.StatusUnauthorized, `{"code":12153,"msg":"Offline user session not found"}`, ErrSessionDead},
		{"400 模型不存在", http.StatusBadRequest, `{"code":11102,"msg":"model service info not found"}`, ErrClient},
		{"400 渠道未批准", http.StatusBadRequest, `{"code":11128,"msg":"channel not approved"}`, ErrClient},
		{"500 上游故障", http.StatusInternalServerError, `{"code":500}`, ErrServer},
		{"404 偶发", http.StatusNotFound, `{"code":404}`, ErrNotFound},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := Classify(c.code, c.body); got != c.want {
				t.Errorf("%s → %v want %v", c.name, got, c.want)
			}
		})
	}
}

// TestErrKindStringContextTooLong 类别名要可读（日志与前端都直接用它）。
func TestErrKindStringContextTooLong(t *testing.T) {
	if got := ErrContextTooLong.String(); got != "context_too_long" {
		t.Errorf("String()=%q want context_too_long", got)
	}
}

// TestContextTooLongMessageKeepsUpstreamWording 消息必须保留上游原文。
//
// 关键约束：下游客户端（DeepSeek Harness）靠文案模式识别上下文溢出并触发
// 自动压缩重试。若只回我们自己的措辞，客户端认不出这是溢出，只会当普通失败
// —— 那正是本次会话死锁难以自愈的原因之一。
func TestContextTooLongMessageKeepsUpstreamWording(t *testing.T) {
	msg := ContextTooLongMessage(realContextTooLongBody)

	// 必须含上游原始 token 数（人类定位问题的最直接线索）。
	if !contains(msg, "1119655") {
		t.Errorf("应保留上游 token 数 1119655，实际=%q", msg)
	}
	// 必须含客户端识别溢出所用的特征串之一。
	identifiable := contains(msg, "prompt is too long") ||
		contains(msg, "context_length_exceeded") ||
		contains(msg, "exceeds the model context limit")
	if !identifiable {
		t.Errorf("应含客户端可识别的溢出特征串，实际=%q", msg)
	}
	// 中文提示应同时给出（面向人类）。
	if !contains(msg, "超出模型长度上限") {
		t.Errorf("应含中文提示，实际=%q", msg)
	}
}

// TestContextTooLongMessageFallbacks 上游字段缺失时逐级回退，绝不返回空串。
func TestContextTooLongMessageFallbacks(t *testing.T) {
	cases := []struct {
		name string
		body string
		want string
	}{
		{"仅 msg", `{"msg":"prompt is too long"}`, "prompt is too long"},
		{"仅 extError.message", `{"extError":{"message":"too long"}}`, "too long"},
		{"仅中文 displayMsg", `{"displayMsg":{"zh":"太长了"}}`, "太长了"},
		{"仅英文 displayMsg", `{"displayMsg":{"en":"too long"}}`, "too long"},
		{"非 JSON 回退原文", `not json at all`, "not json at all"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := ContextTooLongMessage(c.body); got != c.want {
				t.Errorf("got %q want %q", got, c.want)
			}
		})
	}
}

// TestContextTooLongMessageNoDuplicateHint 中文提示与主消息重复时不拼接。
func TestContextTooLongMessageNoDuplicateHint(t *testing.T) {
	body := `{"msg":"对话内容超出模型长度上限","displayMsg":{"zh":"对话内容超出模型长度上限"}}`
	got := ContextTooLongMessage(body)
	if got != "对话内容超出模型长度上限" {
		t.Errorf("重复提示不应拼接，got=%q", got)
	}
}

func contains(haystack, needle string) bool {
	return len(haystack) >= len(needle) && indexOf(haystack, needle) >= 0
}

func indexOf(haystack, needle string) int {
	for i := 0; i+len(needle) <= len(haystack); i++ {
		if haystack[i:i+len(needle)] == needle {
			return i
		}
	}
	return -1
}
