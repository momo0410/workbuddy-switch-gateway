package server

// claude_alias_test.go 覆盖 Claude 模型名 → 上游真实模型名的翻译。
//
// 这块逻辑的价值在于：Claude Code / Claude Desktop 的 /model 菜单里有大量
// 内置型号名（claude-opus-5、claude-sonnet-4-6 等），而本网关上游是国产模型池。
// 翻译错一个字，请求就会以 11102 失败。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// isolateClaudeAliases 把两处客户端配置目录指向空/临时目录，并清空缓存，
// 避免测试读到我开发机上的真实配置。
func isolateClaudeAliases(t *testing.T, claudeDir string) {
	t.Helper()
	t.Setenv("CLAUDE_CONFIG_DIR", claudeDir)
	t.Setenv("LOCALAPPDATA", t.TempDir())

	claudeAliasCache.Lock()
	claudeAliasCache.tables = nil
	claudeAliasCache.fetched = time.Time{}
	claudeAliasCache.Unlock()
}

// TestResolveClaudeModelPassesThroughUpstreamNames：
// 用户直接指定上游模型名时必须原样透传，不能画蛇添足。
func TestResolveClaudeModelPassesThroughUpstreamNames(t *testing.T) {
	isolateClaudeAliases(t, t.TempDir())

	for _, name := range []string{"deepseek-v4-flash", "glm-5.2", "kimi-k2.7", ""} {
		if got := resolveClaudeModel(name); got != name {
			t.Errorf("resolveClaudeModel(%q) = %q, want unchanged", name, got)
		}
	}
}

// TestResolveClaudeModelReadsConfiguredPairs：
// 虚拟名/真实名成对配置（CC Switch 风格）：精确名命中，内置型号按槽位兜底。
func TestResolveClaudeModelReadsConfiguredPairs(t *testing.T) {
	dir := t.TempDir()
	settings := `{
	  "env": {
	    "ANTHROPIC_BASE_URL": "http://127.0.0.1:7863",
	    "ANTHROPIC_AUTH_TOKEN": "sk-test",
	    "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6",
	    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "deepseek-v4-flash",
	    "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-8",
	    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "glm-5.2",
	    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5",
	    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "kimi-k2.7"
	  }
	}`
	if err := os.WriteFile(filepath.Join(dir, "settings.json"), []byte(settings), 0o644); err != nil {
		t.Fatalf("write settings: %v", err)
	}
	isolateClaudeAliases(t, dir)

	cases := map[string]string{
		"claude-sonnet-4-6": "deepseek-v4-flash",
		"claude-opus-4-8":   "glm-5.2",
		"claude-haiku-4-5":  "kimi-k2.7",
		// 大小写不敏感
		"Claude-Sonnet-4-6": "deepseek-v4-flash",
		// 内置型号不走配置槽位（如 claude-opus-5）→ 按名字中的槽位关键词兜底
		"claude-opus-5":   "glm-5.2",
		"claude-sonnet-5": "deepseek-v4-flash",
	}
	for in, want := range cases {
		if got := resolveClaudeModel(in); got != want {
			t.Errorf("resolveClaudeModel(%q) = %q, want %q", in, got, want)
		}
	}
}

// TestResolveClaudeModelFallbackForDirectModelNames：
// 本应用「一键接入」直写真实模型名的形态：精确表里只有真实名自映射，
// Claude 客户端的内置型号名必须按槽位关键词兜底到对应槽位的模型。
func TestResolveClaudeModelFallbackForDirectModelNames(t *testing.T) {
	dir := t.TempDir()
	settings := `{
	  "env": {
	    "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-flash",
	    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "deepseek-v4-flash",
	    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2",
	    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "glm-5.2",
	    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "kimi-k2.7",
	    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "kimi-k2.7",
	    "ANTHROPIC_DEFAULT_FABLE_MODEL": "minimax-m3",
	    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME": "minimax-m3"
	  }
	}`
	if err := os.WriteFile(filepath.Join(dir, "settings.json"), []byte(settings), 0o644); err != nil {
		t.Fatalf("write settings: %v", err)
	}
	isolateClaudeAliases(t, dir)

	cases := map[string]string{
		"claude-opus-5":             "glm-5.2",
		"claude-sonnet-4-6":         "deepseek-v4-flash",
		"claude-haiku-4-5-20251001": "kimi-k2.7",
		"claude-fable-5":            "minimax-m3",
		// 未收录且无槽位关键词 → 回退主槽位（Sonnet），仍比直接 11102 可用
		"claude-mystery-9": "deepseek-v4-flash",
	}
	for in, want := range cases {
		if got := resolveClaudeModel(in); got != want {
			t.Errorf("resolveClaudeModel(%q) = %q, want %q", in, got, want)
		}
	}
}

