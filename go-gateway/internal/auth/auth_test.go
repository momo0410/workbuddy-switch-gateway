package auth

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseNested(t *testing.T) {
	raw := []byte(`{"auth":{"accessToken":"at","refreshToken":"rt","expiresAt":1753600000,"domain":""},"account":{"uid":"u1","enterpriseId":"e1","nickname":"n1"}}`)
	sa, err := Parse(raw)
	if err != nil {
		t.Fatalf("nested parse err: %v", err)
	}
	if sa.AccessToken != "at" || sa.RefreshToken != "rt" || sa.ExpiresAt != 1753600000 {
		t.Errorf("tokens: %+v", sa)
	}
	if sa.UID != "u1" || sa.EnterpriseID != "e1" || sa.Nickname != "n1" {
		t.Errorf("account: %+v", sa)
	}
}

func TestParseFlat(t *testing.T) {
	raw := []byte(`{"accessToken":"at","refreshToken":"rt","expiresAt":1753600000,"uid":"u2","nickname":"n2"}`)
	sa, err := Parse(raw)
	if err != nil || sa.UID != "u2" || sa.AccessToken != "at" {
		t.Fatalf("flat: %+v %v", sa, err)
	}
}

func TestParseMissingToken(t *testing.T) {
	if _, err := Parse([]byte(`{"uid":"u3"}`)); err == nil {
		t.Fatal("want error for missing accessToken")
	}
}

func TestSaveAtomicRoundtrip(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "workbuddy-u1.json")
	a := &Auth{AccessToken: "at", RefreshToken: "rt", ExpiresAt: 1753600000,
		UID: "u1", EnterpriseID: "e1", Nickname: "n1", FilePath: fp}
	if err := a.SaveAtomic(); err != nil {
		t.Fatalf("save: %v", err)
	}
	if _, err := os.Stat(fp + ".tmp"); !os.IsNotExist(err) {
		t.Error("tmp file should not remain")
	}
	raw, err := os.ReadFile(fp)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	b, err := Parse(raw)
	if err != nil {
		t.Fatalf("reparse: %v", err)
	}
	if b.AccessToken != "at" || b.UID != "u1" || b.EnterpriseID != "e1" {
		t.Errorf("roundtrip: %+v", b)
	}
}

// TestParseCreditExpiry 凭证里的 credit 块（宿主写入）被解析为 SoonestExpireAt，
// 并兼容秒/毫秒两种精度；缺失时为零值（= 未知），不报错。
func TestParseCreditExpiry(t *testing.T) {
	cases := []struct {
		name string
		raw  string
		want int64
	}{
		{
			name: "nested milliseconds",
			raw:  `{"auth":{"accessToken":"at"},"account":{"uid":"u1"},"credit":{"soonestExpireAt":1790639067000}}`,
			want: 1790639067, // 毫秒 → 秒
		},
		{
			name: "nested seconds",
			raw:  `{"auth":{"accessToken":"at"},"account":{"uid":"u1"},"credit":{"soonestExpireAt":1790639067}}`,
			want: 1790639067,
		},
		{
			name: "flat",
			raw:  `{"accessToken":"at","uid":"u1","soonestExpireAt":1790639067000}`,
			want: 1790639067,
		},
		{
			name: "missing is unknown",
			raw:  `{"auth":{"accessToken":"at"},"account":{"uid":"u1"}}`,
			want: 0,
		},
		{
			name: "zero is unknown",
			raw:  `{"auth":{"accessToken":"at"},"account":{"uid":"u1"},"credit":{"soonestExpireAt":0}}`,
			want: 0,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			a, err := Parse([]byte(c.raw))
			if err != nil {
				t.Fatalf("parse: %v", err)
			}
			if a.SoonestExpireAt != c.want {
				t.Errorf("SoonestExpireAt=%d want %d", a.SoonestExpireAt, c.want)
			}
		})
	}
}

// TestSaveAtomicPreservesCreditExpiry token 刷新会重写整个凭证文件，
// 必须保留 credit 元数据 —— 否则一次保活就会抹掉选号依据，
// 表现为「到期分层均衡静默退化成原来的三因子随机」。
func TestSaveAtomicPreservesCreditExpiry(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "workbuddy-u1.json")
	a := &Auth{
		AccessToken: "at", RefreshToken: "rt", ExpiresAt: 1753600000,
		UID: "u1", FilePath: fp, SoonestExpireAt: 1790639067,
	}
	if err := a.SaveAtomic(); err != nil {
		t.Fatalf("save: %v", err)
	}
	raw, err := os.ReadFile(fp)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	b, err := Parse(raw)
	if err != nil {
		t.Fatalf("reparse: %v", err)
	}
	if b.SoonestExpireAt != 1790639067 {
		t.Errorf("credit expiry lost on save: got %d want 1790639067", b.SoonestExpireAt)
	}

	// 未知到期日（0）时不应写出空的 credit 块，保持文件精简。
	a2 := &Auth{AccessToken: "at", UID: "u2", FilePath: filepath.Join(dir, "workbuddy-u2.json")}
	if err := a2.SaveAtomic(); err != nil {
		t.Fatalf("save u2: %v", err)
	}
	raw2, _ := os.ReadFile(a2.FilePath)
	if strings.Contains(string(raw2), "credit") {
		t.Errorf("unknown expiry should not emit a credit block: %s", raw2)
	}
}

// TestLoadDirLoadsAllValid 不再按 region 过滤：所有可解析的 auth 文件都被加载，
// 解析失败的文件静默跳过。
func TestLoadDirLoadsAllValid(t *testing.T) {
	dir := t.TempDir()
	cn := `{"auth":{"accessToken":"at1","refreshToken":"r","expiresAt":1,"domain":""},"account":{"uid":"cn1"}}`
	other := `{"auth":{"accessToken":"at2","refreshToken":"r","expiresAt":1,"domain":"example.com"},"account":{"uid":"u2"}}`
	bad := `not json`
	os.WriteFile(filepath.Join(dir, "workbuddy-cn1.json"), []byte(cn), 0o600)
	os.WriteFile(filepath.Join(dir, "workbuddy-u2.json"), []byte(other), 0o600)
	os.WriteFile(filepath.Join(dir, "workbuddy-bad.json"), []byte(bad), 0o600)

	list, err := LoadDir(dir)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if len(list) != 2 {
		t.Fatalf("want 2 valid accounts, got %+v", list)
	}
	for _, a := range list {
		if a.FilePath == "" {
			t.Error("FilePath not set")
		}
	}
}

func TestNeedsRefresh(t *testing.T) {
	a := &Auth{ExpiresAt: 0}
	if !a.NeedsRefresh(0) {
		t.Error("zero expiry should need refresh")
	}
	a.ExpiresAt = 9999999999
	if a.NeedsRefresh(0) {
		t.Error("far future should not need refresh")
	}
}
