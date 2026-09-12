// Package scheduler 定时任务：每日签到（09/21点，末尾顺带派猫/领奖）+ token keepalive（22点）。
// 签到成功后重新查余额，余额 > 0 的冷却账号自动解冻。
package scheduler

import (
	"context"
	"errors"
	"log"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// Config 调度器依赖。
//
// 任务开关用「禁用」命名而非「启用」：零值 Config 即两类任务都启用，
// 与引入开关前的行为逐字一致（老调用方/老测试无需改动）。
type Config struct {
	Pool           *pool.Pool
	Upstream       *upstream.Client
	CheckinHours   []int // 默认 [9, 21]
	KeepaliveHours []int // 默认 [22]

	// CheckinDisabled 显式关闭签到排程（对应 config 的 schedule.checkin_enabled=false）。
	// 禁用后不再有任何签到时点，搭签到便车的猫猫旅行也随之停摆。
	CheckinDisabled bool
	// KeepaliveDisabled 显式关闭 token 保活排程（schedule.keepalive_enabled=false）。
	KeepaliveDisabled bool

	// CheckinScope 签到 + 猫猫旅行覆盖的账号区域："cn"（缺省，仅国服）/ "all"。
	//
	// 国际版（workbuddy.ai）的 billing 与 growth 接口暂无真实数据，默认跳过；
	// token 保活不受此限制（两个区域都需要刷新）。
	CheckinScope string
}

// checkinScopeAllows 该账号是否参与签到与猫猫旅行。
func (s *Scheduler) checkinScopeAllows(a *auth.Auth) bool {
	if strings.EqualFold(strings.TrimSpace(s.cfg.CheckinScope), "all") {
		return true
	}
	return !upstream.IsIntl(a)
}

// Scheduler 调度器。
type Scheduler struct {
	cfg Config

	// mu/adoptTried 领养当日失败记录：uid → 自然日（CST）。门槛未达的账号当日不再重试，
	// 避免同日多趟对上游重试轰炸；进程重启即清零（无需持久化）。
	mu         sync.Mutex
	adoptTried map[string]string
}

// New 构建。
func New(cfg Config) *Scheduler {
	if len(cfg.CheckinHours) == 0 {
		cfg.CheckinHours = []int{9, 21}
	}
	if len(cfg.KeepaliveHours) == 0 {
		cfg.KeepaliveHours = []int{22}
	}
	return &Scheduler{cfg: cfg, adoptTried: make(map[string]string)}
}

// nextFire 返回 now 之后最近的一个整点触发时间；hours 为本地小时（0-23）。
func nextFire(now time.Time, hours []int) time.Time {
	var earliest time.Time
	for _, h := range hours {
		t := time.Date(now.Year(), now.Month(), now.Day(), h, 0, 0, 0, now.Location())
		if !t.After(now) {
			t = t.Add(24 * time.Hour)
		}
		if earliest.IsZero() || t.Before(earliest) {
			earliest = t
		}
	}
	return earliest
}

// taskKind 调度任务类型。
type taskKind int

const (
	taskCheckin taskKind = iota
	taskKeepalive
)

// nextWake 返回 now 之后最近的唤醒时刻，以及该时刻需要执行的全部任务。
// 签到与保活若配到同一小时（如都含 22），该时刻两类任务需一并执行。
// 已显式禁用的任务不进候选（nextFire 对其零值返回零时间，nextWake 再跳过零时点）。
func (s *Scheduler) nextWake(now time.Time) (time.Time, []taskKind) {
	type slot struct {
		at   time.Time
		kind taskKind
	}
	var slots []slot
	if !s.cfg.CheckinDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.CheckinHours), taskCheckin})
	}
	if !s.cfg.KeepaliveDisabled {
		slots = append(slots, slot{nextFire(now, s.cfg.KeepaliveHours), taskKeepalive})
	}
	var earliest time.Time
	for _, sl := range slots {
		if sl.at.IsZero() {
			continue
		}
		if earliest.IsZero() || sl.at.Before(earliest) {
			earliest = sl.at
		}
	}
	if earliest.IsZero() {
		return time.Time{}, nil
	}
	var kinds []taskKind
	for _, sl := range slots {
		if !sl.at.IsZero() && sl.at.Equal(earliest) {
			kinds = append(kinds, sl.kind)
		}
	}
	return earliest, kinds
}

// Run 主循环，阻塞直到 ctx 取消。
func (s *Scheduler) Run(ctx context.Context) {
	for {
		next, kinds := s.nextWake(time.Now())
		if next.IsZero() {
			// 两类任务全部禁用：不空转，只等退出信号。
			<-ctx.Done()
			return
		}
		timer := time.NewTimer(time.Until(next))
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
			// 到点任务在排程时确定（不依赖唤醒时刻的小时数），迟到唤醒也不会漏跑。
			for _, k := range kinds {
				switch k {
				case taskCheckin:
					s.RunCheckinNow()
				case taskKeepalive:
					s.RunKeepaliveNow()
				}
			}
		}
	}
}

