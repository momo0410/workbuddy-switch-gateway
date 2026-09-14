package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// Issue #5 回归：到期同档内不得把候选截断成固定前 5 名
//
// 现场（2026-09-14）：14 个账号里有 8 个的「最近到期积分」落在同一天（2026-10-11），
// 另 6 个分属更早/更晚的档位或处于硬冷却。旧实现在分层后仍按 (权重, uid) 排序截断
// 前 5 名，于是同档那 8 个账号里只有 uid 靠前的 5 个拿得到流量：
//
//	1f3c55e5 / 6a173999 / c644e54a / d1671c77 / 7ffd1102 各约 20%，
//	e2891116 / e94c5d4f / fd6b410d 各约 1.3%
//
// 用户可见表现即 Issue 标题：「十四个账号，只负载均衡使用到 4-5 个账号」。
// ---------------------------------------------------------------------------

// issue5SameTierPool 构造一个「单档 8 个账号、权重完全相同」的池：
// 这是问题现场的最小复现 —— 权重打平时 uid 是稳定决胜键，截断必然固定砍掉同一批。
func issue5SameTierPool(t *testing.T) *Pool {
	t.Helper()
	p := New("")
	// uid 刻意保持字典序可预期，便于断言「被砍掉的总是后几名」。
	uids := []string{"acc-a", "acc-b", "acc-c", "acc-d", "acc-e", "acc-f", "acc-g", "acc-h"}
	for _, u := range uids {
		p.Add(&auth.Auth{UID: u, SoonestExpireAt: expiryAt(14)})
		p.SetCredits(u, 2000)
		// 统一成功率，消除权重差异 → 8 个账号权重完全相同。
		for i := 0; i < 40; i++ {
			p.NoteSuccess(u)
		}
	}
	return p
}

// TestSameTierAllAccountsReceiveTraffic 是 Issue #5 的核心回归：
// 同一天到期的账号必须**全部**分到流量，不能被固定截断掉尾部。
func TestSameTierAllAccountsReceiveTraffic(t *testing.T) {
	withNoPickGap(t)
	p := issue5SameTierPool(t)

	const N = 8000
	counts := map[string]int{}
	for i := 0; i < N; i++ {
		got := p.Pick()
		if got == nil {
			t.Fatalf("iter %d: pick returned nil", i)
		}
		counts[got.UID]++
	}

	if len(counts) != 8 {
		t.Fatalf("只有 %d/8 个同档账号被路由到（Issue #5 复发）：%v", len(counts), counts)
	}
	// 平均分摊判据：每个账号都应拿到接近 1/8 的份额。
	// 旧实现下后 3 名的占比掉到 1% 量级，此断言会立刻失败。
	for uid, n := range counts {
		if n < N*8/100 {
			t.Errorf("账号 %s 仅分到 %d/%d (%.1f%%)，同档账号必须平均分摊", uid, n, N, float64(n)*100/N)
		}
	}
}

// TestSameTierNotTruncatedWhenWeightsTie 直接锁定"权重打平不截断"这一机制：
// 8 个权重完全相同的候选，短名单长度必须是 8（而不是 5）。
func TestSameTierNotTruncatedWhenWeightsTie(t *testing.T) {
	withNoPickGap(t)
	p := issue5SameTierPool(t)

	// 预热让 lastUsed 进入稳定态（否则"从未使用"的满分闲置补偿会掩盖问题）。
	for i := 0; i < 500; i++ {
		p.Pick()
	}

	now := time.Now()
	p.mu.RLock()
	var healthy []*entry
	for _, e := range p.byUID {
		if e.healthy(now) {
			healthy = append(healthy, e)
		}
	}
	tier, tiered := p.earliestExpiryTierLocked(healthy)
	p.mu.RUnlock()

	if !tiered {
		t.Fatal("precondition: 同档账号应触发分层")
	}
	if len(tier) != 8 {
		t.Fatalf("同档候选=%d，期望 8（同一天到期的账号都该留在候选里）", len(tier))
	}
}

// TestNonTieredPathStillTruncatesToFive 反向保护：**未分层**的回退路径
// （全员无到期信息 → 原三因子口径）仍然保留前 5 截断，避免高并发下
// 极低积分账号稀释流量。修复只针对分层路径，不能顺手改坏老语义。
func TestNonTieredPathStillTruncatesToFive(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	// 无到期信息 → earliestExpiryTierLocked 返回 tiered=false。
	for _, u := range []string{"a1", "a2", "a3", "a4", "a5", "a6"} {
		p.Add(&auth.Auth{UID: u})
	}
	for _, u := range []string{"a1", "a2", "a3", "a4", "a5"} {
		p.SetCredits(u, 1000)
	}
	p.SetCredits("a6", 1) // 权重最低，未分层时应在 top5 之外

	for i := 0; i < 2000; i++ {
		if got := p.Pick(); got == nil || got.UID == "a6" {
			t.Fatalf("iter %d: 未分层路径应仍截断前 5，a6 不该被选中，got=%+v", i, got)
		}
	}
}
