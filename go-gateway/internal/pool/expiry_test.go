package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// expiryAt 返回 daysFromNow 天后的当日 23:59:59（本地时区）对应的 Unix 秒。
// 用「日」粒度构造，与 expiryDayKey 的分层口径一致。
func expiryAt(daysFromNow int) int64 {
	d := time.Now().In(time.Local).AddDate(0, 0, daysFromNow)
	return time.Date(d.Year(), d.Month(), d.Day(), 23, 59, 59, 0, time.Local).Unix()
}

// addWithExpiry 往池里加一个带到期日的账号。
func addWithExpiry(p *Pool, uid string, credits int64, daysFromNow int) {
	a := &auth.Auth{UID: uid, SoonestExpireAt: expiryAt(daysFromNow)}
	p.Add(a)
	p.SetCredits(uid, credits)
}

// TestPickPrefersSoonestExpiry 用户场景：A 的积分 25 日到期、B 的 29 日到期
// → 只打 A，因为 A 的额度先失效，浪费不起。
func TestPickPrefersSoonestExpiry(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	addWithExpiry(p, "A", 1000, 3)   // 3 天后到期
	addWithExpiry(p, "B", 1000, 7)   // 7 天后到期
	addWithExpiry(p, "C", 99999, 30) // 积分最多但最晚到期

	for i := 0; i < 200; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatal("pick returned nil")
		}
		if got.UID != "A" {
			t.Fatalf("iter %d: picked %s, want A (soonest expiry must win over higher credits)", i, got.UID)
		}
	}
}

// TestPickFallsToNextTierWhenSoonestExhausted 最早到期的一档用完后，
// 自动轮到下一档，而不是继续空转。
func TestPickFallsToNextTierWhenSoonestExhausted(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	addWithExpiry(p, "A", 1000, 3)
	addWithExpiry(p, "B", 1000, 7)

	if got := p.Pick(); got == nil || got.UID != "A" {
		t.Fatalf("pick=%v want A", got)
	}
	// 模拟 A 的额度被烧完/账号进入冷却：两种方式都应让 B 接管。
	p.Cooldown("A", CoolHard, time.Hour, "余额不足")
	if got := p.Pick(); got == nil || got.UID != "B" {
		t.Fatalf("pick=%v want B after A cooled down", got)
	}
}

// TestPickSameTierSplitsEvenly 同一天到期的账号之间平均分摊（不按积分多少倾斜）。
// 这正是「到了 29 日，大家最近到期都是 29 日 → 平均使用」的行为。
func TestPickSameTierSplitsEvenly(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	// 同一天到期，但积分相差 100 倍：仍应大致各半。
	addWithExpiry(p, "A", 10000, 5)
	addWithExpiry(p, "B", 100, 5)

	const N = 2000
	counts := map[string]int{}
	for i := 0; i < N; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatal("pick returned nil")
		}
		counts[got.UID]++
	}
	// 严格平均的判据：较少的一方不应低于 35%（留出随机波动与闲置补偿的余量）。
	// 若仍按 credits 加权，B 的占比会掉到 1% 量级，此断言会立刻失败。
	for _, uid := range []string{"A", "B"} {
		if counts[uid] < N*35/100 {
			t.Errorf("uid %s picked %d/%d (<35%%): same-tier must split evenly, got %v", uid, counts[uid], N, counts)
		}
	}
}

// TestPickUnknownExpiryGoesLast 无到期信息的账号排到最后：
// 只要还有别的账号可用，就不该被选中。
func TestPickUnknownExpiryGoesLast(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	addWithExpiry(p, "known", 1, 10)  // 积分极少，但有明确到期日
	p.Add(&auth.Auth{UID: "unknown"}) // 无到期信息
	p.SetCredits("unknown", 999999)   // 积分极多

	for i := 0; i < 100; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatal("pick returned nil")
		}
		if got.UID != "known" {
			t.Fatalf("iter %d: picked %s, want known (unknown expiry must rank last)", i, got.UID)
		}
	}
}

// TestPickAllUnknownFallsBackToCredits 全员都无到期信息时，
// 退回原三因子口径（积分高的更受青睐），不因新逻辑把老行为改坏。
func TestPickAllUnknownFallsBackToCredits(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	p.Add(&auth.Auth{UID: "hi"})
	p.Add(&auth.Auth{UID: "lo"})
	p.SetCredits("hi", 10000)
	p.SetCredits("lo", 10)

	const N = 1000
	hi := 0
	for i := 0; i < N; i++ {
		if got := p.Pick(); got != nil && got.UID == "hi" {
			hi++
		}
	}
	if hi < N*80/100 {
		t.Errorf("hi picked %d/%d, want >=80%% (fallback to three-factor weights)", hi, N)
	}
}