// TestResolveClaudeModelUsesDesktopProfileSlots：
// Claude Desktop 的 profile 只有槽位名与 labelOverride，内置型号同样按槽位兜底。
func TestResolveClaudeModelUsesDesktopProfileSlots(t *testing.T) {
	localAppData := t.TempDir()
	libDir := filepath.Join(localAppData, "Claude-3p", "configLibrary")
	if err := os.MkdirAll(libDir, 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	profile := `{
	  "inferenceModels": [
	    {"name": "claude-sonnet-5", "labelOverride": "deepseek-v4-flash", "supports1m": true},
	    {"name": "claude-opus-5", "labelOverride": "glm-5.2", "supports1m": true}
	  ]
	}`
	if err := os.WriteFile(filepath.Join(libDir, "profile.json"), []byte(profile), 0o644); err != nil {
		t.Fatalf("write profile: %v", err)
	}
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	t.Setenv("LOCALAPPDATA", localAppData)
	claudeAliasCache.Lock()
	claudeAliasCache.tables = nil
	claudeAliasCache.fetched = time.Time{}
	claudeAliasCache.Unlock()

	cases := map[string]string{
		"claude-opus-5":     "glm-5.2",
		"claude-sonnet-5":   "deepseek-v4-flash",
		"claude-opus-4-8":   "glm-5.2",
		"claude-sonnet-4-6": "deepseek-v4-flash",
	}
	for in, want := range cases {
		if got := resolveClaudeModel(in); got != want {
			t.Errorf("resolveClaudeModel(%q) = %q, want %q", in, got, want)
		}
	}
}

// TestResolveClaudeModelUnknownClaudeNameIsNotSilentlyReplaced：
// 没有配置可读时，未知的 claude- 名字必须原样放行。
//
// 静默替换成某个「看起来合理」的模型会让用户以为请求成功了，
// 却在消费自己没选过的额度；透传至少能让上游明确报错、可定位。
func TestResolveClaudeModelUnknownClaudeNameIsNotSilentlyReplaced(t *testing.T) {
	isolateClaudeAliases(t, t.TempDir())

	const unknown = "claude-sonnet-9-9-turbo"
	if got := resolveClaudeModel(unknown); got != unknown {
		t.Errorf("resolveClaudeModel(%q) = %q, want unchanged", unknown, got)
	}
}

// TestAnthropicToChatRewritesModelField：端到端确认请求体里的 model 被改写。
func TestAnthropicToChatRewritesModelField(t *testing.T) {
	dir := t.TempDir()
	settings := `{"env":{
	  "ANTHROPIC_DEFAULT_SONNET_MODEL":"claude-sonnet-4-6",
	  "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME":"deepseek-v4-flash"
	}}`
	if err := os.WriteFile(filepath.Join(dir, "settings.json"), []byte(settings), 0o644); err != nil {
		t.Fatalf("write settings: %v", err)
	}
	isolateClaudeAliases(t, dir)

	raw := []byte(`{"model":"claude-sonnet-4-6","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}`)
	body, _, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}

	var out map[string]any
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if out["model"] != "deepseek-v4-flash" {
		t.Errorf("model = %v, want deepseek-v4-flash", out["model"])
	}
}
