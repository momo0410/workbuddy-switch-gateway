package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// Issue #5 回归：熔断期的状态画像必须反映"真正生效的截止"
//
// 现场（2026-09-14 22:5x，/status 实测）：4 个账号被熔断（breaker_until 约 2 小时后），
// 但它们的 until 是零值、cool_kind 是历史残留的 hard_credit/soft_rate。旧实现按
// time.Until(e.until) 算剩余、按 e.coolKind 报类型，于是界面上显示成
//
//	「冷却中」+ 剩余 0 秒 + 余额不足
//
// 而账号实际有 7161 积分、要等的是 2 小时后的熔断退避。用户据此既不知道等多久，
// 也不知道为什么被停用 —— 这正是 Issue 里"频繁不可用/提示冷却"的观感来源。
// ---------------------------------------------------------------------------

// TestStatusBreakerReportsEffectiveDeadline 熔断生效时，剩余秒数按 breakerUntil 算，
// 类型报 "breaker"（而不是残留的 hard_credit）。
func TestStatusBreakerReportsEffectiveDeadline(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	// 先进入 hard 冷却（写入 coolKind=hard_credit），再触发熔断。
	p.Cooldown("u1", CoolHard, time.Hour, "余额不足")
	p.SetBreaker(1, 2*time.Hour, 2*time.Hour)
	p.NoteError("u1") // 阈值 1 → 立刻熔断，breakerUntil = now+2h

	st, ok := p.Status("u1")
	if !ok {
		t.Fatal("no status")
	}
	if !st.Cooling {
		t.Fatal("precondition: 应处于冷却/熔断中")
	}
	if st.BreakerUntil.IsZero() {
		t.Fatal("precondition: 熔断应已打开")
	}

	// 剩余秒数必须反映熔断截止（约 2 小时），而不是 until 的 1 小时、更不是 0。
	if st.CoolRemaining < 7000 || st.CoolRemaining > 7300 {
		t.Errorf("cool_remaining_sec=%d，期望约 2 小时（7200s，按真正生效的熔断截止算）", st.CoolRemaining)
	}
	// 类型必须报 breaker —— 此时主导的是熔断，不是那个已被覆盖的 hard_credit。
	if st.CoolKind != "breaker" {
		t.Errorf("cool_kind=%q，期望 breaker（熔断截止比 until 更晚，主导当前停用）", st.CoolKind)
	}
}

// TestStatusSoftCooldownStillReportsItsOwnKind 反向保护：无熔断时仍报即时冷却的
// 原始类型与时长，修复没有把普通软冷却的口径改坏。
func TestStatusSoftCooldownStillReportsItsOwnKind(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.Cooldown("u1", CoolSoft, time.Hour, "429 rate limit")

	st, _ := p.Status("u1")
	if !st.Cooling {
		t.Fatal("应处于软冷却中")
	}
	if st.CoolKind != "soft_rate" {
		t.Errorf("cool_kind=%q want soft_rate", st.CoolKind)
	}
	if st.CoolRemaining < 3500 || st.CoolRemaining > 3700 {
		t.Errorf("cool_remaining_sec=%d，期望约 1 小时", st.CoolRemaining)
	}
}

// TestStatusBreakerWithoutAnyUntil 仅熔断、从未有过即时冷却的账号：
// 旧实现会报 cool_kind="unknown"（CoolKind 零值）且剩余 0 秒。
func TestStatusBreakerWithoutAnyUntil(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetBreaker(1, 90*time.Minute, 90*time.Minute)
	p.NoteError("u1") // 纯熔断，until 始终为零值

	st, _ := p.Status("u1")
	if !st.Cooling {
		t.Fatal("应处于熔断中")
	}
	if st.CoolKind != "breaker" {
		t.Errorf("cool_kind=%q want breaker（而非 unknown）", st.CoolKind)
	}
	if st.CoolRemaining < 5300 || st.CoolRemaining > 5500 {
		t.Errorf("cool_remaining_sec=%d，期望约 90 分钟", st.CoolRemaining)
	}
}

// TestStatusNotCoolingUnaffected 未冷却时画像保持干净（CoolKind/CoolRemaining 均为空值）。
func TestStatusNotCoolingUnaffected(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	// 制造一次已过期的软冷却：until 在過去，不应再报冷却。
	p.Cooldown("u1", CoolSoft, time.Millisecond, "429")
	time.Sleep(5 * time.Millisecond)
	p.NoteSuccess("u1") // 清熔断运行态

	st, _ := p.Status("u1")
	if st.Cooling {
		t.Errorf("冷却已过期且无熔断，不应报 cooling: %+v", st)
	}
	if st.CoolKind != "" || st.CoolRemaining != 0 {
		t.Errorf("未冷却时 cool_kind/cool_remaining 应为空值: %+v", st)
	}
}