// TestEarliestExpiryTierLocked 分档纯函数的行为（不依赖随机）。
func TestEarliestExpiryTierLocked(t *testing.T) {
	p := New("")
	addWithExpiry(p, "d3", 1, 3)
	addWithExpiry(p, "d7a", 1, 7)
	addWithExpiry(p, "d7b", 1, 7)
	p.Add(&auth.Auth{UID: "none"})

	tier, tiered := p.earliestExpiryTierLocked([]*entry{
		p.byUID["d3"], p.byUID["d7a"], p.byUID["d7b"], p.byUID["none"],
	})
	if !tiered {
		t.Fatal("tiered=false, want true when some account has expiry")
	}
	if len(tier) != 1 || tier[0].a.UID != "d3" {
		t.Fatalf("tier=%v want only d3", uidsOf(tier))
	}

	// 全部无到期信息 → 不分档，调用方回退原口径。
	none := []*entry{p.byUID["none"]}
	if _, tiered := p.earliestExpiryTierLocked(none); tiered {
		t.Error("tiered=true for all-unknown candidates, want false")
	}
}

// TestTierWeightIgnoresCredits 组内权重不含 credits 项。
func TestTierWeightIgnoresCredits(t *testing.T) {
	now := time.Now()
	p := New("")
	rich := &entry{a: &auth.Auth{UID: "rich"}, credits: 100000, successCount: 1}
	poor := &entry{a: &auth.Auth{UID: "poor"}, credits: 1, successCount: 1}

	wr := p.tierWeightOf(rich, now)
	wp := p.tierWeightOf(poor, now)
	if wr != wp {
		t.Errorf("tierWeightOf rich=%v poor=%v, want equal (credits must not affect same-tier weight)", wr, wp)
	}
}

// TestSetExpiryAndPersist 到期日会持久化并在重载后恢复（重启即生效，无需等巡检）。
func TestSetExpiryAndPersist(t *testing.T) {
	dir := t.TempDir()
	fp := dir + "/state.json"
	p := New(fp)
	p.Add(&auth.Auth{UID: "u1"})
	exp := expiryAt(4)
	p.SetCreditsAndExpiry("u1", 500, exp)

	if got := p.List()[0].SoonestExpireAt; got != exp {
		t.Fatalf("SoonestExpireAt=%d want %d", got, exp)
	}
	if got := p.List()[0].ExpireDay; got == "" {
		t.Fatal("ExpireDay empty, want a YYYY-MM-DD key")
	}
	p.Flush()

	// 重载：仅从 state.json（无凭证元数据）恢复。
	p2 := New(fp)
	p2.Add(&auth.Auth{UID: "u1"})
	st := p2.List()[0]
	if st.SoonestExpireAt != exp {
		t.Errorf("after reload SoonestExpireAt=%d want %d", st.SoonestExpireAt, exp)
	}
}

// TestUpsertAppliesCredentialExpiry 凭证文件里的 credit 元数据能进入池，
// 且后续凭证能把到期日推进到新值；缺省值不会把已知到期日抹成未知。
func TestUpsertAppliesCredentialExpiry(t *testing.T) {
	p := New("")
	exp1 := expiryAt(2)
	p.Add(&auth.Auth{UID: "u1", SoonestExpireAt: exp1})
	if got := p.List()[0].SoonestExpireAt; got != exp1 {
		t.Fatalf("SoonestExpireAt=%d want %d", got, exp1)
	}

	// 巡检把到期日推进到更晚的一档（该账号快过期的额度已烧完）。
	exp2 := expiryAt(9)
	p.SetExpiry("u1", exp2)
	if got := p.List()[0].SoonestExpireAt; got != exp2 {
		t.Fatalf("after SetExpiry SoonestExpireAt=%d want %d", got, exp2)
	}

	// 宿主随后同步了更新的凭证：应能再次推进。
	exp3 := expiryAt(15)
	p.Add(&auth.Auth{UID: "u1", SoonestExpireAt: exp3})
	if got := p.List()[0].SoonestExpireAt; got != exp3 {
		t.Fatalf("credential expiry not applied: got %d want %d", got, exp3)
	}

	// 凭证缺少到期信息（0）时不应把已知值抹成未知。
	p.Add(&auth.Auth{UID: "u1"})
	if got := p.List()[0].SoonestExpireAt; got != exp3 {
		t.Fatalf("empty credential expiry clobbered known value: got %d want %d", got, exp3)
	}
}

func uidsOf(es []*entry) []string {
	out := make([]string, 0, len(es))
	for _, e := range es {
		out = append(out, e.a.UID)
	}
	return out
}