// DefaultCreditRefreshInterval 积分到期巡检的默认周期。
//
// 为什么需要独立于签到的高频巡检：到期日决定选号优先级，而它会随消费变化
// （快过期的额度烧完后，该账号的最近到期日跳到下一档，应立刻让出流量）。
// 签到每天只跑两次，间隔太久会让分层选号长期依据过时数据。
// 15 分钟 × 账号数 的请求量相对上游可忽略，且巡检本身不签到、不改账号状态。
const DefaultCreditRefreshInterval = 15 * time.Minute

// creditRefreshGap 巡检账号之间的间隔，避免瞬间并发打满上游。
const creditRefreshGap = 300 * time.Millisecond

// RunCreditRefreshLoop 周期性刷新所有账号的积分余额与到期日，阻塞直到 ctx 取消。
//
// interval <= 0 时用 DefaultCreditRefreshInterval。
func (s *Scheduler) RunCreditRefreshLoop(ctx context.Context, interval time.Duration) {
	if interval <= 0 {
		interval = DefaultCreditRefreshInterval
	}
	// 启动先跑一轮：否则重启后要等一个周期才拿到到期日，
	// 这段时间内分层选号只能依赖 state.json 里持久化的旧值。
	s.refreshCreditsWithGap(ctx)
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			s.refreshCreditsWithGap(ctx)
		}
	}
}

// refreshCreditsWithGap 跑一轮积分巡检，账号之间留出间隔；ctx 取消时提前退出。
func (s *Scheduler) refreshCreditsWithGap(ctx context.Context) {
	for i, st := range s.cfg.Pool.List() {
		if ctx.Err() != nil {
			return
		}
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		if i > 0 {
			select {
			case <-ctx.Done():
				return
			case <-time.After(creditRefreshGap):
			}
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("credit-refresh %s: %v", st.UID, err)
			continue
		}
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		if info.Remain > 0 {
			// 余额恢复的账号顺带复活，避免硬冷却的号空等到下一个签到时点。
			s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
		}
	}
}

// RunCheckinNow 立即对所有账号执行签到 + 余额刷新 + 解冻，末尾顺带跑一趟猫猫旅行。
// 冷却中的账号也参与（签到就是为了解冻它们）；禁用的跳过。
// 区域范围（cfg.CheckinScope，默认仅国服）之外的账号跳过签到与旅行。
//
// 旅行搭签到便车而非独立排程：每日上限按「派出」计 1 次/天且在派出时锁定奖励，
// 晚领不丢分，故分钟粒度巡检无增益，与签到时点（09/21 点）合并执行即可。
// 注意顺序：先签到解冻，旅行才能覆盖到本轮刚恢复的账号。
func (s *Scheduler) RunCheckinNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		if !s.checkinScopeAllows(a) {
			continue
		}
		if err := s.cfg.Upstream.DailyCheckin(a); err != nil {
			log.Printf("checkin %s: %v", st.UID, err)
			// 已签到等业务错误也继续走余额查询
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("user-resource %s: %v", st.UID, err)
			continue
		}
		// 一次请求同时取回余额与「最近到期」：到期日驱动账号池的分层选号，
		// 顺带回写凭证文件，让宿主（workbuddy-switch）也能看到最新到期信息。
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
	}
	// 签到收尾（09/21 点）：顺带推进一趟旅行状态机（领养 / 派出 / 领奖）。
	s.RunTravelNow()
}

// RunCreditRefreshNow 立即刷新所有账号的积分余额与到期日（不签到、不解冻）。
//
// 与签到的分工：签到是「每天两次」的重操作（含旅行），而到期日会随消费实时变化，
// 需要更高频地刷新才能让分层选号跟上（某账号把快过期额度烧完后，
// 它的最近到期日会跳到下一档，此时就应让出流量给更紧迫的账号）。
func (s *Scheduler) RunCreditRefreshNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		info, err := s.cfg.Upstream.UserResourceDetail(a)
		if err != nil {
			log.Printf("credit-refresh %s: %v", st.UID, err)
			continue
		}
		s.cfg.Pool.SetCreditsAndExpiry(st.UID, info.Remain, info.SoonestExpireAt)
		// 余额耗尽时不在此解冻（那是签到的职责）；但余额恢复的账号顺带复活，
		// 避免硬冷却的号要等到下一个签到时点才回到池中。
		if info.Remain > 0 {
			s.cfg.Pool.ReenableIfCredits(st.UID, info.Remain)
		}
	}
}

// RunKeepaliveNow 立即对所有账号刷新 token；session 死亡的自动禁用。
func (s *Scheduler) RunKeepaliveNow() {
	for _, st := range s.cfg.Pool.List() {
		if st.Disabled {
			continue
		}
		a := s.cfg.Pool.AuthByUID(st.UID)
		if a == nil || a.RefreshToken == "" {
			continue
		}
		if err := s.cfg.Upstream.RefreshToken(a); err != nil {
			log.Printf("keepalive %s: %v", st.UID, err)
			var ue *upstream.Error
			if errors.As(err, &ue) && ue.Kind == upstream.ErrSessionDead {
				s.cfg.Pool.Disable(st.UID, "12153 session dead")
			}
			continue
		}
		if err := a.SaveAtomic(); err != nil {
			log.Printf("keepalive %s save: %v", st.UID, err)
		}
	}
}
